//! Obsidian vault bridge tools.
//!
//! Table of Contents:
//! - VaultReadTool: Read files from the Obsidian vault on disk
//! - VaultWriteTool: Write/update files in the Obsidian vault on disk
//! - VaultListTool: List files and directories in the Obsidian vault
//!
//! These tools bridge Iron Clad's workspace (PostgreSQL) with the user's
//! Obsidian vault (filesystem), enabling the agent to read and write
//! markdown files directly in the life system.

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Base path for the Obsidian vault.
/// Configured via VAULT_PATH env var, defaults to the Olson vault location.
fn vault_base_path() -> PathBuf {
    std::env::var("VAULT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Users\miksu\Documents\Olson"))
}

/// Validate that a resolved path is within the vault root (prevent path traversal).
fn validate_vault_path(path: &Path, base: &Path) -> Result<PathBuf, ToolError> {
    let canonical_base = base.canonicalize().map_err(|e| {
        ToolError::ExecutionFailed(format!("Cannot resolve vault base path: {}", e))
    })?;
    let canonical_path = path.canonicalize().map_err(|e| {
        ToolError::ExecutionFailed(format!("Cannot resolve path: {}", e))
    })?;
    if !canonical_path.starts_with(&canonical_base) {
        return Err(ToolError::NotAuthorized(
            "Path traversal outside vault is not allowed".to_string(),
        ));
    }
    Ok(canonical_path)
}

// ============================================================
// VaultReadTool — Read a file from the Obsidian vault
// ============================================================

/// Read a markdown file from the Obsidian vault on disk.
pub struct VaultReadTool;

#[async_trait]
impl Tool for VaultReadTool {
    fn name(&self) -> &str {
        "vault_read"
    }

    fn description(&self) -> &str {
        "Read a file from the Obsidian vault (life system). \
         Provide a relative path like 'Work/Companies/Companies.md' or 'Health/Health.md'. \
         Returns the file contents as text."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path within the vault, e.g. 'Work/Companies/MindSpace/MindSpace.md'"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let rel_path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'path' parameter".to_string()))?;

        let base = vault_base_path();
        let full_path = base.join(rel_path);

        // Validate path is within vault (must exist for canonicalize)
        if !full_path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "File not found: {}",
                rel_path
            )));
        }
        let safe_path = validate_vault_path(&full_path, &base)?;

        // Read the file
        let content = tokio::fs::read_to_string(&safe_path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read '{}': {}", rel_path, e))
        })?;

        Ok(ToolOutput::success(
            serde_json::json!({
                "path": rel_path,
                "content": content,
                "size_bytes": content.len()
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false // Local filesystem, trusted data
    }
}

// ============================================================
// VaultWriteTool — Write a file to the Obsidian vault
// ============================================================

/// Write or update a markdown file in the Obsidian vault on disk.
pub struct VaultWriteTool;

#[async_trait]
impl Tool for VaultWriteTool {
    fn name(&self) -> &str {
        "vault_write"
    }

    fn description(&self) -> &str {
        "Write or update a file in the Obsidian vault (life system). \
         Provide a relative path and content. Creates parent directories if needed. \
         Use mode 'overwrite' to replace or 'append' to add to existing content."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path within the vault, e.g. 'Health/Exercises/2026-02-10.md'"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write"
                },
                "mode": {
                    "type": "string",
                    "enum": ["overwrite", "append"],
                    "description": "Write mode: 'overwrite' replaces the file, 'append' adds to it. Default: overwrite",
                    "default": "overwrite"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let rel_path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'path' parameter".to_string()))?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParameters("missing 'content' parameter".to_string())
            })?;

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("overwrite");

        // Prevent writing to hidden/system directories
        if rel_path.starts_with('.') || rel_path.contains("/.") || rel_path.contains("\\.") {
            return Err(ToolError::NotAuthorized(
                "Cannot write to hidden directories (e.g. .ironclad, .obsidian)".to_string(),
            ));
        }

        let base = vault_base_path();
        let full_path = base.join(rel_path);

        // Create parent directories if needed
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to create directories: {}", e))
            })?;
        }

        // Write or append
        let final_content = match mode {
            "append" => {
                let existing = if full_path.exists() {
                    tokio::fs::read_to_string(&full_path).await.unwrap_or_default()
                } else {
                    String::new()
                };
                if existing.is_empty() {
                    content.to_string()
                } else {
                    format!("{}\n{}", existing, content)
                }
            }
            _ => content.to_string(),
        };

        tokio::fs::write(&full_path, &final_content).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to write '{}': {}", rel_path, e))
        })?;

        // Validate the written path is within vault
        validate_vault_path(&full_path, &base)?;

        Ok(ToolOutput::success(
            serde_json::json!({
                "path": rel_path,
                "mode": mode,
                "size_bytes": final_content.len(),
                "status": "written"
            }),
            start.elapsed(),
        ))
    }

    fn requires_approval(&self) -> bool {
        true // Writing to filesystem requires user approval
    }

    fn requires_sanitization(&self) -> bool {
        false // Local filesystem, trusted operation
    }
}

