//! Embedvec-backed vector store with Fjall persistence.
//!
//! Replaces the pgvector path for semantic search. Vectors are keyed by the
//! chunk's UUID. embedvec assigns its own internal `usize` ids and deletes by
//! id (soft delete), so we keep a `chunk_id -> usize` map, rebuilt from the
//! persisted index payloads on open.
//!
//! `add`/`delete` take `&mut self` and `search` takes `&self`, so the index is
//! held behind an `RwLock`: writes for upsert/delete, reads for search.

use std::collections::HashMap;

use embedvec::{Distance, EmbedVec};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::WorkspaceError;

/// HNSW search beam. Higher = better recall, slower. Floored at `k` per query.
const EF_SEARCH: usize = 64;

struct Inner {
    index: EmbedVec,
    /// Maps our chunk UUIDs to embedvec's internal vector ids.
    by_chunk: HashMap<Uuid, usize>,
}

/// A semantic-search match resolved back to workspace identifiers.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub content: String,
    pub score: f32,
}

/// Persistent vector index for workspace chunk embeddings.
pub struct VectorStore {
    inner: RwLock<Inner>,
    dimension: usize,
}

impl VectorStore {
    /// Open (or create) a Fjall-persisted index at `path` for `dimension`-wide
    /// vectors, rebuilding the chunk-id map from any persisted payloads.
    pub async fn open(path: &str, dimension: usize) -> Result<Self, WorkspaceError> {
        let index = EmbedVec::builder()
            .dimension(dimension)
            .metric(Distance::Cosine)
            .persistence(path)
            .build()
            .await
            .map_err(|e| WorkspaceError::EmbeddingFailed {
                reason: format!("open vector store at {path}: {e}"),
            })?;

        let mut by_chunk = HashMap::new();
        for (id, payload) in index.entries() {
            if let Some(chunk_id) = payload
                .get("chunk_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                by_chunk.insert(chunk_id, id);
            }
        }

        Ok(Self {
            inner: RwLock::new(Inner { index, by_chunk }),
            dimension,
        })
    }

    /// Vector dimension this store was opened with.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Insert or replace the vector for a chunk. Any existing vector for the
    /// same `chunk_id` is soft-deleted first so re-embedding stays consistent.
    pub async fn upsert(
        &self,
        chunk_id: Uuid,
        document_id: Uuid,
        content: &str,
        vector: &[f32],
    ) -> Result<(), WorkspaceError> {
        if vector.len() != self.dimension {
            return Err(WorkspaceError::EmbeddingFailed {
                reason: format!(
                    "dimension mismatch: expected {}, got {}",
                    self.dimension,
                    vector.len()
                ),
            });
        }

        let mut guard = self.inner.write().await;

        if let Some(old_id) = guard.by_chunk.remove(&chunk_id) {
            guard
                .index
                .delete(old_id)
                .await
                .map_err(|e| WorkspaceError::EmbeddingFailed {
                    reason: format!("replace-delete vector: {e}"),
                })?;
        }

        let payload = serde_json::json!({
            "chunk_id": chunk_id.to_string(),
            "document_id": document_id.to_string(),
            "content": content,
        });

        let id = guard
            .index
            .add(vector, payload)
            .await
            .map_err(|e| WorkspaceError::EmbeddingFailed {
                reason: format!("add vector: {e}"),
            })?;

        guard.by_chunk.insert(chunk_id, id);
        Ok(())
    }

    /// Soft-delete a chunk's vector. No-op if the chunk has no vector.
    pub async fn delete(&self, chunk_id: Uuid) -> Result<(), WorkspaceError> {
        let mut guard = self.inner.write().await;
        if let Some(id) = guard.by_chunk.remove(&chunk_id) {
            guard
                .index
                .delete(id)
                .await
                .map_err(|e| WorkspaceError::EmbeddingFailed {
                    reason: format!("delete vector: {e}"),
                })?;
        }
        Ok(())
    }

    /// Whether a chunk currently has a live vector.
    pub async fn contains(&self, chunk_id: Uuid) -> bool {
        self.inner.read().await.by_chunk.contains_key(&chunk_id)
    }

    /// Top-`k` nearest neighbours for a query embedding, resolved to workspace
    /// identifiers. Hits whose payload can't be parsed are skipped.
    pub async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>, WorkspaceError> {
        if query.len() != self.dimension {
            return Err(WorkspaceError::SearchFailed {
                reason: format!(
                    "dimension mismatch: expected {}, got {}",
                    self.dimension,
                    query.len()
                ),
            });
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        let guard = self.inner.read().await;
        let hits = guard
            .index
            .search(query, k, EF_SEARCH.max(k), None)
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("vector search: {e}"),
            })?;

        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let parse = |key: &str| {
                hit.payload
                    .get(key)
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            };
            let (Some(chunk_id), Some(document_id)) = (parse("chunk_id"), parse("document_id"))
            else {
                continue;
            };
            let content = hit
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            out.push(VectorHit {
                chunk_id,
                document_id,
                content,
                score: hit.score,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_store(dim: usize) -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors").to_string_lossy().into_owned();
        let store = VectorStore::open(&path, dim).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn upsert_search_and_delete() {
        let (_dir, store) = temp_store(3).await;
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();
        let c2 = Uuid::new_v4();

        store.upsert(c1, doc, "alpha", &[1.0, 0.0, 0.0]).await.unwrap();
        store.upsert(c2, doc, "beta", &[0.0, 1.0, 0.0]).await.unwrap();

        let hits = store.search(&[1.0, 0.0, 0.0], 2).await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk_id, c1);
        assert_eq!(hits[0].document_id, doc);
        assert_eq!(hits[0].content, "alpha");

        assert!(store.contains(c1).await);
        store.delete(c1).await.unwrap();
        assert!(!store.contains(c1).await);

        // c1 no longer returned as the top exact match.
        let hits = store.search(&[1.0, 0.0, 0.0], 2).await.unwrap();
        assert!(hits.iter().all(|h| h.chunk_id != c1));
    }

    #[tokio::test]
    async fn upsert_replaces_existing_vector() {
        let (_dir, store) = temp_store(3).await;
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();

        store.upsert(c1, doc, "v1", &[1.0, 0.0, 0.0]).await.unwrap();
        store.upsert(c1, doc, "v2", &[0.0, 0.0, 1.0]).await.unwrap();

        let hits = store.search(&[0.0, 0.0, 1.0], 5).await.unwrap();
        let matches: Vec<_> = hits.iter().filter(|h| h.chunk_id == c1).collect();
        assert_eq!(matches.len(), 1, "chunk should have exactly one live vector");
        assert_eq!(matches[0].content, "v2");
    }

    #[tokio::test]
    async fn dimension_mismatch_is_rejected() {
        let (_dir, store) = temp_store(3).await;
        let err = store
            .upsert(Uuid::new_v4(), Uuid::new_v4(), "x", &[1.0, 0.0])
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn map_rebuilds_from_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors").to_string_lossy().into_owned();
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();

        {
            let store = VectorStore::open(&path, 3).await.unwrap();
            store.upsert(c1, doc, "persisted", &[1.0, 0.0, 0.0]).await.unwrap();
        }

        // Reopen: the chunk-id map must be rebuilt from persisted payloads.
        let store = VectorStore::open(&path, 3).await.unwrap();
        assert!(store.contains(c1).await);
        let hits = store.search(&[1.0, 0.0, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].chunk_id, c1);
    }
}
