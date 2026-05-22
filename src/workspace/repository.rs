//! Workspace persistence, backed by embedded pure-Rust stores.
//!
//! Documents + chunks live in [`FjallStore`] (Fjall KV), full-text search in
//! [`FtsIndex`] (tantivy), and vectors in [`VectorStore`] (embedvec). This
//! replaces the previous PostgreSQL-backed implementation; no database is
//! required for the memory workspace.
//!
//! Stores are shared (`Arc`) because each is single-writer; `Repository` is
//! cheap to clone and hands the same stores to every workspace in the process.
//! Methods stay `async` (callers await them); the Fjall/tantivy calls are sync.

use std::sync::Arc;

use uuid::Uuid;

use crate::error::WorkspaceError;
use crate::workspace::document::{MemoryChunk, MemoryDocument, WorkspaceEntry};
use crate::workspace::fjall_store::FjallStore;
use crate::workspace::fts_index::FtsIndex;
use crate::workspace::search::{RankedResult, SearchConfig, SearchResult, reciprocal_rank_fusion};
use crate::workspace::vector_store::VectorStore;

/// Coordinates the three embedded stores behind the workspace memory API.
#[derive(Clone)]
pub struct Repository {
    docs: Arc<FjallStore>,
    fts: Arc<FtsIndex>,
    vectors: Option<Arc<VectorStore>>,
}

impl Repository {
    /// Build a repository over shared stores.
    pub fn new(
        docs: Arc<FjallStore>,
        fts: Arc<FtsIndex>,
        vectors: Option<Arc<VectorStore>>,
    ) -> Self {
        Self { docs, fts, vectors }
    }

    // ==================== Document Operations ====================

    pub async fn get_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        self.docs.get_document_by_path(user_id, agent_id, path)
    }

    pub async fn get_document_by_id(&self, id: Uuid) -> Result<MemoryDocument, WorkspaceError> {
        self.docs.get_document_by_id(id)
    }

    pub async fn get_or_create_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        self.docs.get_or_create_document_by_path(user_id, agent_id, path)
    }

    pub async fn update_document(&self, id: Uuid, content: &str) -> Result<(), WorkspaceError> {
        self.docs.update_document(id, content)
    }

    /// Delete a document, its chunks, and its FTS + vector entries.
    pub async fn delete_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<(), WorkspaceError> {
        let doc = self.docs.get_document_by_path(user_id, agent_id, path)?;
        let chunk_ids = self.docs.delete_chunks(doc.id)?;
        self.fts.delete_document(doc.id)?;
        if let Some(vs) = &self.vectors {
            for id in &chunk_ids {
                vs.delete(*id).await?;
            }
        }
        self.docs.delete_document_by_path(user_id, agent_id, path)?;
        self.fts.commit()?;
        Ok(())
    }

    pub async fn list_directory(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        directory: &str,
    ) -> Result<Vec<WorkspaceEntry>, WorkspaceError> {
        self.docs.list_directory(user_id, agent_id, directory)
    }

    pub async fn list_all_paths(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<String>, WorkspaceError> {
        self.docs.list_all_paths(user_id, agent_id)
    }

    pub async fn list_documents(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<MemoryDocument>, WorkspaceError> {
        self.docs.list_documents(user_id, agent_id)
    }

    // ==================== Chunk Operations ====================

    /// Delete all chunks for a document from the KV store, FTS, and vectors.
    /// Does not commit the FTS writer (the caller batches commits).
    pub async fn delete_chunks(&self, document_id: Uuid) -> Result<(), WorkspaceError> {
        let chunk_ids = self.docs.delete_chunks(document_id)?;
        self.fts.delete_document(document_id)?;
        if let Some(vs) = &self.vectors {
            for id in &chunk_ids {
                vs.delete(*id).await?;
            }
        }
        Ok(())
    }

    /// Insert a chunk into the KV store + FTS (+ vector store if an embedding is
    /// provided). Does not commit the FTS writer (the caller batches commits).
    pub async fn insert_chunk(
        &self,
        document_id: Uuid,
        chunk_index: i32,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<Uuid, WorkspaceError> {
        let id = self.docs.insert_chunk(document_id, chunk_index, content)?;
        self.fts.add_chunk(id, document_id, content)?;
        if let (Some(vs), Some(emb)) = (&self.vectors, embedding) {
            vs.upsert(id, document_id, content, emb).await?;
        }
        Ok(id)
    }

    /// Commit staged FTS writes so they become searchable. Call after a batch
    /// of insert/delete operations (e.g. at the end of a reindex).
    pub async fn commit_search_index(&self) -> Result<(), WorkspaceError> {
        self.fts.commit()
    }

    /// Add/refresh a chunk's embedding in the vector store.
    pub async fn update_chunk_embedding(
        &self,
        chunk_id: Uuid,
        embedding: &[f32],
    ) -> Result<(), WorkspaceError> {
        let Some(vs) = &self.vectors else {
            return Ok(());
        };
        if let Some(chunk) = self.docs.get_chunk(chunk_id)? {
            vs.upsert(chunk_id, chunk.document_id, &chunk.content, embedding)
                .await?;
        }
        Ok(())
    }

    /// Chunks for a (user, agent) that have no vector yet (backfill candidates).
    pub async fn get_chunks_without_embeddings(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<MemoryChunk>, WorkspaceError> {
        let Some(vs) = &self.vectors else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for id in self.docs.chunk_ids_for_user(user_id, agent_id)? {
            if vs.contains(id).await {
                continue;
            }
            if let Some(chunk) = self.docs.get_chunk(id)? {
                out.push(chunk);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    // ==================== Search ====================

    /// Hybrid search: tantivy FTS + embedvec vectors, fused with RRF.
    ///
    /// Single-tenant: results are not scoped by user_id/agent_id (IronClaw runs
    /// one "default" user). Revisit for multi-tenant.
    pub async fn hybrid_search(
        &self,
        _user_id: &str,
        _agent_id: Option<Uuid>,
        query: &str,
        embedding: Option<&[f32]>,
        config: &SearchConfig,
    ) -> Result<Vec<SearchResult>, WorkspaceError> {
        let fts_results: Vec<RankedResult> = if config.use_fts {
            self.fts
                .search(query, config.pre_fusion_limit)?
                .into_iter()
                .enumerate()
                .map(|(i, h)| RankedResult {
                    chunk_id: h.chunk_id,
                    document_id: h.document_id,
                    content: h.content,
                    rank: (i + 1) as u32,
                })
                .collect()
        } else {
            Vec::new()
        };

        let vector_results: Vec<RankedResult> = match (config.use_vector, embedding, &self.vectors) {
            (true, Some(emb), Some(vs)) => vs
                .search(emb, config.pre_fusion_limit)
                .await?
                .into_iter()
                .enumerate()
                .map(|(i, h)| RankedResult {
                    chunk_id: h.chunk_id,
                    document_id: h.document_id,
                    content: h.content,
                    rank: (i + 1) as u32,
                })
                .collect(),
            _ => Vec::new(),
        };

        Ok(reciprocal_rank_fusion(fts_results, vector_results, config))
    }
}
