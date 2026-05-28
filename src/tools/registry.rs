//! Tool registry for managing available tools.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::context::ContextManager;
use crate::extensions::ExtensionManager;
use crate::llm::{LlmProvider, ToolDefinition};
use crate::safety::SafetyLayer;
use crate::tools::builder::{BuildSoftwareTool, BuilderConfig, LlmSoftwareBuilder};
use crate::tools::builtin::{
    ApplyPatchTool, CancelJobTool, CreateJobTool, EchoTool, GithubGetPrTool, GithubListIssuesTool,
    GithubListPrsTool, GithubListReposTool, GithubRecentCommitsTool, HttpTool, JobStatusTool,
    JsonTool, ListDirTool, ListJobsTool, MemoryReadTool, MemorySearchTool, MemoryTreeTool,
    MemoryWriteTool, OpenAppTool, OpenUrlTool, ReadFileTool, ShellTool, SpawnAgentTool, TimeTool,
    ToolActivateTool, ToolAuthTool, ToolInstallTool, ToolListTool, ToolRemoveTool, ToolSearchTool,
    ListMyToolsTool, MissionLookupTool, VaultDeleteTool, VaultListTool, VaultMoveTool,
    VaultReadTool, VaultSearchTool, VaultWriteTool, WriteFileTool,
};
use crate::tools::tool::Tool;
use crate::tools::wasm::{
    Capabilities, ResourceLimits, WasmError, WasmStorageError, WasmToolRuntime, WasmToolStore,
    WasmToolWrapper,
};
use crate::workspace::Workspace;

