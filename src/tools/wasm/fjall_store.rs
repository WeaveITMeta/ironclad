//! Fjall-backed WASM tool storage (replaces the PostgreSQL implementation).
//!
//! Partitions in one keyspace:
//! - `meta`     : tool_id (16)                 -> JSON(StoredWasmTool)
//! - `binary`   : tool_id (16)                 -> hash(32) || wasm_binary
//! - `name_idx` : user \x1f name \x1f version    -> tool_id (16)
//! - `caps`     : tool_id (16)                 -> JSON(StoredCapabilities)
//!
//! BLAKE3 integrity is preserved: the hash is stored alongside the binary and
//! re-verified on load.

use chrono::Utc;
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use uuid::Uuid;

use crate::tools::wasm::storage::{
    StoreToolParams, StoredCapabilities, StoredWasmTool, StoredWasmToolWithBinary, ToolStatus,
    WasmStorageError, WasmToolStore, compute_binary_hash, verify_binary_integrity,
};

const SEP: u8 = 0x1f;
const HASH_LEN: usize = 32;

fn db_err(ctx: &str, e: impl std::fmt::Display) -> WasmStorageError {
    WasmStorageError::Database(format!("{ctx}: {e}"))
}

fn name_key(user_id: &str, name: &str, version: &str) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k.extend_from_slice(name.as_bytes());
    k.push(SEP);
    k.extend_from_slice(version.as_bytes());
    k
}

fn name_prefix(user_id: &str, name: &str) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k.extend_from_slice(name.as_bytes());
    k.push(SEP);
    k
}

fn user_prefix(user_id: &str) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k
}

fn uuid_from(bytes: &[u8]) -> Result<Uuid, WasmStorageError> {
    Uuid::from_slice(bytes).map_err(|e| db_err("decode id", e))
}

/// Fjall-backed [`WasmToolStore`].
pub struct FjallWasmToolStore {
    keyspace: Keyspace,
    meta: PartitionHandle,
    binary: PartitionHandle,
    name_idx: PartitionHandle,
    caps: PartitionHandle,
}

impl FjallWasmToolStore {
    /// Open (or create) the WASM tool store at `path`.
    pub fn open(path: &str) -> Result<Self, WasmStorageError> {
        let keyspace = Config::new(path).open().map_err(|e| db_err("open keyspace", e))?;
        let meta = keyspace
            .open_partition("meta", PartitionCreateOptions::default())
            .map_err(|e| db_err("open meta", e))?;
        let binary = keyspace
            .open_partition("binary", PartitionCreateOptions::default())
            .map_err(|e| db_err("open binary", e))?;
        let name_idx = keyspace
            .open_partition("name_idx", PartitionCreateOptions::default())
            .map_err(|e| db_err("open name_idx", e))?;
        let caps = keyspace
            .open_partition("caps", PartitionCreateOptions::default())
            .map_err(|e| db_err("open caps", e))?;
        Ok(Self {
            keyspace,
            meta,
            binary,
            name_idx,
            caps,
        })
    }

    fn persist(&self) -> Result<(), WasmStorageError> {
        self.keyspace
            .persist(PersistMode::Buffer)
            .map_err(|e| db_err("persist", e))
    }

    fn load_meta(&self, tool_id: Uuid) -> Result<Option<StoredWasmTool>, WasmStorageError> {
        match self.meta.get(tool_id.as_bytes()).map_err(|e| db_err("get meta", e))? {
            Some(b) => Ok(Some(
                serde_json::from_slice(&b).map_err(|e| db_err("decode meta", e))?,
            )),
            None => Ok(None),
        }
    }

    /// All (version, tool_id) for a name, sorted by version descending.
    fn versions(&self, user_id: &str, name: &str) -> Result<Vec<(String, Uuid)>, WasmStorageError> {
        let mut out = Vec::new();
        for kv in self.name_idx.prefix(name_prefix(user_id, name)) {
            let (k, v) = kv.map_err(|e| db_err("scan name_idx", e))?;
            // key = user \x1f name \x1f version ; version is after the 2nd SEP.
            let version = k
                .rsplit(|b| *b == SEP)
                .next()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            out.push((version, uuid_from(&v)?));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0)); // version DESC (lexicographic, as PG TEXT)
        Ok(out)
    }

    /// The latest-version tool meta for a name, or NotFound.
    fn latest_meta(&self, user_id: &str, name: &str) -> Result<StoredWasmTool, WasmStorageError> {
        let versions = self.versions(user_id, name)?;
        for (_v, id) in versions {
            if let Some(meta) = self.load_meta(id)? {
                return Ok(meta);
            }
        }
        Err(WasmStorageError::NotFound(name.to_string()))
    }
}

