//! Obsidian vault bridge tools.
//!
//! Table of Contents:
//! - VaultReadTool: Read files from the Obsidian vault on disk
//! - VaultWriteTool: Write/update files in the Obsidian vault on disk
//! - VaultListTool: List files and directories in the Obsidian vault
//!
//! These tools bridge IronClaw's workspace (PostgreSQL) with the user's
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
                "Cannot write to hidden directories (e.g. .ironclaw, .obsidian)".to_string(),
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
