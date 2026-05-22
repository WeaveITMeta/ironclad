//! Fjall-backed secret storage (replaces the PostgreSQL implementation).
//!
//! Two partitions in one keyspace:
//! - `secrets` : `user \x1f name`  -> JSON(Secret)
//! - `id_idx`  : id (16 bytes)      -> `user \x1f name`   (for record_usage by id)
//!
//! Encryption/decryption stays in [`SecretsCrypto`]; only ciphertext is stored.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use secrecy::ExposeSecret;
use uuid::Uuid;

use crate::secrets::crypto::SecretsCrypto;
use crate::secrets::store::SecretsStore;
use crate::secrets::types::{CreateSecretParams, DecryptedSecret, Secret, SecretError, SecretRef};

const SEP: u8 = 0x1f;

fn db_err(ctx: &str, e: impl std::fmt::Display) -> SecretError {
    SecretError::Database(format!("{ctx}: {e}"))
}

fn key_of(user_id: &str, name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(user_id.len() + name.len() + 1);
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k.extend_from_slice(name.as_bytes());
    k
}

fn user_prefix(user_id: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(user_id.len() + 1);
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k
}

/// Fjall-backed [`SecretsStore`].
pub struct FjallSecretsStore {
    keyspace: Keyspace,
    secrets: PartitionHandle,
    id_idx: PartitionHandle,
    crypto: Arc<SecretsCrypto>,
}

impl FjallSecretsStore {
    /// Open (or create) the secret store at `path`.
    pub fn open(path: &str, crypto: Arc<SecretsCrypto>) -> Result<Self, SecretError> {
        let keyspace = Config::new(path).open().map_err(|e| db_err("open keyspace", e))?;
        let secrets = keyspace
            .open_partition("secrets", PartitionCreateOptions::default())
            .map_err(|e| db_err("open secrets", e))?;
        let id_idx = keyspace
            .open_partition("id_idx", PartitionCreateOptions::default())
            .map_err(|e| db_err("open id_idx", e))?;
        Ok(Self {
            keyspace,
            secrets,
            id_idx,
            crypto,
        })
    }

    fn persist(&self) -> Result<(), SecretError> {
        self.keyspace
            .persist(PersistMode::Buffer)
            .map_err(|e| db_err("persist", e))
    }

    fn load(&self, user_id: &str, name: &str) -> Result<Option<Secret>, SecretError> {
        match self
            .secrets
            .get(key_of(user_id, name))
            .map_err(|e| db_err("get secret", e))?
        {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).map_err(|e| db_err("decode secret", e))?,
            )),
            None => Ok(None),
        }
    }

    fn store(&self, secret: &Secret) -> Result<(), SecretError> {
        let bytes = serde_json::to_vec(secret).map_err(|e| db_err("encode secret", e))?;
        self.secrets
            .insert(key_of(&secret.user_id, &secret.name), bytes)
            .map_err(|e| db_err("put secret", e))?;
        self.id_idx
            .insert(secret.id.as_bytes(), key_of(&secret.user_id, &secret.name))
            .map_err(|e| db_err("put id_idx", e))?;
        Ok(())
    }
}

#[async_trait]
impl SecretsStore for FjallSecretsStore {
    async fn create(
        &self,
        user_id: &str,
        params: CreateSecretParams,
    ) -> Result<Secret, SecretError> {
        let plaintext = params.value.expose_secret().as_bytes();
        let (encrypted_value, key_salt) = self.crypto.encrypt(plaintext)?;
        let now = Utc::now();

        // Upsert: preserve id/created_at/usage_count/last_used_at on update.
        let secret = match self.load(user_id, &params.name)? {
            Some(existing) => Secret {
                encrypted_value,
                key_salt,
                provider: params.provider,
                expires_at: params.expires_at,
                updated_at: now,
                ..existing
            },
            None => Secret {
                id: Uuid::new_v4(),
                user_id: user_id.to_string(),
                name: params.name.clone(),
                encrypted_value,
                key_salt,
                provider: params.provider,
                expires_at: params.expires_at,
                last_used_at: None,
                usage_count: 0,
                created_at: now,
                updated_at: now,
            },
        };
        self.store(&secret)?;
        self.persist()?;
        Ok(secret)
    }

    async fn get(&self, user_id: &str, name: &str) -> Result<Secret, SecretError> {
        match self.load(user_id, name)? {
            Some(secret) => {
                if let Some(expires_at) = secret.expires_at {
                    if expires_at < Utc::now() {
                        return Err(SecretError::Expired);
                    }
                }
                Ok(secret)
            }
            None => Err(SecretError::NotFound(name.to_string())),
        }
    }