// ============================================================
// VaultListTool — List files in the Obsidian vault
// ============================================================

/// List files and directories in the Obsidian vault.
pub struct VaultListTool;

#[async_trait]
impl Tool for VaultListTool {
    fn name(&self) -> &str {
        "vault_list"
    }

    fn description(&self) -> &str {
        "List files and directories in the Obsidian vault (life system). \
         Provide a relative directory path or empty string for root. \
         Returns immediate children with type (file/directory) and size."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative directory path within the vault, e.g. 'Work/Companies' or '' for root",
                    "default": ""
                }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let rel_path = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let base = vault_base_path();
        let full_path = if rel_path.is_empty() {
            base.clone()
        } else {
            base.join(rel_path)
        };

        if !full_path.exists() || !full_path.is_dir() {
            return Err(ToolError::ExecutionFailed(format!(
                "Directory not found: {}",
                rel_path
            )));
        }

        // Validate path is within vault
        validate_vault_path(&full_path, &base)?;

        // Read directory entries
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&full_path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read directory: {}", e))
        })?;

        while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read entry: {}", e))
        })? {
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files/directories
            if name.starts_with('.') {
                continue;
            }

            let metadata = entry.metadata().await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to read metadata: {}", e))
            })?;

            entries.push(serde_json::json!({
                "name": name,
                "type": if metadata.is_dir() { "directory" } else { "file" },
                "size_bytes": if metadata.is_file() { metadata.len() } else { 0 }
            }));
        }

        // Sort: directories first, then alphabetically
        entries.sort_by(|a, b| {
            let a_type = a.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let b_type = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            b_type.cmp(a_type).then(a_name.cmp(b_name))
        });

        Ok(ToolOutput::success(
            serde_json::json!({
                "path": if rel_path.is_empty() { "/" } else { rel_path },
                "entries": entries,
                "count": entries.len()
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false // Local filesystem, trusted data
    }
}

// ============================================================
// VaultSearchTool — Grep across the Obsidian vault
// ============================================================

/// Walk the vault recursively, find markdown files whose content contains
/// the query (case-insensitive), and return file paths + a snippet around
/// each first match. Capped at 20 results so the LLM context stays sane.
pub struct VaultSearchTool;

#[async_trait]
impl Tool for VaultSearchTool {
    fn name(&self) -> &str {
        "vault_search"
    }

    fn description(&self) -> &str {
        "Search the Obsidian vault for markdown notes containing a query \
         string. Case-insensitive contains-match across file contents AND \
         filenames. Returns up to 20 hits with a short snippet showing where \
         the match was found. Use this when McKale asks 'what do I have on X' \
         or 'find my note about Y' — it's faster than vault_list when you \
         don't already know the folder."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring to search for. Case-insensitive. E.g., 'Eustress', 'butter bear', 'taxes 2026'."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of hits to return. Default 20, hard cap 50.",
                    "default": 20,
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'query' parameter".into()))?
            .trim();
        if query.is_empty() {
            return Err(ToolError::InvalidParameters("'query' is empty".into()));
        }
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(50) as usize;

        let base = vault_base_path();
        if !base.is_dir() {
            return Err(ToolError::ExecutionFailed(format!(
                "Vault root missing: {}",
                base.display()
            )));
        }

        // Tokenize the query. Whole-string substring matching used to be
        // the algorithm here, which meant "UACI incubator Eustress draft"
        // never found a file titled "UACI Incubator Application Eustress
        // Engine" because the exact phrase isn't in the file. JARVIS would
        // then truthfully report "no results" and the user would assume
        // the file didn't exist. Real bug.
        //
        // New behavior: split the query into tokens, drop stopwords, and
        // require every remaining token to appear (in filename OR body).
        // Score by how many distinct tokens matched, sort descending.
        const STOPWORDS: &[&str] = &[
            "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into",
            "is", "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then",
            "there", "these", "they", "this", "to", "was", "will", "with",
        ];
        let query_lc = query.to_lowercase();
        let tokens: Vec<String> = query_lc
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty() && !STOPWORDS.contains(t))
            .map(String::from)
            .collect();
        // Fall back to the raw query if every token was a stopword.
        let effective_tokens: Vec<String> = if tokens.is_empty() {
            vec![query_lc.clone()]
        } else {
            tokens
        };

        struct ScoredHit {
            path: String,
            tokens_matched: usize,
            snippet: String,
        }

        let mut scored: Vec<ScoredHit> = Vec::new();
        let mut visited_files = 0usize;
        let mut stack: Vec<PathBuf> = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                // Skip dotdirs (.obsidian, .ironclad, .git, etc.)
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !meta.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                visited_files += 1;
                let rel = path.strip_prefix(&base).unwrap_or(&path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let name_lc = name.to_lowercase();

                // Read body once (the OS cache makes this cheap on repeats).
                let body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                let body_lc = body.to_lowercase();
                let haystack = format!("{} {}", name_lc, body_lc);

                let mut tokens_matched = 0;
                let mut first_body_hit_idx: Option<usize> = None;
                for tok in &effective_tokens {
                    if haystack.contains(tok) {
                        tokens_matched += 1;
                        if first_body_hit_idx.is_none() {
                            if let Some(i) = body_lc.find(tok) {
                                first_body_hit_idx = Some(i);
                            }
                        }
                    }
                }

                if tokens_matched < effective_tokens.len() {
                    continue;
                }

                // Snippet: prefer a body excerpt around the first hit; if
                // every token only matched in the filename, surface that.
                let snippet = if let Some(idx) = first_body_hit_idx {
                    let lo = idx.saturating_sub(60);
                    let hi = (idx + 120).min(body.len());
                    let raw = &body[lo..hi];
                    raw.chars()
                        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    format!("(filename match) {}", rel_str)
                };

                scored.push(ScoredHit {
                    path: rel_str,
                    tokens_matched,
                    snippet,
                });
            }
        }

        // Rank: most tokens matched first. Within equal scores, shorter
        // paths win (canonical files tend to live higher in the tree).
        scored.sort_by(|a, b| {
            b.tokens_matched
                .cmp(&a.tokens_matched)
                .then_with(|| a.path.len().cmp(&b.path.len()))
        });
        scored.truncate(limit);

        let hits: Vec<serde_json::Value> = scored
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "snippet": h.snippet,
                    "tokens_matched": h.tokens_matched,
                })
            })
            .collect();

        Ok(ToolOutput::success(
            serde_json::json!({
                "query": query,
                "tokens": effective_tokens,
                "hits": hits,
                "count": hits.len(),
                "files_scanned": visited_files,
                "truncated": hits.len() >= limit,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// VaultDeleteTool — Remove a file from the Obsidian vault
// ============================================================

/// Delete a file from the Obsidian vault. Approval-gated. Refuses to touch
/// hidden directories or the JARVIS identity/system files.
pub struct VaultDeleteTool;

#[async_trait]
impl Tool for VaultDeleteTool {
    fn name(&self) -> &str {
        "vault_delete"
    }

    fn description(&self) -> &str {
        "Delete a file from the Obsidian vault. Required when finishing a \
         move (vault_read source → vault_write destination → vault_delete \
         source) so the file doesn't get duplicated. Approval-gated; refuses \
         hidden / system paths."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path within the vault, e.g. '00 System/JARVIS/Old_Draft.md'"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let rel_path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'path' parameter".to_string()))?;

        if rel_path.starts_with('.') || rel_path.contains("/.") || rel_path.contains("\\.") {
            return Err(ToolError::NotAuthorized(
                "Cannot delete from hidden directories (e.g. .ironclad, .obsidian)".to_string(),
            ));
        }
        // Identity files own the agent's behavior — never let a delete reach them.
        let lower = rel_path.to_ascii_lowercase();
        for protected in [
            "agents.md",
            "soul.md",
            "identity.md",
            "user.md",
            "claude.md",
            "memory.md",
            "heartbeat.md",
            "readme.md",
        ] {
            if lower.ends_with(protected) {
                return Err(ToolError::NotAuthorized(format!(
                    "Refusing to delete protected identity/system file: {}",
                    rel_path
                )));
            }
        }

        let base = vault_base_path();
        let full_path = base.join(rel_path);
        if !full_path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "File does not exist: {}",
                rel_path
            )));
        }
        validate_vault_path(&full_path, &base)?;

        if full_path.is_dir() {
            return Err(ToolError::NotAuthorized(
                "vault_delete only deletes files, not directories. Use shell with explicit approval if a directory delete is required.".to_string(),
            ));
        }

        tokio::fs::remove_file(&full_path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to delete '{}': {}", rel_path, e))
        })?;

        Ok(ToolOutput::success(
            serde_json::json!({
                "path": rel_path,
                "status": "deleted"
            }),
            start.elapsed(),
        ))
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// VaultMoveTool — Atomically move/rename a file in the vault
// ============================================================