/// Registry of available tools.
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool.
    pub async fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().await.insert(name.clone(), tool);
        tracing::debug!("Registered tool: {}", name);
    }

    /// Register a tool (sync version for startup).
    pub fn register_sync(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if let Ok(mut tools) = self.tools.try_write() {
            tools.insert(name.clone(), tool);
            tracing::debug!("Registered tool: {}", name);
        }
    }

    /// Unregister a tool.
    pub async fn unregister(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.write().await.remove(name)
    }

    /// Get a tool by name.
    pub async fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().await.get(name).cloned()
    }

    /// Check if a tool exists.
    pub async fn has(&self, name: &str) -> bool {
        self.tools.read().await.contains_key(name)
    }

    /// List all tool names.
    pub async fn list(&self) -> Vec<String> {
        self.tools.read().await.keys().cloned().collect()
    }

    /// Get the number of registered tools.
    pub fn count(&self) -> usize {
        self.tools.try_read().map(|t| t.len()).unwrap_or(0)
    }

    /// Get all tools.
    pub async fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().await.values().cloned().collect()
    }

    /// Get tool definitions for LLM function calling.
    pub async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .await
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            })
            .collect()
    }

    /// Get tool definitions for specific tools.
    pub async fn tool_definitions_for(&self, names: &[&str]) -> Vec<ToolDefinition> {
        let tools = self.tools.read().await;
        names
            .iter()
            .filter_map(|name| tools.get(*name))
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            })
            .collect()
    }

    /// Register all built-in tools.
    pub fn register_builtin_tools(&self) {
        self.register_sync(Arc::new(EchoTool));
        self.register_sync(Arc::new(TimeTool));
        self.register_sync(Arc::new(JsonTool));
        self.register_sync(Arc::new(HttpTool::new()));
        // Alpha-2 hands: open URLs and launch desktop apps.
        self.register_sync(Arc::new(OpenUrlTool));
        self.register_sync(Arc::new(OpenAppTool));

        tracing::info!("Registered {} built-in tools", self.count());
    }

    /// Register development tools for building software.
    ///
    /// These tools provide shell access, file operations, and code editing
    /// capabilities needed for the software builder. Call this after
    /// `register_builtin_tools()` to enable code generation features.
    pub fn register_dev_tools(&self) {
        self.register_sync(Arc::new(ShellTool::new()));
        self.register_sync(Arc::new(ReadFileTool::new()));
        self.register_sync(Arc::new(WriteFileTool::new()));
        self.register_sync(Arc::new(ListDirTool::new()));
        self.register_sync(Arc::new(ApplyPatchTool::new()));

        tracing::info!("Registered 5 development tools");
    }

    /// Register memory tools with a workspace.
    ///
    /// Memory tools require a workspace for persistence. Call this after
    /// `register_builtin_tools()` if you have a workspace available.
    pub fn register_memory_tools(&self, workspace: Arc<Workspace>) {
        self.register_sync(Arc::new(MemorySearchTool::new(Arc::clone(&workspace))));
        self.register_sync(Arc::new(MemoryWriteTool::new(Arc::clone(&workspace))));
        self.register_sync(Arc::new(MemoryReadTool::new(Arc::clone(&workspace))));
        self.register_sync(Arc::new(MemoryTreeTool::new(workspace)));

        tracing::info!("Registered 4 memory tools");
    }

    /// Register job management tools.
    ///
    /// Job tools allow the LLM to create, list, check status, and cancel jobs.
    /// These enable natural language job management without hardcoded intent parsing.
    pub fn register_job_tools(&self, context_manager: Arc<ContextManager>) {
        self.register_sync(Arc::new(CreateJobTool::new(Arc::clone(&context_manager))));
        self.register_sync(Arc::new(ListJobsTool::new(Arc::clone(&context_manager))));
        self.register_sync(Arc::new(JobStatusTool::new(Arc::clone(&context_manager))));
        self.register_sync(Arc::new(CancelJobTool::new(context_manager)));

        tracing::info!("Registered 4 job management tools");
    }

    /// Register Obsidian vault bridge tools (read, write, list).
    ///
    /// These tools allow the agent to read and write files directly in the
    /// user's Obsidian vault on disk, bridging Iron Clad's workspace with
    /// the life system.
    pub fn register_vault_tools(&self) {
        self.register_sync(Arc::new(VaultReadTool));
        self.register_sync(Arc::new(VaultWriteTool));
        self.register_sync(Arc::new(VaultListTool));
        self.register_sync(Arc::new(VaultSearchTool));
        self.register_sync(Arc::new(VaultDeleteTool));
        self.register_sync(Arc::new(VaultMoveTool));

        tracing::info!("Registered 6 vault bridge tools");
    }

    /// Register `list_my_tools` so JARVIS can self-introspect when he
    /// loses track of which tools he has. Cheap, no side effects.
    pub fn register_tool_inventory_tool(self: &Arc<Self>) {
        self.register_sync(Arc::new(ListMyToolsTool::new(Arc::clone(self))));
        tracing::info!("Registered list_my_tools (self-introspection)");
    }

    /// Register `mission_lookup` so JARVIS can parse the curated
    /// `00 System/Workspaces.md` mission-to-profile mapping at the start
    /// of an "open mission workspace" flow.
    pub fn register_mission_lookup_tool(&self) {
        self.register_sync(Arc::new(MissionLookupTool));
        tracing::info!("Registered mission_lookup (Workspaces.md parser)");
    }

    /// Register Windows 11 Virtual Desktop tools. Reads (`list_desktops`)
    /// auto-approve; writes (`switch`, `new`, `move_window_to_desktop`) pop
    /// the approval banner since they visibly mutate the user's screen.
    /// Only registers on Windows; on other platforms the tools would
    /// fail with a "not supported" error so we skip them entirely.
    #[cfg(target_os = "windows")]
    pub fn register_windows_desktop_tools(&self) {
        use crate::tools::builtin::{
            WindowsListDesktopsTool, WindowsListMonitorsTool, WindowsMoveWindowToDesktopTool,
            WindowsNewDesktopTool, WindowsSnapWindowTool, WindowsSwitchDesktopTool,
        };
        self.register_sync(Arc::new(WindowsListDesktopsTool));
        self.register_sync(Arc::new(WindowsSwitchDesktopTool));
        self.register_sync(Arc::new(WindowsNewDesktopTool));
        self.register_sync(Arc::new(WindowsMoveWindowToDesktopTool));
        self.register_sync(Arc::new(WindowsListMonitorsTool));
        self.register_sync(Arc::new(WindowsSnapWindowTool));
        tracing::info!("Registered 6 Windows desktop + window tools");
    }

    #[cfg(not(target_os = "windows"))]
    pub fn register_windows_desktop_tools(&self) {
        // No-op on non-Windows platforms.
    }

    /// Register the sub-agent dispatcher (`spawn_agent`).
    ///
    /// The tool holds:
    /// - An Arc back-reference to this registry to resolve `tool_names`
    ///   into `Arc<dyn Tool>` at spawn time.
    /// - Sonnet + Opus `LlmProvider`s pre-instantiated.
    /// - The ChannelManager so the BACKGROUND sub-agent task can broadcast
    ///   its completion summary back as an unsolicited assistant message
    ///   (fire-and-forget — the main agent loop doesn't block on it).
    ///
    /// Must be called AFTER the ChannelManager is built and the gateway
    /// channel is registered, otherwise the broadcast at completion lands
    /// in a void.
    pub fn register_spawn_agent_tool(
        self: &Arc<Self>,
        sonnet: Arc<dyn LlmProvider>,
        opus: Arc<dyn LlmProvider>,
        channels: Arc<crate::channels::ChannelManager>,
        context_manager: Arc<crate::context::ContextManager>,
    ) {
        let tool =
            SpawnAgentTool::new(Arc::clone(self), sonnet, opus, channels, context_manager);
        self.register_sync(Arc::new(tool));
        tracing::info!(
            "Registered spawn_agent (Sonnet + Opus sub-agents, fire-and-forget, job-tracked)"
        );
    }

    /// Register native GitHub tools. Read-only, auto-approved, scoped to
    /// the `GITHUB_DEFAULT_ORG` from `.env` unless the LLM overrides.
    /// Skipped silently if `GITHUB_PAT` isn't set.
    pub fn register_github_tools(&self) {
        if std::env::var("GITHUB_PAT")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            tracing::info!("GITHUB_PAT not set; skipping github_* tool registration");
            return;
        }
        self.register_sync(Arc::new(GithubListReposTool));
        self.register_sync(Arc::new(GithubListPrsTool));
        self.register_sync(Arc::new(GithubGetPrTool));
        self.register_sync(Arc::new(GithubListIssuesTool));
        self.register_sync(Arc::new(GithubRecentCommitsTool));
        tracing::info!("Registered 5 GitHub native tools");
    }

    /// Register extension management tools (search, install, auth, activate, list, remove).
    ///
    /// These allow the LLM to manage MCP servers and WASM tools through conversation.
    pub fn register_extension_tools(&self, manager: Arc<ExtensionManager>) {
        self.register_sync(Arc::new(ToolSearchTool::new(Arc::clone(&manager))));
        self.register_sync(Arc::new(ToolInstallTool::new(Arc::clone(&manager))));
        self.register_sync(Arc::new(ToolAuthTool::new(Arc::clone(&manager))));
        self.register_sync(Arc::new(ToolActivateTool::new(Arc::clone(&manager))));
        self.register_sync(Arc::new(ToolListTool::new(Arc::clone(&manager))));
        self.register_sync(Arc::new(ToolRemoveTool::new(manager)));
        tracing::info!("Registered 6 extension management tools");
    }

    /// Register the software builder tool.
    ///
    /// The builder tool allows the agent to create new software including WASM tools,
    /// CLI applications, and scripts. It uses an LLM-driven iterative build loop.
    ///
    /// This also registers the dev tools (shell, file operations) needed by the builder.
    pub async fn register_builder_tool(
        self: &Arc<Self>,
        llm: Arc<dyn LlmProvider>,
        safety: Arc<SafetyLayer>,
        config: Option<BuilderConfig>,
    ) {
        // First register dev tools needed by the builder
        self.register_dev_tools();

        // Create the builder (arg order: config, llm, safety, tools)
        let builder = Arc::new(LlmSoftwareBuilder::new(
            config.unwrap_or_default(),
            llm,
            safety,
            Arc::clone(self),
        ));

        // Register the build_software tool
        self.register(Arc::new(BuildSoftwareTool::new(builder)))
            .await;

        tracing::info!("Registered software builder tool");
    }

    /// Register a WASM tool from bytes.
    ///
    /// This validates and compiles the WASM component, then registers it as a tool.
    /// The tool will be executed in a sandboxed environment with the given capabilities.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let runtime = Arc::new(WasmToolRuntime::new(WasmRuntimeConfig::default())?);
    /// let wasm_bytes = std::fs::read("my_tool.wasm")?;
    ///
    /// registry.register_wasm(WasmToolRegistration {
    ///     name: "my_tool",
    ///     wasm_bytes: &wasm_bytes,
    ///     runtime: &runtime,
    ///     description: Some("My custom tool description"),
    ///     ..Default::default()
    /// }).await?;
    /// ```
    pub async fn register_wasm(&self, reg: WasmToolRegistration<'_>) -> Result<(), WasmError> {
        // Prepare the module (validates and compiles)
        let prepared = reg
            .runtime
            .prepare(reg.name, reg.wasm_bytes, reg.limits)
            .await?;

        // Create the wrapper
        let mut wrapper = WasmToolWrapper::new(Arc::clone(reg.runtime), prepared, reg.capabilities);

        // Apply overrides if provided
        if let Some(desc) = reg.description {
            wrapper = wrapper.with_description(desc);
        }
        if let Some(s) = reg.schema {
            wrapper = wrapper.with_schema(s);
        }

        // Register the tool
        self.register(Arc::new(wrapper)).await;

        tracing::info!(name = reg.name, "Registered WASM tool");
        Ok(())
    }

    /// Register a WASM tool from database storage.
    ///
    /// Loads the WASM binary with integrity verification and configures capabilities.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let store = FjallWasmToolStore::open("~/.ironclad/wasm-index")?;
    /// let runtime = Arc::new(WasmToolRuntime::new(WasmRuntimeConfig::default())?);
    ///
    /// registry.register_wasm_from_storage(
    ///     &store,
    ///     &runtime,
    ///     "user_123",
    ///     "my_tool",
    /// ).await?;
    /// ```
    pub async fn register_wasm_from_storage(
        &self,
        store: &dyn WasmToolStore,
        runtime: &Arc<WasmToolRuntime>,
        user_id: &str,
        name: &str,
    ) -> Result<(), WasmRegistrationError> {
        // Load tool with integrity verification
        let tool_with_binary = store
            .get_with_binary(user_id, name)
            .await
            .map_err(WasmRegistrationError::Storage)?;

        // Load capabilities
        let stored_caps = store
            .get_capabilities(tool_with_binary.tool.id)
            .await
            .map_err(WasmRegistrationError::Storage)?;

        let capabilities = stored_caps.map(|c| c.to_capabilities()).unwrap_or_default();

        // Register the tool
        self.register_wasm(WasmToolRegistration {
            name: &tool_with_binary.tool.name,
            wasm_bytes: &tool_with_binary.wasm_binary,
            runtime,
            capabilities,
            limits: None,
            description: Some(&tool_with_binary.tool.description),
            schema: Some(tool_with_binary.tool.parameters_schema.clone()),
        })
        .await
        .map_err(WasmRegistrationError::Wasm)?;

        tracing::info!(
            name = tool_with_binary.tool.name,
            user_id = user_id,
            trust_level = %tool_with_binary.tool.trust_level,
            "Registered WASM tool from storage"
        );

        Ok(())
    }
}