#[async_trait::async_trait]
impl WasmToolStore for FjallWasmToolStore {
    async fn store(&self, params: StoreToolParams) -> Result<StoredWasmTool, WasmStorageError> {
        let binary_hash = compute_binary_hash(&params.wasm_binary);
        let now = Utc::now();

        // Upsert by (user, name, version): preserve id + created_at if present.
        let existing_id = self
            .name_idx
            .get(name_key(&params.user_id, &params.name, &params.version))
            .map_err(|e| db_err("get name_idx", e))?;
        let (id, created_at) = match existing_id {
            Some(b) => {
                let id = uuid_from(&b)?;
                let created = self.load_meta(id)?.map(|m| m.created_at).unwrap_or(now);
                (id, created)
            }
            None => (Uuid::new_v4(), now),
        };

        let tool = StoredWasmTool {
            id,
            user_id: params.user_id.clone(),
            name: params.name.clone(),
            version: params.version.clone(),
            description: params.description,
            parameters_schema: params.parameters_schema,
            source_url: params.source_url,
            trust_level: params.trust_level,
            status: ToolStatus::Active,
            created_at,
            updated_at: now,
        };

        let meta_bytes = serde_json::to_vec(&tool).map_err(|e| db_err("encode meta", e))?;
        let mut bin = Vec::with_capacity(HASH_LEN + params.wasm_binary.len());
        bin.extend_from_slice(&binary_hash);
        bin.extend_from_slice(&params.wasm_binary);

        self.meta
            .insert(id.as_bytes(), meta_bytes)
            .map_err(|e| db_err("put meta", e))?;
        self.binary
            .insert(id.as_bytes(), bin)
            .map_err(|e| db_err("put binary", e))?;
        self.name_idx
            .insert(
                name_key(&params.user_id, &params.name, &params.version),
                id.as_bytes().to_vec(),
            )
            .map_err(|e| db_err("put name_idx", e))?;
        self.persist()?;
        Ok(tool)
    }

    async fn get(&self, user_id: &str, name: &str) -> Result<StoredWasmTool, WasmStorageError> {
        let tool = self.latest_meta(user_id, name)?;
        match tool.status {
            ToolStatus::Active => Ok(tool),
            ToolStatus::Disabled => Err(WasmStorageError::Disabled),
            ToolStatus::Quarantined => Err(WasmStorageError::Quarantined),
        }
    }

