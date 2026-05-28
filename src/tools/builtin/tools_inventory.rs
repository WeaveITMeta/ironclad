//! Tool inventory generator + `list_my_tools` tool.
//!
//! At ~200 registered tools, the LLM can't reliably scan the full schema
//! list on every turn. The generator composes a compact categorized
//! summary that lives in the system prompt (via TOOLS.md, refreshed at
//! boot) and that JARVIS can also query mid-conversation through the
//! `list_my_tools` tool when he needs to remember what's possible.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::registry::ToolRegistry;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Walk the registry and produce a categorized inventory string. Native
/// tools are grouped by family. Anything that doesn't match a native
/// family is treated as an MCP namespace / router; we surface its name
/// plus a one-line excerpt of its description (which for namespace
/// routers contains the sub-tool list).
pub async fn generate_inventory(registry: &ToolRegistry) -> String {
    let names = registry.list().await;
    let mut native: Vec<String> = Vec::new();
    // After namespace-router install, MCP-shaped entries are typically one
    // router per server (e.g. `eustress`, `playwright_marketing`). Group
    // them by their full registered name so each gets its own line in the
    // inventory.
    let mut mcp_routers: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();

    let all_tools = registry.all().await;
    for tool in all_tools {
        let name = tool.name().to_string();
        if is_native_tool(&name) {
            native.push(name);
        } else {
            mcp_routers.insert(name, tool);
        }
    }
    drop(names); // unused after the all_tools fetch; just keep it cheap

    // Family rollup for native tools. These prefixes / exact names live
    // close to the registry definitions in tools/builtin.
    let families: &[(&str, &[&str], &str)] = &[
        ("Voice / general", &["echo", "time", "json", "http", "open_url", "open_app"], ""),
        ("Memory", &[], "memory_"),
        ("Vault", &[], "vault_"),
        ("GitHub", &[], "github_"),
        ("Dev", &["shell", "read_file", "write_file", "list_dir", "apply_patch"], ""),
        ("Builder", &["build_software"], ""),
        ("Jobs", &["create_job", "list_jobs", "job_status", "cancel_job"], ""),
        ("Sub-agents", &["spawn_agent"], ""),
        ("Tool discovery", &["list_my_tools"], ""),
        ("Windows desktops", &[], "windows_"),
        ("Extensions", &[], "tool_"),
    ];

    let mut out = String::new();
    out.push_str("Auto-generated at boot from the live registry. Use this as your tool map.\n\n");

    out.push_str("### Native\n");
    for (label, exact, prefix) in families {
        let mut hits: Vec<&String> = Vec::new();
        for n in &native {
            let exact_match = exact.iter().any(|e| n == e);
            let prefix_match = !prefix.is_empty() && n.starts_with(prefix);
            if exact_match || prefix_match {
                hits.push(n);
            }
        }
        if hits.is_empty() {
            continue;
        }
        hits.sort();
        out.push_str(&format!(
            "- **{}** ({}): {}\n",
            label,
            hits.len(),
            hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    // Any native tool that didn't slot into a family — surface so it
    // doesn't become invisible.
    let categorized: std::collections::HashSet<&String> = families
        .iter()
        .flat_map(|(_, exact, prefix)| {
            native.iter().filter(move |n| {
                let e = exact.iter().any(|e| n.as_str() == *e);
                let p = !prefix.is_empty() && n.starts_with(*prefix);
                e || p
            })
        })
        .collect();
    let mut uncategorized: Vec<&String> =
        native.iter().filter(|n| !categorized.contains(n)).collect();
    if !uncategorized.is_empty() {
        uncategorized.sort();
        out.push_str(&format!(
            "- **Other** ({}): {}\n",
            uncategorized.len(),
            uncategorized
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !mcp_routers.is_empty() {
        out.push_str("\n### MCP namespaces (call via router)\n");
        out.push_str(
            "Each entry is a single router tool; pass `{tool: \"<sub>\", args: {...}}` to dispatch.\n",
        );
        for (name, tool) in &mcp_routers {
            // First-sentence excerpt of the router's description; that's
            // where the sub-tool list lives for namespace routers.
            let desc = tool.description();
            let snippet = first_sentences(desc, 2);
            out.push_str(&format!("- **{}**: {}\n", name, snippet));
        }
    }

    out.push_str(
        "\n### Calling sub-agents\nUse `spawn_agent` with glob patterns in `tools` to scope an agent's capabilities. \
         Examples: `\"playwright_marketing_*\"` for marketing-Gmail browser only, `\"eustress_*\"` for the full sim engine, \
         `\"vault_*\"` for the full vault toolkit.\n",
    );

    out
}

/// Is this tool one of the natives shipped with Iron Clad? Combines an
/// explicit allowlist (so single-word names like `shell` or `echo` are
/// not mistaken for MCP namespaces) with prefix matching for the families
/// that use a shared prefix (`memory_*`, `vault_*`, etc).
fn is_native_tool(name: &str) -> bool {
    const NATIVE_EXACT: &[&str] = &[
        "echo",
        "time",
        "json",
        "http",
        "open_url",
        "open_app",
        "shell",
        "read_file",
        "write_file",
        "list_dir",
        "apply_patch",
        "build_software",
        "spawn_agent",
        "list_my_tools",
    ];
    const NATIVE_PREFIXES: &[&str] = &[
        "memory_",
        "vault_",
        "github_",
        "create_job",
        "list_jobs",
        "job_status",
        "cancel_job",
        "tool_",
        "windows_",
    ];
    NATIVE_EXACT.iter().any(|e| name == *e)
        || NATIVE_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Pull the first `n` sentences from a description string. Defensive
/// against descriptions written as one long paragraph — we cap at 240
/// chars regardless so a verbose tool can't blow up the inventory.
fn first_sentences(text: &str, n: usize) -> String {
    let mut count = 0;
    let mut end = text.len();
    for (i, c) in text.char_indices() {
        if c == '.' || c == '!' || c == '?' {
            count += 1;
            if count >= n {
                end = (i + 1).min(text.len());
                break;
            }
        }
    }
    let mut snippet = text[..end].trim().to_string();
    if snippet.len() > 240 {
        snippet.truncate(237);
        snippet.push_str("...");
    }
    snippet
}

/// LLM-callable tool that returns the inventory string. Useful when
/// JARVIS hits a point mid-conversation where he's forgotten what's
/// possible — one tool call surfaces the full map.
pub struct ListMyToolsTool {
    registry: Arc<ToolRegistry>,
}

impl ListMyToolsTool {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ListMyToolsTool {
    fn name(&self) -> &str {
        "list_my_tools"
    }

    fn description(&self) -> &str {
        "Return a categorized inventory of every tool currently registered \
         (native families + MCP namespaces with tool counts). Call this when \
         you need to remember what capabilities you have without scanning \
         the full tool list. Free, fast, no side effects."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let inventory = generate_inventory(&self.registry).await;
        Ok(ToolOutput::text(inventory, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}
