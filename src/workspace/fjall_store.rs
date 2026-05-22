//! Fjall-backed key-value store for workspace documents and chunks.
//!
//! Replaces the PostgreSQL `memory_documents` / `memory_chunks` tables with a
//! pure-Rust embedded LSM store. Layout (one Fjall keyspace, four partitions):
//!
//! - `docs`       : doc_id (16 bytes)            -> JSON(MemoryDocument)
//! - `doc_path`   : user \x1f agent \x1f path     -> doc_id (16 bytes)   [unique path index]
//! - `chunks`     : chunk_id (16 bytes)          -> JSON(MemoryChunk)
//! - `chunk_doc`  : doc_id (16) + index (be i32) -> chunk_id (16 bytes)  [chunks-by-doc, ordered]
//!
//! Full-text search (tantivy) and vectors (embedvec) are layered on top
//! elsewhere; this module owns only the canonical document/chunk records.
//! Methods are synchronous (fjall is sync); callers bridge to async.

// Wired into the workspace in step B3 (Repository swap); tests exercise it now.
#![allow(dead_code)]

use chrono::Utc;
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use uuid::Uuid;

use crate::error::WorkspaceError;
use crate::workspace::document::{MemoryChunk, MemoryDocument, WorkspaceEntry};

/// Field separator for composite keys (ASCII unit separator; absent from paths).
const SEP: u8 = 0x1f;
/// Sentinel for a `None` agent id in composite keys (never collides with a UUID).
const AGENT_NONE: &str = "~none~";

fn store_err(ctx: &str, e: impl std::fmt::Display) -> WorkspaceError {
    WorkspaceError::SearchFailed {
        reason: format!("{ctx}: {e}"),
    }
}

fn agent_str(agent_id: Option<Uuid>) -> String {
    agent_id
        .map(|a| a.to_string())
        .unwrap_or_else(|| AGENT_NONE.to_string())
}

/// Composite `doc_path` key: `user \x1f agent \x1f path`.
fn path_key(user_id: &str, agent_id: Option<Uuid>, path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(user_id.len() + path.len() + 40);
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k.extend_from_slice(agent_str(agent_id).as_bytes());
    k.push(SEP);
    k.extend_from_slice(path.as_bytes());
    k
}

/// Prefix matching all `doc_path` keys for a (user, agent): `user \x1f agent \x1f`.
fn path_prefix(user_id: &str, agent_id: Option<Uuid>) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(user_id.as_bytes());
    k.push(SEP);
    k.extend_from_slice(agent_str(agent_id).as_bytes());
    k.push(SEP);
    k
}

/// `chunk_doc` key: doc_id (16 bytes) followed by big-endian chunk index.
fn chunk_doc_key(document_id: Uuid, chunk_index: i32) -> Vec<u8> {
    let mut k = Vec::with_capacity(20);
    k.extend_from_slice(document_id.as_bytes());
    k.extend_from_slice(&chunk_index.to_be_bytes());
    k
}

fn uuid_from_slice(bytes: &[u8]) -> Result<Uuid, WorkspaceError> {
    Uuid::from_slice(bytes).map_err(|e| store_err("decode uuid", e))
}

/// Fjall-backed document + chunk store.
pub struct FjallStore {
    keyspace: Keyspace,
    docs: PartitionHandle,
    doc_path: PartitionHandle,
    chunks: PartitionHandle,
    chunk_doc: PartitionHandle,
}

impl FjallStore {
    /// Open (or create) the store at `path`.
    pub fn open(path: &str) -> Result<Self, WorkspaceError> {
        let keyspace = Config::new(path)
            .open()
            .map_err(|e| store_err("open keyspace", e))?;
        let docs = keyspace
            .open_partition("docs", PartitionCreateOptions::default())
            .map_err(|e| store_err("open docs", e))?;
        let doc_path = keyspace
            .open_partition("doc_path", PartitionCreateOptions::default())
            .map_err(|e| store_err("open doc_path", e))?;
        let chunks = keyspace
            .open_partition("chunks", PartitionCreateOptions::default())
            .map_err(|e| store_err("open chunks", e))?;
        let chunk_doc = keyspace
            .open_partition("chunk_doc", PartitionCreateOptions::default())
            .map_err(|e| store_err("open chunk_doc", e))?;
        Ok(Self {
            keyspace,
            docs,
            doc_path,
            chunks,
            chunk_doc,
        })
    }

    fn persist(&self) -> Result<(), WorkspaceError> {
        self.keyspace
            .persist(PersistMode::Buffer)
            .map_err(|e| store_err("persist", e))
    }