    async fn get_with_binary(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<StoredWasmToolWithBinary, WasmStorageError> {
        let tool = self.latest_meta(user_id, name)?;
        let stored = self
            .binary
            .get(tool.id.as_bytes())
            .map_err(|e| db_err("get binary", e))?
            .ok_or_else(|| WasmStorageError::NotFound(name.to_string()))?;
        if stored.len() < HASH_LEN {
            return Err(WasmStorageError::InvalidData("binary record too short".into()));
        }
        let binary_hash = stored[..HASH_LEN].to_vec();
        let wasm_binary = stored[HASH_LEN..].to_vec();
        if !verify_binary_integrity(&wasm_binary, &binary_hash) {
            tracing::error!(user_id, name, "WASM binary integrity check failed");
            return Err(WasmStorageError::IntegrityCheckFailed);
        }
        match tool.status {
            ToolStatus::Active => Ok(StoredWasmToolWithBinary {
                tool,
                wasm_binary,
                binary_hash,
            }),
            ToolStatus::Disabled => Err(WasmStorageError::Disabled),
            ToolStatus::Quarantined => Err(WasmStorageError::Quarantined),
        }
    }

    async fn get_capabilities(
        &self,
        tool_id: Uuid,
    ) -> Result<Option<StoredCapabilities>, WasmStorageError> {
        match self.caps.get(tool_id.as_bytes()).map_err(|e| db_err("get caps", e))? {
            Some(b) => Ok(Some(
                serde_json::from_slice(&b).map_err(|e| db_err("decode caps", e))?,
            )),
            None => Ok(None),
        }
    }

    async fn list(&self, user_id: &str) -> Result<Vec<StoredWasmTool>, WasmStorageError> {
        // Latest version per name.
        use std::collections::HashMap;
        let mut latest: HashMap<String, (String, Uuid)> = HashMap::new();
        for kv in self.name_idx.prefix(user_prefix(user_id)) {
            let (k, v) = kv.map_err(|e| db_err("scan name_idx", e))?;
            let parts: Vec<&[u8]> = k.split(|b| *b == SEP).collect();
            if parts.len() < 3 {
                continue;
            }
            let name = String::from_utf8_lossy(parts[1]).into_owned();
            let version = String::from_utf8_lossy(parts[2]).into_owned();
            let id = uuid_from(&v)?;
            latest
                .entry(name)
                .and_modify(|cur| {
                    if version > cur.0 {
                        *cur = (version.clone(), id);
                    }
                })
                .or_insert((version, id));
        }
        let mut tools = Vec::new();
        for (_name, (_v, id)) in latest {
            if let Some(meta) = self.load_meta(id)? {
                tools.push(meta);
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }

    async fn update_status(
        &self,
        user_id: &str,
        name: &str,
        status: ToolStatus,
    ) -> Result<(), WasmStorageError> {
        let versions = self.versions(user_id, name)?;
        if versions.is_empty() {
            return Err(WasmStorageError::NotFound(name.to_string()));
        }
        for (_v, id) in versions {
            if let Some(mut meta) = self.load_meta(id)? {
                meta.status = status;
                meta.updated_at = Utc::now();
                let bytes = serde_json::to_vec(&meta).map_err(|e| db_err("encode meta", e))?;
                self.meta
                    .insert(id.as_bytes(), bytes)
                    .map_err(|e| db_err("put meta", e))?;
            }
        }
        self.persist()?;
        Ok(())
    }

    async fn delete(&self, user_id: &str, name: &str) -> Result<bool, WasmStorageError> {
        let versions = self.versions(user_id, name)?;
        if versions.is_empty() {
            return Ok(false);
        }
        for (version, id) in versions {
            self.meta.remove(id.as_bytes()).map_err(|e| db_err("del meta", e))?;
            self.binary.remove(id.as_bytes()).map_err(|e| db_err("del binary", e))?;
            self.caps.remove(id.as_bytes()).map_err(|e| db_err("del caps", e))?;
            self.name_idx
                .remove(name_key(user_id, name, &version))
                .map_err(|e| db_err("del name_idx", e))?;
        }
        self.persist()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::wasm::storage::TrustLevel;

    fn temp_store() -> (tempfile::TempDir, FjallWasmToolStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FjallWasmToolStore::open(&dir.path().join("wasm").to_string_lossy()).unwrap();
        (dir, s)
    }

    fn params(user: &str, name: &str, version: &str, body: &[u8]) -> StoreToolParams {
        StoreToolParams {
            user_id: user.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            description: "desc".to_string(),
            wasm_binary: body.to_vec(),
            parameters_schema: serde_json::json!({"type": "object"}),
            source_url: None,
            trust_level: TrustLevel::User,
        }
    }

    #[tokio::test]
    async fn store_get_with_binary_integrity() {
        let (_d, s) = temp_store();
        let body = b"(module wasm body)";
        let stored = s.store(params("u", "t", "1.0.0", body)).await.unwrap();
        assert_eq!(stored.name, "t");

        let got = s.get("u", "t").await.unwrap();
        assert_eq!(got.id, stored.id);

        let with_bin = s.get_with_binary("u", "t").await.unwrap();
        assert_eq!(with_bin.wasm_binary, body);
        assert_eq!(with_bin.binary_hash, compute_binary_hash(body));
    }

    #[tokio::test]
    async fn list_returns_latest_version_per_name() {
        let (_d, s) = temp_store();
        s.store(params("u", "t", "1.0.0", b"a")).await.unwrap();
        s.store(params("u", "t", "1.2.0", b"b")).await.unwrap();
        s.store(params("u", "other", "0.1.0", b"c")).await.unwrap();

        let list = s.list("u").await.unwrap();
        assert_eq!(list.len(), 2);
        let t = list.iter().find(|x| x.name == "t").unwrap();
        assert_eq!(t.version, "1.2.0");
    }

    #[tokio::test]
    async fn update_status_then_get_errors() {
        let (_d, s) = temp_store();
        s.store(params("u", "t", "1.0.0", b"x")).await.unwrap();
        s.update_status("u", "t", ToolStatus::Disabled).await.unwrap();
        assert!(matches!(s.get("u", "t").await, Err(WasmStorageError::Disabled)));
    }

    #[tokio::test]
    async fn delete_removes_tool() {
        let (_d, s) = temp_store();
        s.store(params("u", "t", "1.0.0", b"x")).await.unwrap();
        assert!(s.delete("u", "t").await.unwrap());
        assert!(matches!(s.get("u", "t").await, Err(WasmStorageError::NotFound(_))));
        assert!(!s.delete("u", "t").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_same_version_keeps_id() {
        let (_d, s) = temp_store();
        let a = s.store(params("u", "t", "1.0.0", b"v1")).await.unwrap();
        let b = s.store(params("u", "t", "1.0.0", b"v2")).await.unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(s.get_with_binary("u", "t").await.unwrap().wasm_binary, b"v2");
    }
}