    async fn get_decrypted(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<DecryptedSecret, SecretError> {
        let secret = self.get(user_id, name).await?;
        self.crypto.decrypt(&secret.encrypted_value, &secret.key_salt)
    }

    async fn exists(&self, user_id: &str, name: &str) -> Result<bool, SecretError> {
        self.secrets
            .contains_key(key_of(user_id, name))
            .map_err(|e| db_err("exists", e))
    }

    async fn list(&self, user_id: &str) -> Result<Vec<SecretRef>, SecretError> {
        let mut refs = Vec::new();
        for kv in self.secrets.prefix(user_prefix(user_id)) {
            let (_k, v) = kv.map_err(|e| db_err("scan secrets", e))?;
            let secret: Secret =
                serde_json::from_slice(&v).map_err(|e| db_err("decode secret", e))?;
            refs.push(SecretRef {
                name: secret.name,
                provider: secret.provider,
            });
        }
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(refs)
    }

    async fn delete(&self, user_id: &str, name: &str) -> Result<bool, SecretError> {
        let Some(secret) = self.load(user_id, name)? else {
            return Ok(false);
        };
        self.secrets
            .remove(key_of(user_id, name))
            .map_err(|e| db_err("del secret", e))?;
        self.id_idx
            .remove(secret.id.as_bytes())
            .map_err(|e| db_err("del id_idx", e))?;
        self.persist()?;
        Ok(true)
    }

    async fn record_usage(&self, secret_id: Uuid) -> Result<(), SecretError> {
        let Some(key) = self
            .id_idx
            .get(secret_id.as_bytes())
            .map_err(|e| db_err("get id_idx", e))?
        else {
            return Ok(());
        };
        if let Some(bytes) = self.secrets.get(&key).map_err(|e| db_err("get secret", e))? {
            let mut secret: Secret =
                serde_json::from_slice(&bytes).map_err(|e| db_err("decode secret", e))?;
            secret.usage_count += 1;
            secret.last_used_at = Some(Utc::now());
            let updated = serde_json::to_vec(&secret).map_err(|e| db_err("encode secret", e))?;
            self.secrets
                .insert(key.to_vec(), updated)
                .map_err(|e| db_err("put secret", e))?;
            self.persist()?;
        }
        Ok(())
    }

    async fn is_accessible(
        &self,
        user_id: &str,
        secret_name: &str,
        allowed_secrets: &[String],
    ) -> Result<bool, SecretError> {
        if !self.exists(user_id, secret_name).await? {
            return Ok(false);
        }
        for pattern in allowed_secrets {
            if pattern == secret_name {
                return Ok(true);
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                if secret_name.starts_with(prefix) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn temp_store() -> (tempfile::TempDir, FjallSecretsStore) {
        let dir = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef0123456789abcdef";
        let crypto = Arc::new(SecretsCrypto::new(SecretString::from(key.to_string())).unwrap());
        let store =
            FjallSecretsStore::open(&dir.path().join("secrets").to_string_lossy(), crypto).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn create_get_decrypt() {
        let (_d, s) = temp_store();
        s.create("user1", CreateSecretParams::new("api_key", "sk-test-12345"))
            .await
            .unwrap();
        let decrypted = s.get_decrypted("user1", "api_key").await.unwrap();
        assert_eq!(decrypted.expose(), "sk-test-12345");
    }

    #[tokio::test]
    async fn upsert_preserves_id_and_created() {
        let (_d, s) = temp_store();
        let first = s
            .create("u", CreateSecretParams::new("k", "v1"))
            .await
            .unwrap();
        let second = s
            .create("u", CreateSecretParams::new("k", "v2"))
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(s.get_decrypted("u", "k").await.unwrap().expose(), "v2");
    }

    #[tokio::test]
    async fn exists_list_delete() {
        let (_d, s) = temp_store();
        assert!(!s.exists("u", "k").await.unwrap());
        s.create("u", CreateSecretParams::new("k1", "v")).await.unwrap();
        s.create("u", CreateSecretParams::new("k2", "v").with_provider("openai"))
            .await
            .unwrap();
        s.create("other", CreateSecretParams::new("k3", "v")).await.unwrap();

        let list = s.list("u").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "k1");

        assert!(s.delete("u", "k1").await.unwrap());
        assert!(!s.exists("u", "k1").await.unwrap());
        assert!(!s.delete("u", "k1").await.unwrap());
    }

    #[tokio::test]
    async fn record_usage_by_id() {
        let (_d, s) = temp_store();
        let secret = s.create("u", CreateSecretParams::new("k", "v")).await.unwrap();
        s.record_usage(secret.id).await.unwrap();
        let reloaded = s.get("u", "k").await.unwrap();
        assert_eq!(reloaded.usage_count, 1);
        assert!(reloaded.last_used_at.is_some());
    }

    #[tokio::test]
    async fn user_isolation_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef0123456789abcdef";
        let crypto = Arc::new(SecretsCrypto::new(SecretString::from(key.to_string())).unwrap());
        let path = dir.path().join("secrets").to_string_lossy().into_owned();
        {
            let s = FjallSecretsStore::open(&path, crypto.clone()).unwrap();
            s.create("u1", CreateSecretParams::new("shared", "u1v")).await.unwrap();
            s.create("u2", CreateSecretParams::new("shared", "u2v")).await.unwrap();
        }
        let s = FjallSecretsStore::open(&path, crypto).unwrap();
        assert_eq!(s.get_decrypted("u1", "shared").await.unwrap().expose(), "u1v");
        assert_eq!(s.get_decrypted("u2", "shared").await.unwrap().expose(), "u2v");
    }
}