    fn load_doc(&self, id: Uuid) -> Result<Option<MemoryDocument>, WorkspaceError> {
        match self.docs.get(id.as_bytes()).map_err(|e| store_err("get doc", e))? {
            Some(bytes) => {
                let doc = serde_json::from_slice(&bytes).map_err(|e| store_err("decode doc", e))?;
                Ok(Some(doc))
            }
            None => Ok(None),
        }
    }

    fn store_doc(&self, doc: &MemoryDocument) -> Result<(), WorkspaceError> {
        let bytes = serde_json::to_vec(doc).map_err(|e| store_err("encode doc", e))?;
        self.docs
            .insert(doc.id.as_bytes(), bytes)
            .map_err(|e| store_err("put doc", e))?;
        self.doc_path
            .insert(
                path_key(&doc.user_id, doc.agent_id, &doc.path),
                doc.id.as_bytes().to_vec(),
            )
            .map_err(|e| store_err("put doc_path", e))?;
        Ok(())
    }

    // ==================== Document Operations ====================

    /// Get a document by its (user, agent, path), or `DocumentNotFound`.
    pub fn get_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        let id_bytes = self
            .doc_path
            .get(path_key(user_id, agent_id, path))
            .map_err(|e| store_err("get doc_path", e))?;
        match id_bytes {
            Some(b) => {
                let id = uuid_from_slice(&b)?;
                self.load_doc(id)?.ok_or(WorkspaceError::DocumentNotFound {
                    doc_type: path.to_string(),
                    user_id: user_id.to_string(),
                })
            }
            None => Err(WorkspaceError::DocumentNotFound {
                doc_type: path.to_string(),
                user_id: user_id.to_string(),
            }),
        }
    }

    /// Get a document by id.
    pub fn get_document_by_id(&self, id: Uuid) -> Result<MemoryDocument, WorkspaceError> {
        self.load_doc(id)?.ok_or(WorkspaceError::DocumentNotFound {
            doc_type: "unknown".to_string(),
            user_id: "unknown".to_string(),
        })
    }

    /// Get an existing document by path or create an empty one.
    pub fn get_or_create_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<MemoryDocument, WorkspaceError> {
        match self.get_document_by_path(user_id, agent_id, path) {
            Ok(doc) => Ok(doc),
            Err(WorkspaceError::DocumentNotFound { .. }) => {
                let doc = MemoryDocument::new(user_id, agent_id, path);
                self.store_doc(&doc)?;
                self.persist()?;
                Ok(doc)
            }
            Err(e) => Err(e),
        }
    }

    /// Replace a document's content and bump `updated_at`.
    pub fn update_document(&self, id: Uuid, content: &str) -> Result<(), WorkspaceError> {
        let mut doc = self.get_document_by_id(id)?;
        doc.content = content.to_string();
        doc.updated_at = Utc::now();
        self.store_doc(&doc)?;
        self.persist()
    }

    /// Delete a document (by path) and all of its chunks.
    pub fn delete_document_by_path(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        path: &str,
    ) -> Result<(), WorkspaceError> {
        let doc = self.get_document_by_path(user_id, agent_id, path)?;
        self.delete_chunks(doc.id)?;
        self.doc_path
            .remove(path_key(user_id, agent_id, path))
            .map_err(|e| store_err("del doc_path", e))?;
        self.docs
            .remove(doc.id.as_bytes())
            .map_err(|e| store_err("del doc", e))?;
        self.persist()
    }

    /// Load every document for a (user, agent).
    fn all_documents(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<MemoryDocument>, WorkspaceError> {
        let mut docs = Vec::new();
        for kv in self.doc_path.prefix(path_prefix(user_id, agent_id)) {
            let (_k, v) = kv.map_err(|e| store_err("scan doc_path", e))?;
            let id = uuid_from_slice(&v)?;
            if let Some(doc) = self.load_doc(id)? {
                docs.push(doc);
            }
        }
        Ok(docs)
    }

    /// All document paths for a (user, agent), sorted.
    pub fn list_all_paths(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<String>, WorkspaceError> {
        let mut paths: Vec<String> = self
            .all_documents(user_id, agent_id)?
            .into_iter()
            .map(|d| d.path)
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// All documents for a (user, agent), newest first.
    pub fn list_documents(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<MemoryDocument>, WorkspaceError> {
        let mut docs = self.all_documents(user_id, agent_id)?;
        docs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(docs)
    }

    /// List the immediate children (files + virtual directories) of a directory.
    ///
    /// Mirrors the old `list_workspace_files` SQL function: empty string lists
    /// the root; a directory is reported when any path has deeper components.
    pub fn list_directory(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
        directory: &str,
    ) -> Result<Vec<WorkspaceEntry>, WorkspaceError> {
        let dir = if directory.is_empty() || directory.ends_with('/') {
            directory.to_string()
        } else {
            format!("{directory}/")
        };

        // child_name -> entry being built
        let mut children: std::collections::HashMap<String, WorkspaceEntry> =
            std::collections::HashMap::new();

        for doc in self.all_documents(user_id, agent_id)? {
            if !dir.is_empty() && !doc.path.starts_with(&dir) {
                continue;
            }
            let rest = &doc.path[dir.len()..];
            if rest.is_empty() {
                continue;
            }
            let (child_name, is_dir) = match rest.find('/') {
                Some(i) => (rest[..i].to_string(), true),
                None => (rest.to_string(), false),
            };
            let full_path = format!("{dir}{child_name}");

            let entry = children.entry(child_name).or_insert_with(|| WorkspaceEntry {
                path: full_path,
                is_directory: is_dir,
                updated_at: None,
                content_preview: None,
            });
            // A child is a directory if any path nests beneath it.
            entry.is_directory = entry.is_directory || is_dir;
            // Track the latest update among the child's documents.
            if entry.updated_at.map_or(true, |t| doc.updated_at > t) {
                entry.updated_at = Some(doc.updated_at);
            }
            // Files get a content preview; directories do not.
            if !entry.is_directory {
                let preview: String = doc.content.chars().take(200).collect();
                entry.content_preview = Some(preview);
            } else {
                entry.content_preview = None;
            }
        }

        let mut entries: Vec<WorkspaceEntry> = children.into_values().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    // ==================== Chunk Operations ====================

    /// Insert a chunk and return its id.
    pub fn insert_chunk(
        &self,
        document_id: Uuid,
        chunk_index: i32,
        content: &str,
    ) -> Result<Uuid, WorkspaceError> {
        let chunk = crate::workspace::document::MemoryChunk::new(document_id, chunk_index, content);
        let bytes = serde_json::to_vec(&chunk).map_err(|e| store_err("encode chunk", e))?;
        self.chunks
            .insert(chunk.id.as_bytes(), bytes)
            .map_err(|e| store_err("put chunk", e))?;
        self.chunk_doc
            .insert(
                chunk_doc_key(document_id, chunk_index),
                chunk.id.as_bytes().to_vec(),
            )
            .map_err(|e| store_err("put chunk_doc", e))?;
        self.persist()?;
        Ok(chunk.id)
    }

    /// Load a single chunk by id.
    pub fn get_chunk(&self, id: Uuid) -> Result<Option<MemoryChunk>, WorkspaceError> {
        match self
            .chunks
            .get(id.as_bytes())
            .map_err(|e| store_err("get chunk", e))?
        {
            Some(bytes) => {
                let chunk =
                    serde_json::from_slice(&bytes).map_err(|e| store_err("decode chunk", e))?;
                Ok(Some(chunk))
            }
            None => Ok(None),
        }
    }

    /// Delete all chunks for a document. Returns the deleted chunk ids so the
    /// caller can evict them from the vector and FTS indexes.
    pub fn delete_chunks(&self, document_id: Uuid) -> Result<Vec<Uuid>, WorkspaceError> {
        let mut deleted = Vec::new();
        let mut idx_keys = Vec::new();
        for kv in self.chunk_doc.prefix(document_id.as_bytes().to_vec()) {
            let (k, v) = kv.map_err(|e| store_err("scan chunk_doc", e))?;
            let chunk_id = uuid_from_slice(&v)?;
            deleted.push(chunk_id);
            idx_keys.push(k.to_vec());
        }
        for chunk_id in &deleted {
            self.chunks
                .remove(chunk_id.as_bytes())
                .map_err(|e| store_err("del chunk", e))?;
        }
        for k in idx_keys {
            self.chunk_doc
                .remove(k)
                .map_err(|e| store_err("del chunk_doc", e))?;
        }
        self.persist()?;
        Ok(deleted)
    }

    /// Ids of all chunks for a (user, agent), via their documents.
    pub fn chunk_ids_for_user(
        &self,
        user_id: &str,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<Uuid>, WorkspaceError> {
        let mut ids = Vec::new();
        for doc in self.all_documents(user_id, agent_id)? {
            for kv in self.chunk_doc.prefix(doc.id.as_bytes().to_vec()) {
                let (_k, v) = kv.map_err(|e| store_err("scan chunk_doc", e))?;
                ids.push(uuid_from_slice(&v)?);
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FjallStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem").to_string_lossy().into_owned();
        (dir, FjallStore::open(&path).unwrap())
    }

    #[test]
    fn create_get_update_delete_document() {
        let (_d, s) = temp_store();
        let doc = s
            .get_or_create_document_by_path("u1", None, "context/vision.md")
            .unwrap();
        assert!(doc.content.is_empty());

        // get_or_create is idempotent (same id).
        let again = s
            .get_or_create_document_by_path("u1", None, "context/vision.md")
            .unwrap();
        assert_eq!(doc.id, again.id);

        s.update_document(doc.id, "hello world").unwrap();
        let fetched = s.get_document_by_path("u1", None, "context/vision.md").unwrap();
        assert_eq!(fetched.content, "hello world");
        assert_eq!(s.get_document_by_id(doc.id).unwrap().content, "hello world");

        s.delete_document_by_path("u1", None, "context/vision.md").unwrap();
        assert!(matches!(
            s.get_document_by_path("u1", None, "context/vision.md"),
            Err(WorkspaceError::DocumentNotFound { .. })
        ));
    }

    #[test]
    fn path_scoping_by_user_and_agent() {
        let (_d, s) = temp_store();
        let agent = Uuid::new_v4();
        s.get_or_create_document_by_path("u1", None, "a.md").unwrap();
        s.get_or_create_document_by_path("u1", Some(agent), "a.md").unwrap();
        s.get_or_create_document_by_path("u2", None, "a.md").unwrap();

        assert_eq!(s.list_all_paths("u1", None).unwrap(), vec!["a.md"]);
        assert_eq!(s.list_all_paths("u1", Some(agent)).unwrap(), vec!["a.md"]);
        // Distinct records despite identical path.
        assert_ne!(
            s.get_document_by_path("u1", None, "a.md").unwrap().id,
            s.get_document_by_path("u1", Some(agent), "a.md").unwrap().id
        );
    }

    #[test]
    fn list_directory_files_and_virtual_dirs() {
        let (_d, s) = temp_store();
        for p in ["README.md", "projects/alpha/notes.md", "projects/beta.md"] {
            let doc = s.get_or_create_document_by_path("u1", None, p).unwrap();
            s.update_document(doc.id, "some content").unwrap();
        }

        let root = s.list_directory("u1", None, "").unwrap();
        let names: Vec<&str> = root.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"projects"));
        let projects = root.iter().find(|e| e.name() == "projects").unwrap();
        assert!(projects.is_directory);
        assert!(projects.content_preview.is_none());
        let readme = root.iter().find(|e| e.name() == "README.md").unwrap();
        assert!(!readme.is_directory);
        assert!(readme.content_preview.is_some());

        let projects_dir = s.list_directory("u1", None, "projects").unwrap();
        let pnames: Vec<&str> = projects_dir.iter().map(|e| e.name()).collect();
        assert!(pnames.contains(&"alpha")); // virtual dir
        assert!(pnames.contains(&"beta.md")); // file
        assert!(projects_dir.iter().find(|e| e.name() == "alpha").unwrap().is_directory);
    }

    #[test]
    fn insert_and_delete_chunks() {
        let (_d, s) = temp_store();
        let doc = s.get_or_create_document_by_path("u1", None, "m.md").unwrap();
        let c0 = s.insert_chunk(doc.id, 0, "chunk zero").unwrap();
        let c1 = s.insert_chunk(doc.id, 1, "chunk one").unwrap();

        let mut ids = s.chunk_ids_for_user("u1", None).unwrap();
        ids.sort();
        let mut expected = vec![c0, c1];
        expected.sort();
        assert_eq!(ids, expected);

        let deleted = s.delete_chunks(doc.id).unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(s.chunk_ids_for_user("u1", None).unwrap().is_empty());
    }

    #[test]
    fn reopen_persists_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem").to_string_lossy().into_owned();
        let id;
        {
            let s = FjallStore::open(&path).unwrap();
            let doc = s.get_or_create_document_by_path("u1", None, "keep.md").unwrap();
            s.update_document(doc.id, "persisted body").unwrap();
            id = doc.id;
        }
        let s = FjallStore::open(&path).unwrap();
        assert_eq!(s.get_document_by_id(id).unwrap().content, "persisted body");
    }
}