/// Error when registering a WASM tool from storage.
#[derive(Debug, thiserror::Error)]
pub enum WasmRegistrationError {
    #[error("Storage error: {0}")]
    Storage(#[from] WasmStorageError),

    #[error("WASM error: {0}")]
    Wasm(#[from] WasmError),
}

/// Configuration for registering a WASM tool.
pub struct WasmToolRegistration<'a> {
    /// Unique name for the tool.
    pub name: &'a str,
    /// Raw WASM component bytes.
    pub wasm_bytes: &'a [u8],
    /// WASM runtime for compilation and execution.
    pub runtime: &'a Arc<WasmToolRuntime>,
    /// Security capabilities to grant the tool.
    pub capabilities: Capabilities,
    /// Optional resource limits (uses defaults if None).
    pub limits: Option<ResourceLimits>,
    /// Optional description override.
    pub description: Option<&'a str>,
    /// Optional parameter schema override.
    pub schema: Option<serde_json::Value>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool::EchoTool;

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).await;

        assert!(registry.has("echo").await);
        assert!(registry.get("echo").await.is_some());
        assert!(registry.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_list_tools() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).await;

        let tools = registry.list().await;
        assert!(tools.contains(&"echo".to_string()));
    }

    #[tokio::test]
    async fn test_tool_definitions() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).await;

        let defs = registry.tool_definitions().await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }
}
