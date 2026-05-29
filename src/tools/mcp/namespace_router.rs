//! MCP namespace router.
//!
//! Problem: every MCP server we connect to (Playwright × 5, Eustress) adds
//! 20–75 tools to the registry. Each one's full JSON schema lands in the
//! Anthropic API call's `tools` array every turn. At ~200 tools across
//! all servers, that's roughly 40K tokens of schema bloat per turn before
//! the conversation even starts.
//!
//! Solution: register ONE router tool per MCP namespace. The router's
//! schema is small (a string tool name + an arbitrary args object). Its
//! description lists the sub-tool names so JARVIS knows what's behind it.
//! On call, the router unwraps `{tool, args}` and dispatches to the real
//! MCP tool by full prefixed name. Net effect: 5 Playwright router tools
//! + 1 Eustress router tool instead of 175 individual schemas. Roughly
//! 80–90% of the MCP schema bloat is gone, while JARVIS still has full
//! capability access.
//!
//! Trade-off: one extra layer of indirection in JARVIS's reasoning ("call
//! eustress with {tool: 'query_state', args: ...}" instead of just calling
//! `eustress_query_state`). The description of the router tool makes the
//! call shape explicit so this is one extra word for JARVIS to think.
//! Per-sub-tool input validation is deferred to the underlying MCP
//! server, which is where it lived anyway.

use std::sync::Arc;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::registry::ToolRegistry;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

const MAX_TOOLS_IN_DESCRIPTION: usize = 40;

/// A single MCP namespace exposed as one combined tool. Created at
/// registration time once the real per-tool wrappers are loaded.
pub struct McpNamespaceRouter {
    /// e.g. `playwright_marketing`, `eustress`.
    namespace: String,
    /// Cached at construction so it doesn't need to walk the registry on
    /// every `description()` call.
    description: String,
    /// Map of `<sub>` → Arc<dyn Tool>. The router holds direct references
    /// to the underlying wrapper tools; we unregister them from the
    /// registry after the router takes them, so the LLM sees only the
    /// router and we don't double-display.
    sub_tools: std::collections::BTreeMap<String, Arc<dyn Tool>>,
}

impl McpNamespaceRouter {
    /// Build a router for a namespace by taking ownership of the wrapper
    /// `Arc<dyn Tool>`s. The caller is expected to unregister these names
    /// from the registry after construction so the LLM only sees this
    /// router and not the per-tool surface.
    pub fn new(namespace: impl Into<String>, tools: Vec<Arc<dyn Tool>>) -> Self {
        let namespace = namespace.into();
        let prefix = format!("{}_", namespace);
        let mut sub_tools: std::collections::BTreeMap<String, Arc<dyn Tool>> =
            std::collections::BTreeMap::new();
        for t in tools {
            let full = t.name().to_string();
            if let Some(short) = full.strip_prefix(&prefix) {
                sub_tools.insert(short.to_string(), t);
            }
        }
        let names: Vec<&str> = sub_tools.keys().map(|s| s.as_str()).collect();
        let preview: Vec<&str> = names.iter().take(MAX_TOOLS_IN_DESCRIPTION).copied().collect();
        let tail = if names.len() > MAX_TOOLS_IN_DESCRIPTION {
            format!(", ... ({} sub-tools total)", names.len())
        } else {
            String::new()
        };
        let description = format!(
            "Combined access to the `{}` MCP namespace ({} sub-tools). \
             Call this tool with `tool` set to the sub-tool name and `args` \
             set to that sub-tool's parameters. Sub-tools: {}{}. \
             To learn a sub-tool's exact parameters, try a call and read the \
             validation error.",
            namespace,
            names.len(),
            preview.join(", "),
            tail
        );
        Self {
            namespace,
            description,
            sub_tools,
        }
    }

    /// Used by `install_namespace_routers` to decide whether to register
    /// the router at all — if a namespace only has 1 or 2 tools, the
    /// overhead of the router isn't worth the indirection.
    pub fn sub_tool_count(&self) -> usize {
        self.sub_tools.len()
    }
}

