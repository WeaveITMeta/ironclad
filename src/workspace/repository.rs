//! Database repository for workspace persistence.
//!
//! All workspace data is stored in PostgreSQL:
//! - Documents in `memory_documents` table
//! - Chunks in `memory_chunks` table (with FTS and vector indexes)

use std::sync::Arc;

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::error::WorkspaceError;

use crate::workspace::document::{MemoryChunk, MemoryDocument, WorkspaceEntry};
use crate::workspace::search::{RankedResult, SearchConfig, SearchResult, reciprocal_rank_fusion};
use crate::workspace::vector_store::VectorStore;

/// Database repository for workspace operations.
pub struct Repository {
    pool: Pool,
    /// Optional embedvec-backed vector index (Fjall). When set, semantic search
    /// and embedding writes go through it instead of pgvector.
    vector_store: Option<Arc<VectorStore>>,
}

impl Repository {
    /// Create a new repository with a connection pool.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            vector_store: None,
        }
    }

    /// Attach an embedvec-backed vector store for semantic search.
    pub fn with_vector_store(mut self, vector_store: Arc<VectorStore>) -> Self {
        self.vector_store = Some(vector_store);
        self
    }

    /// Get a connection from the pool.
    async fn conn(&self) -> Result<deadpool_postgres::Object, WorkspaceError> {
        self.pool
            .get()
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("Failed to get connection: {}", e),
            })
    }

    // ==================== Document Operations ====================

    /// Get a document by its path.
    pub async fn get_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        let conn = self.conn().await?;

        let row = conn
            .query_opt(
                r#"
                SELECT id, user_id, agent_id, path, content,
                       created_at, updated_at, metadata
                FROM memory_documents
                WHERE user_id = $1 AND agent_id IS NOT DISTINCT FROM $2 AND path = $3
                "#,
                &[&user_id, &agent_id, &path],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("Query failed: {}", e),
            })?;

        match row {
            Some(row) => Ok(self.row_to_document(&row)),
            None => Err(WorkspaceError::DocumentNotFound {
                doc_type: path.to_string(),
                user_id: user_id.to_string(),
            }),
        }
    }

    /// Get a document by ID.
    pub async fn get_document_by_id(&self, id: Uuid) -> Result<MemoryDocument, WorkspaceError> {
        let conn = self.conn().await?;

        let row = conn
            .query_opt(
                r#"
                SELECT id, user_id, agent_id, path, content,
                       created_at, updated_at, metadata
                FROM memory_documents WHERE id = $1
                "#,
                &[&id],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("Query failed: {}", e),
            })?;

        match row {
            Some(row) => Ok(self.row_to_document(&row)),
            None => Err(WorkspaceError::DocumentNotFound {
                doc_type: "unknown".to_string(),
                user_id: "unknown".to_string(),
            }),
        }
    }

    /// Get or create a document by path.
    pub async fn get_or_create_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        // Try to get existing document first
        match self.get_document_by_path(user_id, agent_id, path).await {
            Ok(doc) => return Ok(doc),
            Err(WorkspaceError::DocumentNotFound { .. }) => {}
            Err(e) => return Err(e),
        }

        // Create new document
        let conn = self.conn().await?;
        let id = Uuid::new_v4();
        let now = Utc::now();
        let metadata = serde_json::json!({});

        conn.execute(
            r#"
            INSERT INTO memory_documents (id, user_id, agent_id, path, content, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, $4, '', $5, $6, $7)
            ON CONFLICT (user_id, agent_id, path) DO NOTHING
            "#,
            &[&id, &user_id, &agent_id, &path, &metadata, &now, &now],
        )
        .await
        .map_err(|e| WorkspaceError::SearchFailed {
            reason: format!("Insert failed: {}", e),
        })?;

        // Fetch the document (might have been created by concurrent request)
        self.get_document_by_path(user_id, agent_id, path).await
    }

    /// Update a document's content.
    pub async fn update_document(&self, id: Uuid, content: &str) -> Result<(), WorkspaceError> {
        let conn = self.conn().await?;

        conn.execute(
            "UPDATE memory_documents SET content = $2, updated_at = NOW() WHERE id = $1",
            &[&id, &content],
        )
        .await
        .map_err(|e| WorkspaceError::SearchFailed {
            reason: format!("Update failed: {}", e),
        })?;

        Ok(())
    }

    /// Delete a document by its path.
    pub async fn delete_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<(), WorkspaceError> {
        let conn = self.conn().await?;

        // First get the document to delete its chunks
        let doc = self.get_document_by_path(user_id, agent_id, path).await?;
        self.delete_chunks(doc.id).await?;

        // Delete the document
        conn.execute(
            r#"
            DELETE FROM memory_documents
            WHERE user_id = $1 AND agent_id IS NOT DISTINCT FROM $2 AND path = $3
            "#,
            &[&user_id, &agent_id, &path],
        )
        .await
        .map_err(|e| WorkspaceError::SearchFailed {
            reason: format!("Delete failed: {}", e),
        })?;

        Ok(())
    }

    /// List files and directories in a directory path.
    ///
    /// Returns immediate children (not recursive).
    /// Empty string lists the root directory.
    pub async fn list_directory(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        directory: &str,
    ) -> Result<Vec<WorkspaceEntry>, WorkspaceError> {
        let conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT path, is_directory, updated_at, content_preview FROM list_workspace_files($1, $2, $3)",
                &[&user_id, &agent_id, &directory],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("List directory failed: {}", e),
            })?;

        Ok(rows
            .iter()
            .map(|row| {
                let updated_at: Option<DateTime<Utc>> = row.get("updated_at");
                WorkspaceEntry {
                    path: row.get("path"),
                    is_directory: row.get("is_directory"),
                    updated_at,
                    content_preview: row.get("content_preview"),
                }
            })
            .collect())
    }

    /// List all file paths in the workspace (flat list).
    pub async fn list_all_paths(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<String>, WorkspaceError> {
        let conn = self.conn().await?;

        let rows = conn
            .query(
                r#"
                SELECT path FROM memory_documents
                WHERE user_id = $1 AND agent_id IS NOT DISTINCT FROM $2
                ORDER BY path
                "#,
                &[&user_id, &agent_id],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("List paths failed: {}", e),
            })?;

        Ok(rows.iter().map(|row| row.get("path")).collect())
    }

    /// List all documents for a user.
    pub async fn list_documents(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<MemoryDocument>, WorkspaceError> {
        let conn = self.conn().await?;

        let rows = conn
            .query(
                r#"
                SELECT id, user_id, agent_id, path, content,
                       created_at, updated_at, metadata
                FROM memory_documents
                WHERE user_id = $1 AND agent_id IS NOT DISTINCT FROM $2
                ORDER BY updated_at DESC
                "#,
                &[&user_id, &agent_id],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("Query failed: {}", e),
            })?;

        Ok(rows.iter().map(|r| self.row_to_document(r)).collect())
    }

    fn row_to_document(&self, row: &tokio_postgres::Row) -> MemoryDocument {
        MemoryDocument {
            id: row.get("id"),
            user_id: row.get("user_id"),
            agent_id: row.get("agent_id"),
            path: row.get("path"),
            content: row.get("content"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            metadata: row.get("metadata"),
        }
    }

    // ==================== Chunk Operations ====================

    /// Delete all chunks for a document.
    pub async fn delete_chunks(&self, document_id: Uuid) -> Result<(), WorkspaceError> {
        let conn = self.conn().await?;

        // Remove the document's vectors from the store first (keyed by chunk id).
        if let Some(vs) = &self.vector_store {
            let rows = conn
                .query(
                    "SELECT id FROM memory_chunks WHERE document_id = $1",
                    &[&document_id],
                )
                .await
                .map_err(|e| WorkspaceError::ChunkingFailed {
                    reason: format!("Lookup failed: {}", e),
                })?;
            for row in &rows {
                let chunk_id: Uuid = row.get("id");
                vs.delete(chunk_id).await?;
            }
        }

        conn.execute(
            "DELETE FROM memory_chunks WHERE document_id = $1",
            &[&document_id],
        )
        .await
        .map_err(|e| WorkspaceError::ChunkingFailed {
            reason: format!("Delete failed: {}", e),
        })?;

        Ok(())
    }

    /// Insert a chunk.
    pub async fn insert_chunk(
        &self,
        document_id: Uuid,
        chunk_index: i32,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<Uuid, WorkspaceError> {
        let conn = self.conn().await?;
        let id = Uuid::new_v4();

        conn.execute(
            r#"
            INSERT INTO memory_chunks (id, document_id, chunk_index, content)
            VALUES ($1, $2, $3, $4)
            "#,
            &[&id, &document_id, &chunk_index, &content],
        )
        .await
        .map_err(|e| WorkspaceError::ChunkingFailed {
            reason: format!("Insert failed: {}", e),
        })?;

        // Mirror the embedding into the vector store (keyed by chunk id).
        if let (Some(vs), Some(emb)) = (&self.vector_store, embedding) {
            vs.upsert(id, document_id, content, emb).await?;
        }

        Ok(id)
    }

    /// Update a chunk's embedding.
    pub async fn update_chunk_embedding(
        &self,
        chunk_id: Uuid,
        embedding: &[f32],
    ) -> Result<(), WorkspaceError> {
        // Embeddings now live only in the vector store. Look up the chunk's
        // document id + content and upsert; no-op if no store is attached.
        let Some(vs) = &self.vector_store else {
            return Ok(());
        };
        let conn = self.conn().await?;
        let row = conn
            .query_opt(
                "SELECT document_id, content FROM memory_chunks WHERE id = $1",
                &[&chunk_id],
            )
            .await
            .map_err(|e| WorkspaceError::EmbeddingFailed {
                reason: format!("Lookup failed: {}", e),
            })?;
        if let Some(row) = row {
            let document_id: Uuid = row.get("document_id");
            let content: String = row.get("content");
            vs.upsert(chunk_id, document_id, &content, embedding).await?;
        }

        Ok(())
    }

    /// Get chunks without embeddings for backfilling.
    pub async fn get_chunks_without_embeddings(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<MemoryChunk>, WorkspaceError> {
        let Some(vs) = &self.vector_store else {
            return Ok(Vec::new());
        };
        let conn = self.conn().await?;

        // A chunk needs an embedding if it isn't in the vector store yet.
        let rows = conn
            .query(
                r#"
                SELECT c.id, c.document_id, c.chunk_index, c.content, c.created_at
                FROM memory_chunks c
                JOIN memory_documents d ON d.id = c.document_id
                WHERE d.user_id = $1 AND d.agent_id IS NOT DISTINCT FROM $2
                ORDER BY c.created_at
                "#,
                &[&user_id, &agent_id],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("Query failed: {}", e),
            })?;

        let mut out = Vec::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            if vs.contains(id).await {
                continue;
            }
            out.push(MemoryChunk {
                id,
                document_id: row.get("document_id"),
                chunk_index: row.get("chunk_index"),
                content: row.get("content"),
                embedding: None,
                created_at: row.get("created_at"),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    // ==================== Search Operations ====================

    /// Perform hybrid search combining FTS and vector similarity.
    pub async fn hybrid_search(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        query: &str,
        embedding: Option<&[f32]>,
        config: &SearchConfig,
    ) -> Result<Vec<SearchResult>, WorkspaceError> {
        let fts_results = if config.use_fts {
            self.fts_search(user_id, agent_id, query, config.pre_fusion_limit)
                .await?
        } else {
            Vec::new()
        };

        let vector_results = if config.use_vector && embedding.is_some() {
            self.vector_search(
                user_id,
                agent_id,
                embedding.unwrap(),
                config.pre_fusion_limit,
            )
            .await?
        } else {
            Vec::new()
        };

        Ok(reciprocal_rank_fusion(fts_results, vector_results, config))
    }

    /// Full-text search using PostgreSQL ts_rank_cd.
    async fn fts_search(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedResult>, WorkspaceError> {
        let conn = self.conn().await?;

        let rows = conn
            .query(
                r#"
                SELECT c.id as chunk_id, c.document_id, c.content,
                       ts_rank_cd(c.content_tsv, plainto_tsquery('english', $3)) as rank
                FROM memory_chunks c
                JOIN memory_documents d ON d.id = c.document_id
                WHERE d.user_id = $1 AND d.agent_id IS NOT DISTINCT FROM $2
                  AND c.content_tsv @@ plainto_tsquery('english', $3)
                ORDER BY rank DESC
                LIMIT $4
                "#,
                &[&user_id, &agent_id, &query, &(limit as i64)],
            )
            .await
            .map_err(|e| WorkspaceError::SearchFailed {
                reason: format!("FTS query failed: {}", e),
            })?;

        Ok(rows
            .iter()
            .enumerate()
            .map(|(i, row)| RankedResult {
                chunk_id: row.get("chunk_id"),
                document_id: row.get("document_id"),
                content: row.get("content"),
                rank: (i + 1) as u32,
            })
            .collect())
    }

    /// Vector similarity search via the embedvec-backed store.
    ///
    /// Single-tenant: results are not filtered by user_id/agent_id, since
    /// IronClaw runs a single "default" user. Revisit for multi-tenant.
    async fn vector_search(
        &self,
        _user_id: &str,
        _agent_id: Option<Uuid>,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<RankedResult>, WorkspaceError> {
        let Some(vs) = &self.vector_store else {
            return Ok(Vec::new());
        };
        let hits = vs.search(embedding, limit).await?;
        Ok(hits
            .into_iter()
            .enumerate()
            .map(|(i, hit)| RankedResult {
                chunk_id: hit.chunk_id,
                document_id: hit.document_id,
                content: hit.content,
                rank: (i + 1) as u32,
            })
            .collect())
    }
}
