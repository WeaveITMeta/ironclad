//! OS-control: launch apps and open URLs from the orchestrator.
//!
//! Alpha-2 ("App + page control") needs JARVIS to be able to do
//! "open the Eustress repo" → browser opens the URL, and
//! "open Obsidian" → the Windows app launches. Two narrow tools, no GUI
//! automation here — page-level automation is delegated to Playwright MCP.

use async_trait::async_trait;
use serde_json::json;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Open a URL in the user's default browser. Fires and forgets.
pub struct OpenUrlTool;

#[async_trait]
impl Tool for OpenUrlTool {
    fn name(&self) -> &str {
        "open_url"
    }

    fn description(&self) -> &str {
        "Open a URL in the user's default browser. Returns immediately after launching."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Full URL to open, e.g. https://github.com/WeaveITMeta/Eustress"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'url' parameter".to_string()))?;

        open::that(url).map_err(|e| {
            ToolError::ExecutionFailed(format!("open_url: failed to open '{url}': {e}"))
        })?;

        Ok(ToolOutput::text(
            format!("opened {url} in default browser"),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

/// Launch a desktop app by name or full path.
///
/// On Windows, resolves the name via App Paths (so "code", "obsidian",
/// "chrome" work without absolute paths). On other platforms, falls back to
/// invoking the name as a command.
pub struct OpenAppTool;

#[async_trait]
impl Tool for OpenAppTool {
    fn name(&self) -> &str {
        "open_app"
    }

    fn description(&self) -> &str {
        "Launch a desktop application by short name (e.g. 'code', 'obsidian', 'chrome') or \
         full executable path. On Windows uses the App Paths registry so short names work \
         without absolute paths."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "app": {
                    "type": "string",
                    "description": "App short name (code, obsidian, chrome, ...) or absolute path"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional CLI arguments to pass to the app"
                }
            },
            "required": ["app"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();
        let app = params
            .get("app")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'app' parameter".to_string()))?;
        let extra_args: Vec<String> = params
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        #[cfg(windows)]
        {
            // `start` is a cmd builtin; the empty string is the window title
            // placeholder so positional args parse correctly.
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/c").arg("start").arg("").arg(app);
            for a in &extra_args {
                cmd.arg(a);
            }
            cmd.spawn().map_err(|e| {
                ToolError::ExecutionFailed(format!("open_app: failed to launch '{app}': {e}"))
            })?;
        }
        #[cfg(not(windows))]
        {
            let mut cmd = tokio::process::Command::new(app);
            for a in &extra_args {
                cmd.arg(a);
            }
            cmd.spawn().map_err(|e| {
                ToolError::ExecutionFailed(format!("open_app: failed to launch '{app}': {e}"))
            })?;
        }

        Ok(ToolOutput::text(
            format!("launched {app}"),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}
