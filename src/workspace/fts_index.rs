//! Tantivy-backed full-text search index for workspace chunks.
//!
//! Replaces PostgreSQL's `tsvector` / `ts_rank_cd` full-text search. Each chunk
//! is one tantivy document: `chunk_id` + `document_id` as exact (STRING) fields,
//! `content` as a tokenized + stored TEXT field so results carry their text.
//!
//! Canonical chunk text lives in the Fjall store; the copy here exists only to
//! assemble search results without a second lookup. Single-tenant: results are
//! not scoped by user/agent (Iron Clad runs one "default" user), matching the
//! vector store. Writes go through a Mutex'd IndexWriter; search reads a reader.

// Wired into the workspace in step B3 (Repository swap); tests exercise it now.
#![allow(dead_code)]

use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};
use uuid::Uuid;

use crate::error::WorkspaceError;

fn fts_err(ctx: &str, e: impl std::fmt::Display) -> WorkspaceError {
    WorkspaceError::SearchFailed {
        reason: format!("{ctx}: {e}"),
    }
}

/// A full-text match resolved to workspace identifiers.
#[derive(Debug, Clone)]
pub struct FtsHit {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub content: String,
    pub score: f32,
}

/// Tantivy index over workspace chunks.
pub struct FtsIndex {
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    f_chunk_id: Field,
    f_document_id: Field,
    f_content: Field,
}

impl FtsIndex {
    /// Open (or create) the FTS index at `path`.
    pub fn open(path: &str) -> Result<Self, WorkspaceError> {
        std::fs::create_dir_all(path).map_err(|e| fts_err("create fts dir", e))?;

        let mut sb = Schema::builder();
        let f_chunk_id = sb.add_text_field("chunk_id", STRING | STORED);
        let f_document_id = sb.add_text_field("document_id", STRING | STORED);
        let f_content = sb.add_text_field("content", TEXT | STORED);
        let schema = sb.build();

        let dir = MmapDirectory::open(path).map_err(|e| fts_err("open mmap dir", e))?;
        let index = Index::open_or_create(dir, schema).map_err(|e| fts_err("open index", e))?;
        let writer: IndexWriter = index
            .writer(50_000_000)
            .map_err(|e| fts_err("create writer", e))?;
        let reader = index.reader().map_err(|e| fts_err("create reader", e))?;

        Ok(Self {
            index,
            writer: Mutex::new(writer),
            reader,
            f_chunk_id,
            f_document_id,
            f_content,
        })
    }

    fn writer(&self) -> Result<std::sync::MutexGuard<'_, IndexWriter>, WorkspaceError> {
        self.writer
            .lock()
            .map_err(|e| fts_err("writer lock", e))
    }

    /// Index a chunk. Not visible to search until [`commit`](Self::commit).
    pub fn add_chunk(
        &self,
        chunk_id: Uuid,
        document_id: Uuid,
        content: &str,
    ) -> Result<(), WorkspaceError> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.f_chunk_id, chunk_id.to_string());
        doc.add_text(self.f_document_id, document_id.to_string());
        doc.add_text(self.f_content, content);
        self.writer()?
            .add_document(doc)
            .map_err(|e| fts_err("add document", e))?;
        Ok(())
    }

    /// Remove every indexed chunk for a document. Effective after `commit`.
    pub fn delete_document(&self, document_id: Uuid) -> Result<(), WorkspaceError> {
        let term = Term::from_field_text(self.f_document_id, &document_id.to_string());
        self.writer()?.delete_term(term);
        Ok(())
    }

    /// Flush pending writes and refresh the reader so they become searchable.
    pub fn commit(&self) -> Result<(), WorkspaceError> {
        self.writer()?
            .commit()
            .map_err(|e| fts_err("commit", e))?;
        self.reader.reload().map_err(|e| fts_err("reload", e))?;
        Ok(())
    }

    /// Top-`k` full-text matches for `query`. A query that fails to parse
    /// yields no results (forgiving, like `plainto_tsquery`).
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<FtsHit>, WorkspaceError> {
        if k == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.f_content]);
        let parsed = match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => return Ok(Vec::new()),
        };

        let top = searcher
            .search(&parsed, &TopDocs::with_limit(k))
            .map_err(|e| fts_err("search", e))?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).map_err(|e| fts_err("fetch doc", e))?;
            let parse_id = |f: Field| {
                doc.get_first(f)
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            };
            let (Some(chunk_id), Some(document_id)) =
                (parse_id(self.f_chunk_id), parse_id(self.f_document_id))
            else {
                continue;
            };
            let content = doc
                .get_first(self.f_content)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            hits.push(FtsHit {
                chunk_id,
                document_id,
                content,
                score,
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index() -> (tempfile::TempDir, FtsIndex) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts").to_string_lossy().into_owned();
        (dir, FtsIndex::open(&path).unwrap())
    }

    #[test]
    fn add_commit_search() {
        let (_d, fts) = temp_index();
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();
        let c2 = Uuid::new_v4();
        fts.add_chunk(c1, doc, "the quick brown fox jumps").unwrap();
        fts.add_chunk(c2, doc, "lazy dogs sleep all day").unwrap();
        fts.commit().unwrap();

        let hits = fts.search("brown fox", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk_id, c1);
        assert_eq!(hits[0].document_id, doc);
        assert!(hits[0].content.contains("brown fox"));
    }

    #[test]
    fn delete_document_removes_from_results() {
        let (_d, fts) = temp_index();
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();
        fts.add_chunk(c1, doc, "unique searchable token zebra").unwrap();
        fts.commit().unwrap();
        assert!(!fts.search("zebra", 10).unwrap().is_empty());

        fts.delete_document(doc).unwrap();
        fts.commit().unwrap();
        assert!(fts.search("zebra", 10).unwrap().is_empty());
    }

    #[test]
    fn empty_and_unparseable_queries_are_safe() {
        let (_d, fts) = temp_index();
        let doc = Uuid::new_v4();
        fts.add_chunk(Uuid::new_v4(), doc, "content here").unwrap();
        fts.commit().unwrap();
        assert!(fts.search("", 10).unwrap().is_empty());
        // Lone special characters shouldn't error.
        let _ = fts.search("()", 10).unwrap();
    }

    #[test]
    fn reopen_persists_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts").to_string_lossy().into_owned();
        let doc = Uuid::new_v4();
        let c1 = Uuid::new_v4();
        {
            let fts = FtsIndex::open(&path).unwrap();
            fts.add_chunk(c1, doc, "persisted searchable phrase").unwrap();
            fts.commit().unwrap();
        }
        let fts = FtsIndex::open(&path).unwrap();
        let hits = fts.search("persisted phrase", 10).unwrap();
        assert_eq!(hits[0].chunk_id, c1);
    }
}