#[async_trait]
impl Tool for McpNamespaceRouter {
    fn name(&self) -> &str {
        &self.namespace
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": "Sub-tool name within this namespace (e.g. 'browser_navigate' or 'query_state'). Do NOT prefix with the namespace; the router prefixes for you."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments object passed straight through to the sub-tool. Shape depends on the sub-tool; see its server's docs or trigger a validation error to learn the schema.",
                    "additionalProperties": true
                }
            },
            "required": ["tool"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let tool = params
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParameters(format!(
                    "missing 'tool' in {} router call",
                    self.namespace
                ))
            })?;
        // Claude sometimes emits `"args": "{}"` (string) instead of
        // `"args": {}` (object) when calling namespace routers — a
        // known Anthropic tool-use serialization quirk that surfaces as
        // every Playwright call returning 400 Bad Request. Defensive
        // unwrap: if args is a JSON string, try to parse it back as an
        // object; if that fails the original string survives so a
        // sub-tool that legitimately wants a string parameter still
        // works.
        let args = match params.get("args") {
            None => serde_json::json!({}),
            Some(serde_json::Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    serde_json::json!({})
                } else if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    tracing::debug!(
                        "namespace_router[{}]: unwrapped string-wrapped args",
                        self.namespace
                    );
                    parsed
                } else {
                    serde_json::Value::String(s.clone())
                }
            }
            Some(other) => other.clone(),
        };

        let target = self.sub_tools.get(tool).ok_or_else(|| {
            // Hint with the closest matches if the model misspelled.
            let mut nearby: Vec<&str> = self
                .sub_tools
                .keys()
                .filter(|s| s.contains(tool) || tool.contains(s.as_str()))
                .map(|s| s.as_str())
                .take(5)
                .collect();
            if nearby.is_empty() {
                nearby = self.sub_tools.keys().map(|s| s.as_str()).take(5).collect();
            }
            ToolError::InvalidParameters(format!(
                "no sub-tool '{}' in {} namespace. Closest: {}",
                tool,
                self.namespace,
                nearby.join(", ")
            ))
        })?;

        target.execute(args, ctx).await
    }

    fn requires_approval(&self) -> bool {
        // Router itself is read-only metadata; per-sub-tool approval is
        // handled at the wrapper layer (the wrappers JARVIS calls THROUGH
        // the router still have their own `requires_approval()` returning
        // true for destructive ops, and the agent loop checks the
        // EXECUTED tool's approval flag at dispatch time, not the
        // router's). Routing a destructive op still pops the banner.
        false
    }

    fn requires_sanitization(&self) -> bool {
        // The router's output is whatever the sub-tool returned; sub-tool
        // wrappers already declare their own sanitization needs.
        true
    }
}

/// Walk the registry, group MCP-namespace tools, take their Arcs, build
/// a router per namespace, register the router, unregister the originals.
/// Threshold: only collapse namespaces with at least 3 sub-tools (below
/// that, the indirection costs more in confusion than it saves in
/// tokens).
pub async fn install_namespace_routers(
    registry: &Arc<ToolRegistry>,
    namespaces: &[&str],
) -> usize {
    let all_names = registry.list().await;
    let mut installed = 0usize;
    for ns in namespaces {
        let prefix = format!("{}_", ns);
        let matches: Vec<String> = all_names
            .iter()
            .filter(|n| n.starts_with(&prefix))
            .cloned()
            .collect();
        if matches.len() < 3 {
            continue;
        }
        // Take ownership of the wrapper Arcs by unregistering them now;
        // they live inside the router after this.
        let mut wrappers: Vec<Arc<dyn Tool>> = Vec::with_capacity(matches.len());
        for name in &matches {
            if let Some(tool) = registry.unregister(name).await {
                wrappers.push(tool);
            }
        }
        if wrappers.is_empty() {
            continue;
        }
        let router = McpNamespaceRouter::new((*ns).to_string(), wrappers);
        let count = router.sub_tool_count();
        registry.register(Arc::new(router)).await;
        tracing::info!(
            "Installed MCP namespace router '{}' (collapsed {} sub-tools into 1)",
            ns,
            count
        );
        installed += 1;
    }
    installed
}