/// Move (or rename) a vault file. Implemented as a filesystem-level rename
/// when source and destination are on the same volume, falling back to a
/// copy + delete otherwise. Single approval covers the whole move (versus
/// three separate approvals if the agent chained read/write/delete).
pub struct VaultMoveTool;

#[async_trait]
impl Tool for VaultMoveTool {
    fn name(&self) -> &str {
        "vault_move"
    }

    fn description(&self) -> &str {
        "Move or rename a file within the Obsidian vault. Atomic at the \
         filesystem level when possible; copy+delete fallback otherwise. \
         Single approval covers the whole move. Use this instead of \
         vault_read + vault_write + vault_delete for relocations."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "string",
                    "description": "Source path relative to the vault root"
                },
                "to": {
                    "type": "string",
                    "description": "Destination path relative to the vault root"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "If true, allow overwriting an existing destination file. Default: false",
                    "default": false
                }
            },
            "required": ["from", "to"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let from = params
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'from' parameter".to_string()))?;
        let to = params
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'to' parameter".to_string()))?;
        let overwrite = params
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        for p in [from, to] {
            if p.starts_with('.') || p.contains("/.") || p.contains("\\.") {
                return Err(ToolError::NotAuthorized(
                    "Cannot move into or out of hidden directories".to_string(),
                ));
            }
        }

        let base = vault_base_path();
        let src = base.join(from);
        let dst = base.join(to);

        if !src.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "Source does not exist: {}",
                from
            )));
        }
        if src.is_dir() {
            return Err(ToolError::NotAuthorized(
                "vault_move only moves files, not directories".to_string(),
            ));
        }
        if dst.exists() && !overwrite {
            return Err(ToolError::ExecutionFailed(format!(
                "Destination already exists (set overwrite=true to replace): {}",
                to
            )));
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to create destination directory: {}", e))
            })?;
        }

        // Try a fast rename first; fall back to copy + delete if rename
        // fails (cross-volume, locked, etc.).
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => {}
            Err(_) => {
                tokio::fs::copy(&src, &dst).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Copy fallback failed: {}", e))
                })?;
                tokio::fs::remove_file(&src).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!(
                        "Copy succeeded but source delete failed (file is now duplicated at {}): {}",
                        to, e
                    ))
                })?;
            }
        }

        validate_vault_path(&dst, &base)?;

        let size = tokio::fs::metadata(&dst)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(ToolOutput::success(
            serde_json::json!({
                "from": from,
                "to": to,
                "size_bytes": size,
                "status": "moved"
            }),
            start.elapsed(),
        ))
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_base_path_default() {
        // Without env var, should return the default path
        let path = vault_base_path();
        assert!(path.to_str().unwrap().contains("Olson"));
    }
}
