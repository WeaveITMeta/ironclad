//! Iron Clad - Main entry point.

use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use ironclad::{
    agent::{Agent, AgentDeps, SessionManager},
    channels::{
        ChannelManager, GatewayChannel, HttpChannel, ReplChannel, WebhookServer,
        WebhookServerConfig,
        wasm::{
            RegisteredEndpoint, SharedWasmChannel, WasmChannelLoader, WasmChannelRouter,
            WasmChannelRuntime, WasmChannelRuntimeConfig, create_wasm_channel_router,
        },
        web::log_layer::{LogBroadcaster, WebLogLayer},
    },
    cli::{
        Cli, Command, run_mcp_command, run_memory_command, run_status_command, run_tool_command,
    },
    config::Config,
    context::ContextManager,
    extensions::ExtensionManager,
    history::FjallHistoryStore as Store,
    llm::create_llm_provider,
    safety::SafetyLayer,
    secrets::{FjallSecretsStore, SecretsCrypto, SecretsStore},
    settings::Settings,
    setup::run_onboard_mode,
    tools::{
        ToolRegistry,
        mcp::{McpClient, McpSessionManager, config::load_mcp_servers, is_authenticated},
        wasm::{WasmToolLoader, WasmToolRuntime},
    },
    workspace::{
        EmbeddingProvider, FjallStore, FtsIndex, OpenAiEmbeddings, Repository, VectorStore,
        Workspace,
    },
};

/// Iron Clad's local data directory (`~/.ironclad`), created if missing.
fn ironclad_data_dir() -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".ironclad");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Open the embedded memory stores (Fjall KV + tantivy FTS + embedvec vectors),
/// shared by every workspace in the process. Replaces PostgreSQL for memory;
/// returns None only if the KV or FTS store fails to open.
async fn open_memory_repository(
    embeddings: &Option<Arc<dyn EmbeddingProvider>>,
) -> Option<Repository> {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".ironclad");
    let _ = std::fs::create_dir_all(&dir);

    let docs = match FjallStore::open(&dir.join("memory-index").to_string_lossy()) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Memory KV store open failed; memory disabled: {}", e);
            return None;
        }
    };
    let fts = match FtsIndex::open(&dir.join("fts-index").to_string_lossy()) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("FTS index open failed; memory disabled: {}", e);
            return None;
        }
    };
    let vectors = if let Some(emb) = embeddings {
        match VectorStore::open(&dir.join("vector-index").to_string_lossy(), emb.dimension()).await {
            Ok(vs) => Some(Arc::new(vs)),
            Err(e) => {
                tracing::warn!("Vector store open failed; semantic search disabled: {}", e);
                None
            }
        }
    } else {
        None
    };
    tracing::info!(
        "Memory stores ready: Fjall + tantivy{}",
        if vectors.is_some() { " + embedvec" } else { "" }
    );
    Some(Repository::new(Arc::new(docs), Arc::new(fts), vectors))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Handle non-agent commands first (they don't need full setup)
    match &cli.command {
        Some(Command::Tool(tool_cmd)) => {
            // Simple logging for CLI commands
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
                )
                .init();

            return run_tool_command(tool_cmd.clone()).await;
        }
        Some(Command::Config(config_cmd)) => {
            // Config commands don't need logging setup
            return ironclad::cli::run_config_command(config_cmd.clone())
                .map_err(|e| anyhow::anyhow!("{}", e));
        }
        Some(Command::Mcp(mcp_cmd)) => {
            // Simple logging for MCP commands
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
                )
                .init();

            return run_mcp_command(mcp_cmd.clone()).await;
        }
        Some(Command::Memory(mem_cmd)) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
                )
                .init();

            // Memory commands use the embedded stores (and optionally embeddings)
            let _ = dotenvy::dotenv();
            let config = Config::from_env().map_err(|e| anyhow::anyhow!("{}", e))?;

            // Set up embeddings if available (OpenAI only)
            let embeddings: Option<Arc<dyn ironclad::workspace::EmbeddingProvider>> =
                if config.embeddings.enabled {
                    if let Some(api_key) = config.embeddings.openai_api_key() {
                        let dim = match config.embeddings.model.as_str() {
                            "text-embedding-3-large" => 3072,
                            _ => 1536,
                        };
                        Some(Arc::new(ironclad::workspace::OpenAiEmbeddings::with_model(
                            api_key,
                            &config.embeddings.model,
                            dim,
                        )))
                    } else {
                        None
                    }
                } else {
                    None
                };

            let repository = open_memory_repository(&embeddings).await;
            return run_memory_command(mem_cmd.clone(), repository, embeddings).await;
        }
        Some(Command::Status) => {
            let _ = dotenvy::dotenv();
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
                )
                .init();

            return run_status_command().await;
        }
        Some(Command::Onboard {
            skip_auth: _,
            channels_only: _,
        }) => {
            // Onboarding now runs entirely in the Leptos dashboard. The legacy
            // CLI prompts are gone; this subcommand fires up the web-only
            // wizard backend and waits for the user to drive it from the
            // dashboard (or `cargo run-jarvis`).
            let _ = dotenvy::dotenv();

            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .init();

            let (host, port) = onboard_bind_addr();
            return run_onboard_mode(&host, port).await;
        }
        None | Some(Command::Run) => {
            // Continue to run agent
        }
    }

    // Load .env if present
    let _ = dotenvy::dotenv();

    // First-run check: if the user hasn't completed onboarding, run the web
    // wizard backend instead of the agent. The Leptos dashboard (served by
    // Trunk via `cargo run-jarvis`) hits these endpoints to drive setup; once
    // the user posts to `/api/onboard/complete`, they restart Iron Clad to
    // enter full mode. The legacy CLI wizard no longer exists.
    if !cli.no_onboard && !Settings::load().onboard_completed {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();

        let (host, port) = onboard_bind_addr();
        return run_onboard_mode(&host, port).await;
    }

    // Load configuration (after potential setup)
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(ironclad::error::ConfigError::MissingRequired { key, hint }) => {
            eprintln!("Configuration error: Missing required setting '{}'", key);
            eprintln!("  {}", hint);
            eprintln!();
            eprintln!(
                "Run 'ironclad onboard' to configure, or set the required environment variables."
            );
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    };

    // LLM auth is done via API key (ANTHROPIC_API_KEY). No session manager needed.

    // Initialize tracing. Default filter is verbose by design so the
    // terminal shows what Claude says, what tools fire, and the MCP
    // request/response chatter without the user having to set RUST_LOG.
    // Specific subsystems are bumped to DEBUG where the bulk of the
    // observability lives (MCP transport, tool execution, gateway HTTP).
    // Override with `RUST_LOG=ironclad=info` if you want it quieter.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "ironclad=debug,\
             ironclad::tools::mcp=debug,\
             ironclad::agent::agent_loop=info,\
             ironclad::llm::anthropic=debug,\
             tower_http=debug,\
             hyper=info,\
             reqwest=info",
        )
    });

    // Create log broadcaster before tracing init so the WebLogLayer can capture all events.
    // This gets wired to the gateway's /api/logs/events SSE endpoint later.
    let log_broadcaster = Arc::new(LogBroadcaster::new());

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(WebLogLayer::new(Arc::clone(&log_broadcaster)))
        .init();

    // Create CLI channel
    let repl_channel = if let Some(ref msg) = cli.message {
        Some(ReplChannel::with_message(msg.clone()))
    } else if config.channels.cli.enabled {
        Some(ReplChannel::new())
    } else {
        None
    };

    tracing::info!("Starting Iron Clad...");
    tracing::info!("Loaded configuration for agent: {}", config.agent.name);
    tracing::info!("LLM: Anthropic Claude API ({})", config.llm.anthropic.model);

    // Initialize the embedded history store (optional for testing)
    let store = if cli.no_db {
        tracing::warn!("Running without history store");
        None
    } else {
        let store = Store::open(&ironclad_data_dir().join("history-index").to_string_lossy())?;
        tracing::info!("History store (Fjall) ready");
        Some(Arc::new(store))
    };

    // Initialize LLM provider (Anthropic Claude API via ANTHROPIC_API_KEY)
    let llm = create_llm_provider(&config.llm)?;
    tracing::info!("LLM provider initialized: {}", llm.model_name());

    // Initialize safety layer
    let safety = Arc::new(SafetyLayer::new(&config.safety));
    tracing::info!("Safety layer initialized");

    // Initialize tool registry. `register_builtin_tools()` already emits a
    // "Registered N built-in tools" line, so don't double-log it here.
    let tools = Arc::new(ToolRegistry::new());
    tools.register_builtin_tools();

    // Create embeddings provider if configured (OpenAI only)
    let embeddings: Option<Arc<dyn EmbeddingProvider>> = if config.embeddings.enabled {
        if let Some(api_key) = config.embeddings.openai_api_key() {
            tracing::info!(
                "Embeddings enabled via OpenAI (model: {})",
                config.embeddings.model
            );
            Some(Arc::new(OpenAiEmbeddings::with_model(
                api_key,
                &config.embeddings.model,
                match config.embeddings.model.as_str() {
                    "text-embedding-3-large" => 3072,
                    _ => 1536, // text-embedding-3-small and ada-002
                },
            )))
        } else {
            tracing::warn!("Embeddings configured but OPENAI_API_KEY not set");
            None
        }
    } else {
        tracing::info!("Embeddings disabled (set OPENAI_API_KEY or EMBEDDING_ENABLED=true)");
        None
    };

    // Open the embedded memory stores (Fjall + tantivy + embedvec), shared by
    // every workspace. No database needed for memory.
    let repository = open_memory_repository(&embeddings).await;

    // Register memory tools when the memory stores are available.
    if let Some(ref repo) = repository {
        let mut workspace = Workspace::new("default", repo.clone());
        if let Some(ref emb) = embeddings {
            workspace = workspace.with_embeddings(emb.clone());
        }
        let workspace = Arc::new(workspace);

        // Bootstrap JARVIS identity files if missing. These get loaded into
        // every Claude system prompt by Workspace::system_prompt(), so they
        // set JARVIS's voice and behavior across all chats. Idempotent: only
        // writes when the path doesn't already exist.
        bootstrap_jarvis_identity(&workspace).await;

        tools.register_memory_tools(workspace);
        // Vault bridge: read / list / write tools scoped to VAULT_PATH.
        // Reads + lists are auto-approved (the LLM can recon the vault
        // freely); writes still pop the approval banner.
        tools.register_vault_tools();
    }
    // Native GitHub tools (no-op when GITHUB_PAT is unset).
    tools.register_github_tools();
    // Self-introspection so JARVIS can ask "what can I do?" mid-conversation.
    tools.register_tool_inventory_tool();
    // Workspaces.md parser for mission-driven desktop layouts.
    tools.register_mission_lookup_tool();
    // Native Win11 virtual-desktop control (list / switch / new / move-window)
    // plus monitor enumeration and snap-zone window placement. Reads
    // auto-approve; writes pop the banner.
    tools.register_windows_desktop_tools();
    // Self-diagnostic log query. Needs the LogBroadcaster which was
    // built before tracing init.
    tools.register_recent_logs_tool(Arc::clone(&log_broadcaster));

    // Build Sonnet + Opus providers up-front (cheap; just clones the
    // AnthropicConfig with model override). We hold them here and register
    // `spawn_agent` later, AFTER the ChannelManager is built, so the tool
    // can broadcast completion summaries back to McKale.
    let subagent_providers: Option<(
        Arc<dyn ironclad::llm::LlmProvider>,
        Arc<dyn ironclad::llm::LlmProvider>,
        Arc<dyn ironclad::llm::LlmProvider>,
    )> = {
        use ironclad::llm::AnthropicProvider;
        let mut haiku_cfg = config.llm.anthropic.clone();
        haiku_cfg.model = std::env::var("SUBAGENT_HAIKU_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5".to_string());
        let mut sonnet_cfg = config.llm.anthropic.clone();
        sonnet_cfg.model = std::env::var("SUBAGENT_SONNET_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
        let mut opus_cfg = config.llm.anthropic.clone();
        opus_cfg.model = std::env::var("SUBAGENT_OPUS_MODEL")
            .unwrap_or_else(|_| "claude-opus-4-7".to_string());
        match (
            AnthropicProvider::new(haiku_cfg),
            AnthropicProvider::new(sonnet_cfg),
            AnthropicProvider::new(opus_cfg),
        ) {
            (Ok(h), Ok(s), Ok(o)) => Some((
                Arc::new(h) as Arc<dyn ironclad::llm::LlmProvider>,
                Arc::new(s) as Arc<dyn ironclad::llm::LlmProvider>,
                Arc::new(o) as Arc<dyn ironclad::llm::LlmProvider>,
            )),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                tracing::warn!(
                    "spawn_agent not registered (failed to build Haiku/Sonnet/Opus providers): {e}"
                );
                None
            }
        }
    };

    // Register builder tool if enabled
    if config.builder.enabled {
        tools
            .register_builder_tool(
                llm.clone(),
                safety.clone(),
                Some(config.builder.to_builder_config()),
            )
            .await;
        tracing::info!("Builder mode enabled");
    }

    // Create secrets store if master key is configured (needed for MCP auth and WASM channels)
    let secrets_store: Option<Arc<dyn SecretsStore + Send + Sync>> =
        if let Some(master_key) = config.secrets.master_key() {
            match SecretsCrypto::new(master_key.clone()) {
                Ok(crypto) => match FjallSecretsStore::open(
                    &ironclad_data_dir().join("secrets-index").to_string_lossy(),
                    Arc::new(crypto),
                ) {
                    Ok(s) => Some(Arc::new(s)),
                    Err(e) => {
                        tracing::warn!("Failed to open secrets store: {}", e);
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to initialize secrets crypto: {}", e);
                    None
                }
            }
        } else {
            None
        };

    let mcp_session_manager = Arc::new(McpSessionManager::new());

    // Create WASM tool runtime (sync, just builds the wasmtime engine)
    let wasm_tool_runtime: Option<Arc<WasmToolRuntime>> =
        if config.wasm.enabled && config.wasm.tools_dir.exists() {
            match WasmToolRuntime::new(config.wasm.to_runtime_config()) {
                Ok(runtime) => Some(Arc::new(runtime)),
                Err(e) => {
                    tracing::warn!("Failed to initialize WASM runtime: {}", e);
                    None
                }
            }
        } else {
            None
        };

    // Load WASM tools and MCP servers concurrently.
    // Both register into the shared ToolRegistry (RwLock-based) so concurrent writes are safe.
    let wasm_tools_future = async {
        if let Some(ref runtime) = wasm_tool_runtime {
            let loader = WasmToolLoader::new(Arc::clone(runtime), Arc::clone(&tools));
            match loader.load_from_dir(&config.wasm.tools_dir).await {
                Ok(results) => {
                    if !results.loaded.is_empty() {
                        tracing::info!(
                            "Loaded {} WASM tools from {}",
                            results.loaded.len(),
                            config.wasm.tools_dir.display()
                        );
                    }
                    for (path, err) in &results.errors {
                        tracing::warn!("Failed to load WASM tool {}: {}", path.display(), err);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to scan WASM tools directory: {}", e);
                }
            }
        }
    };

    let mcp_servers_future = async {
        // MCP loading used to be entirely gated on `secrets_store` being
        // Some, which silently disabled the whole loader when
        // `SECRETS_MASTER_KEY` wasn't in `.env`. That broke Playwright +
        // Eustress (neither needs a secrets store; only OAuth-authenticated
        // remote MCPs do). Now we load every server, and only skip the
        // ones that genuinely require auth when no secrets store is open.
        match load_mcp_servers().await {
            Ok(servers) => {
                let enabled: Vec<_> = servers.enabled_servers().cloned().collect();
                if !enabled.is_empty() {
                    tracing::info!("Loading {} configured MCP server(s)...", enabled.len());
                }

                let mut join_set = tokio::task::JoinSet::new();
                for server in enabled {
                    let mcp_sm = Arc::clone(&mcp_session_manager);
                    let secrets_opt = secrets_store.as_ref().map(Arc::clone);
                    let tools = Arc::clone(&tools);

                        join_set.spawn(async move {
                            let server_name = server.name.clone();

                            // Stdio MCP servers go through a separate transport
                            // (subprocess + newline-JSON-RPC). The HTTP client's
                            // OAuth / session machinery doesn't apply.
                            if server.is_stdio() {
                                tracing::debug!(
                                    "Spawning stdio MCP server '{}': {}",
                                    server_name,
                                    server.command.as_deref().unwrap_or("(no command)")
                                );
                                match ironclad::tools::mcp::StdioMcpClient::spawn(&server).await {
                                    Ok(client) => {
                                        let client = std::sync::Arc::new(client);
                                        match client.create_tools().await {
                                            Ok(tool_impls) => {
                                                let n = tool_impls.len();
                                                for tool in tool_impls {
                                                    tools.register(tool).await;
                                                }
                                                tracing::info!(
                                                    "Loaded {} tools from stdio MCP '{}'",
                                                    n,
                                                    server_name
                                                );
                                            }
                                            Err(e) => tracing::warn!(
                                                "stdio MCP '{}' create_tools failed: {}",
                                                server_name,
                                                e
                                            ),
                                        }
                                    }
                                    Err(e) => tracing::warn!(
                                        "stdio MCP '{}' spawn failed: {}",
                                        server_name,
                                        e
                                    ),
                                }
                                return;
                            }

                            // Pick HTTP transport: authenticated when the
                            // server declares OAuth AND we have a secrets
                            // store to fetch tokens from, otherwise simple
                            // unauthenticated (works for the local Playwright
                            // MCPs that don't require auth).
                            let client = match (server.requires_auth(), &secrets_opt) {
                                (true, Some(secrets)) => {
                                    let has_tokens = is_authenticated(&server, secrets, "default").await;
                                    tracing::debug!(
                                        "MCP '{}' requires auth, has_tokens={}",
                                        server_name,
                                        has_tokens
                                    );
                                    McpClient::new_authenticated(
                                        server,
                                        mcp_sm,
                                        Arc::clone(secrets),
                                        "default",
                                    )
                                }
                                (true, None) => {
                                    tracing::warn!(
                                        "MCP '{}' requires OAuth but no secrets store is open \
                                         (SECRETS_MASTER_KEY unset). Skipping.",
                                        server_name
                                    );
                                    return;
                                }
                                (false, _) => {
                                    // Hosted MCPs authed by a long-lived
                                    // API key (e.g. Strategic Profits)
                                    // declare static_headers in config. We
                                    // need to keep the McpServerConfig on
                                    // the client so resolved_static_headers
                                    // actually fires at request time.
                                    if server.static_headers.is_some() {
                                        McpClient::new_with_static_auth(
                                            server,
                                            Arc::clone(&mcp_sm),
                                        )
                                    } else {
                                        McpClient::new_with_name(
                                            &server_name,
                                            &server.url,
                                            Arc::clone(&mcp_sm),
                                        )
                                    }
                                }
                            };

                            tracing::debug!("Fetching tools from MCP server '{}'...", server_name);
                            match client.list_tools().await {
                                Ok(mcp_tools) => {
                                    let tool_count = mcp_tools.len();
                                    tracing::debug!(
                                        "Got {} tools from MCP server '{}'",
                                        tool_count,
                                        server_name
                                    );
                                    match client.create_tools().await {
                                        Ok(tool_impls) => {
                                            for tool in tool_impls {
                                                tools.register(tool).await;
                                            }
                                            tracing::info!(
                                                "Loaded {} tools from MCP server '{}'",
                                                tool_count,
                                                server_name
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to create tools from MCP server '{}': {}",
                                                server_name,
                                                e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    let err_str = e.to_string();
                                    if err_str.contains("401") || err_str.contains("authentication")
                                    {
                                        tracing::warn!(
                                            "MCP server '{}' requires authentication. \
                                             Run: ironclad mcp auth {}",
                                            server_name,
                                            server_name
                                        );
                                    } else {
                                        tracing::warn!(
                                            "Failed to connect to MCP server '{}': {}",
                                            server_name,
                                            e
                                        );
                                    }
                                }
                            }
                        });
                    }

                while let Some(result) = join_set.join_next().await {
                    if let Err(e) = result {
                        tracing::warn!("MCP server loading task panicked: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("No MCP servers configured ({})", e);
            }
        }
    };

    tokio::join!(wasm_tools_future, mcp_servers_future);

    // Collapse big MCP namespaces into single router tools. Cuts the
    // tool-definition payload sent to Claude on every turn from ~200
    // schemas down to ~40 (native ~30 + 1 router per MCP namespace).
    // List the namespaces explicitly: only the ones with >= 3 sub-tools
    // actually get collapsed, but listing them here is cheap and
    // documents intent.
    let mcp_namespaces: &[&str] = &[
        "playwright_marketing",
        "playwright_personal",
        "playwright_state",
        "playwright_federal",
        "playwright_tech",
        "playwright_cdp",
        "playwright",
        "eustress",
        "sp-shared",
    ];
    let _collapsed = ironclad::tools::mcp::install_namespace_routers(&tools, mcp_namespaces).await;

    // Now that every native + WASM + MCP tool (and any collapsed
    // namespace routers) is in the registry, refresh TOOLS.md so the
    // system prompt carries an accurate categorized inventory. This is
    // what gives Haiku a mental map of his surface without paying for
    // the full schema list in the prompt.
    if let Some(ref repo) = repository {
        let workspace = Arc::new(Workspace::new("default", repo.clone()));
        let inventory = ironclad::tools::builtin::generate_inventory(&tools).await;
        if let Err(e) = workspace
            .write(ironclad::workspace::paths::TOOLS, &inventory)
            .await
        {
            tracing::warn!("Failed to refresh TOOLS.md inventory: {}", e);
        } else {
            tracing::info!(
                "Refreshed TOOLS.md inventory ({} top-level tools)",
                tools.count()
            );
        }
    }

    // Create extension manager for in-chat discovery/install/auth/activate
    let extension_manager = if let Some(ref secrets) = secrets_store {
        let manager = Arc::new(ExtensionManager::new(
            Arc::clone(&mcp_session_manager),
            Arc::clone(secrets),
            Arc::clone(&tools),
            wasm_tool_runtime.clone(),
            config.wasm.tools_dir.clone(),
            config.channels.wasm_channels_dir.clone(),
            config.tunnel.public_url.clone(),
            "default".to_string(),
        ));
        tools.register_extension_tools(Arc::clone(&manager));
        tracing::info!("Extension manager initialized with in-chat discovery tools");
        Some(manager)
    } else {
        tracing::debug!(
            "Extension manager not available (no secrets store). \
             Extension tools won't be registered."
        );
        None
    };

    tracing::info!(
        "Tool registry initialized with {} total tools",
        tools.count()
    );

    // Initialize channel manager
    let mut channels = ChannelManager::new();

    if let Some(repl) = repl_channel {
        channels.add(Box::new(repl));
        if cli.message.is_some() {
            tracing::info!("Single message mode");
        } else {
            tracing::info!("REPL mode enabled");
        }
    }

    // Collect webhook route fragments; a single WebhookServer hosts them all.
    let mut webhook_routes: Vec<axum::Router> = Vec::new();

    // Load WASM channels and register their webhook routes.
    if config.channels.wasm_channels_enabled && config.channels.wasm_channels_dir.exists() {
        match WasmChannelRuntime::new(WasmChannelRuntimeConfig::default()) {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                let loader = WasmChannelLoader::new(Arc::clone(&runtime));

                match loader
                    .load_from_dir(&config.channels.wasm_channels_dir)
                    .await
                {
                    Ok(results) => {
                        let wasm_router = Arc::new(WasmChannelRouter::new());
                        let mut has_webhook_channels = false;

                        for loaded in results.loaded {
                            let channel_name = loaded.name().to_string();
                            tracing::info!("Loaded WASM channel: {}", channel_name);

                            let secret_name = loaded.webhook_secret_name();

                            let webhook_secret = if let Some(ref secrets) = secrets_store {
                                secrets
                                    .get_decrypted("default", &secret_name)
                                    .await
                                    .ok()
                                    .map(|s| s.expose().to_string())
                            } else {
                                None
                            };

                            let secret_header =
                                loaded.webhook_secret_header().map(|s| s.to_string());

                            let webhook_path = format!("/webhook/{}", channel_name);
                            let endpoints = vec![RegisteredEndpoint {
                                channel_name: channel_name.clone(),
                                path: webhook_path.clone(),
                                methods: vec!["POST".to_string()],
                                require_secret: webhook_secret.is_some(),
                            }];

                            let channel_arc = Arc::new(loaded.channel);

                            {
                                let mut config_updates = std::collections::HashMap::new();

                                if let Some(ref tunnel_url) = config.tunnel.public_url {
                                    config_updates.insert(
                                        "tunnel_url".to_string(),
                                        serde_json::Value::String(tunnel_url.clone()),
                                    );
                                }

                                if let Some(ref secret) = webhook_secret {
                                    config_updates.insert(
                                        "webhook_secret".to_string(),
                                        serde_json::Value::String(secret.clone()),
                                    );
                                }

                                if !config_updates.is_empty() {
                                    channel_arc.update_config(config_updates).await;
                                    tracing::info!(
                                        channel = %channel_name,
                                        has_tunnel = config.tunnel.public_url.is_some(),
                                        has_webhook_secret = webhook_secret.is_some(),
                                        "Injected runtime config into channel"
                                    );
                                }
                            }

                            tracing::info!(
                                channel = %channel_name,
                                has_webhook_secret = webhook_secret.is_some(),
                                secret_header = ?secret_header,
                                "Registering channel with router"
                            );

                            wasm_router
                                .register(
                                    Arc::clone(&channel_arc),
                                    endpoints,
                                    webhook_secret.clone(),
                                    secret_header,
                                )
                                .await;
                            has_webhook_channels = true;

                            if let Some(ref secrets) = secrets_store {
                                match inject_channel_credentials(
                                    &channel_arc,
                                    secrets.as_ref(),
                                    &channel_name,
                                )
                                .await
                                {
                                    Ok(count) => {
                                        if count > 0 {
                                            tracing::info!(
                                                channel = %channel_name,
                                                credentials_injected = count,
                                                "Channel credentials injected"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            channel = %channel_name,
                                            error = %e,
                                            "Failed to inject channel credentials"
                                        );
                                    }
                                }
                            }

                            channels.add(Box::new(SharedWasmChannel::new(channel_arc)));
                        }

                        if has_webhook_channels && config.tunnel.public_url.is_some() {
                            webhook_routes.push(create_wasm_channel_router(
                                wasm_router,
                                extension_manager.as_ref().map(Arc::clone),
                            ));
                        }

                        for (path, err) in &results.errors {
                            tracing::warn!(
                                "Failed to load WASM channel {}: {}",
                                path.display(),
                                err
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to scan WASM channels directory: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to initialize WASM channel runtime: {}", e);
            }
        }
    }

    // Add HTTP channel if configured and not CLI-only mode.
    // Extract its routes for the unified server; the channel itself just
    // provides the mpsc stream.
    let mut webhook_server_addr: Option<std::net::SocketAddr> = None;
    if !cli.cli_only {
        if let Some(ref http_config) = config.channels.http {
            let http_channel = HttpChannel::new(http_config.clone());
            webhook_routes.push(http_channel.routes());
            let (host, port) = http_channel.addr();
            webhook_server_addr = Some(
                format!("{}:{}", host, port)
                    .parse()
                    .expect("HttpConfig host:port must be a valid SocketAddr"),
            );
            channels.add(Box::new(http_channel));
            tracing::info!(
                "HTTP channel enabled on {}:{}",
                http_config.host,
                http_config.port
            );
        }
    }

    // Start the unified webhook server if any routes were registered.
    let mut webhook_server = if !webhook_routes.is_empty() {
        let addr =
            webhook_server_addr.unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 8080)));
        let mut server = WebhookServer::new(WebhookServerConfig { addr });
        for routes in webhook_routes {
            server.add_routes(routes);
        }
        server.start().await?;
        Some(server)
    } else {
        None
    };

    // Create workspace for agent (shared with memory tools)
    let workspace = repository.as_ref().map(|repo| {
        let mut ws = Workspace::new("default", repo.clone());
        if let Some(ref emb) = embeddings {
            ws = ws.with_embeddings(emb.clone());
        }
        Arc::new(ws)
    });

    // Backfill embeddings if we just enabled the provider
    if let (Some(ws), Some(_)) = (&workspace, &embeddings) {
        match ws.backfill_embeddings().await {
            Ok(count) if count > 0 => {
                tracing::info!("Backfilled embeddings for {} chunks", count);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to backfill embeddings: {}", e);
            }
        }
    }

    // Create context manager (shared between job tools and agent)
    let context_manager = Arc::new(ContextManager::new(config.agent.max_parallel_jobs));

    // Create session manager (shared between agent and web gateway)
    let session_manager = Arc::new(SessionManager::new());

    // Register job tools
    tools.register_job_tools(Arc::clone(&context_manager));

    // Add web gateway channel if configured
    if let Some(ref gw_config) = config.channels.gateway {
        let mut gw = GatewayChannel::new(gw_config.clone());
        if let Some(ref ws) = workspace {
            gw = gw.with_workspace(Arc::clone(ws));
        }
        gw = gw.with_context_manager(Arc::clone(&context_manager));
        gw = gw.with_session_manager(Arc::clone(&session_manager));
        gw = gw.with_log_broadcaster(Arc::clone(&log_broadcaster));
        gw = gw.with_tool_registry(Arc::clone(&tools));
        gw = gw.with_voice(config.voice.clone());
        if let Some(ref ext_mgr) = extension_manager {
            gw = gw.with_extension_manager(Arc::clone(ext_mgr));
        }

        tracing::info!(
            "Web gateway enabled on {}:{}",
            gw_config.host,
            gw_config.port
        );

        channels.add(Box::new(gw));
    }

    // Wrap channels in Arc so spawn_agent can hold a back-reference for
    // fire-and-forget result broadcasting, and Agent::new can hold the
    // same shared instance. Must happen AFTER all channels.add(...) calls.
    let channels = Arc::new(channels);

    // Now that the ChannelManager is built and wrapped, register the
    // sub-agent dispatcher so spawn_agent's background tasks can broadcast
    // their completion summaries back to McKale as unsolicited assistant
    // messages.
    let sonnet_for_autonomous: Option<Arc<dyn ironclad::llm::LlmProvider>> =
        if let Some((haiku, sonnet, opus)) = subagent_providers {
            let sonnet_clone = Arc::clone(&sonnet);
            tools.register_spawn_agent_tool(
                haiku,
                sonnet,
                opus,
                Arc::clone(&channels),
                Arc::clone(&context_manager),
            );
            Some(sonnet_clone)
        } else {
            None
        };

    // Autonomous loop: opt-in via AUTONOMOUS_LOOP_ENABLED=true. Runs in
    // parallel with the main agent message loop on its own timer; uses
    // Sonnet so it's cheaper than the main brain. Pushes summaries
    // through a notify channel into the broadcaster so they land in the
    // JARVIS transcript like any other assistant message. Voice is
    // gated by a sentinel the model writes in its final response.
    if std::env::var("AUTONOMOUS_LOOP_ENABLED").as_deref() == Ok("true") {
        match (sonnet_for_autonomous, workspace.as_ref()) {
            (Some(sonnet_llm), Some(workspace_arc)) => {
                let workspace_for_loop = Arc::clone(workspace_arc);
                let interval_secs: u64 = std::env::var("AUTONOMOUS_LOOP_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300);
                let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<
                    ironclad::channels::OutgoingResponse,
                >(32);
                let channels_fwd = Arc::clone(&channels);
                let notify_user = std::env::var("AUTONOMOUS_LOOP_USER")
                    .unwrap_or_else(|_| "default".to_string());
                tokio::spawn(async move {
                    while let Some(resp) = notify_rx.recv().await {
                        let results = channels_fwd.broadcast_all(&notify_user, resp).await;
                        for (ch, result) in results {
                            if let Err(e) = result {
                                tracing::warn!(
                                    "autonomous loop broadcast to {} failed: {}",
                                    ch,
                                    e
                                );
                            }
                        }
                    }
                });

                // Least-privilege allow list for stabilization. Pulled
                // from the conversation: observe widely, act narrowly,
                // never delete or create desktops.
                let allowed: std::collections::HashSet<String> = [
                    // observe / read
                    "recent_logs",
                    "claude_code_transcript_tail",
                    "windows_get_input_focus",
                    "windows_screenshot_foreground",
                    "windows_list_monitors",
                    "windows_list_desktops",
                    "vault_read",
                    "vault_search",
                    "vault_list",
                    "memory_read",
                    "memory_search",
                    "memory_tree",
                    "feedback_log_read",
                    "feedback_log_write",
                    "list_my_tools",
                    "mission_lookup",
                    "time",
                    "json",
                    "github_list_prs",
                    "github_list_issues",
                    "github_list_repos",
                    "github_recent_commits",
                    "github_get_pr",
                    "read_file",
                    "list_dir",
                    // act / write — narrow but covers the build-itself loop
                    "spawn_agent",
                    "windows_focus_window",
                    "windows_type_text",
                    "windows_press_key",
                    "windows_switch_desktop",
                    "windows_snap_window",
                    "windows_move_window_to_desktop",
                    "vault_write",
                    "vault_move",
                    "memory_write",
                    "write_file",
                    "apply_patch",
                    "shell",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let mut denied = std::collections::HashSet::new();
                denied.insert("vault_delete".to_string());
                denied.insert("windows_new_desktop".to_string());

                let config = ironclad::agent::AutonomousLoopConfig {
                    interval: std::time::Duration::from_secs(interval_secs),
                    enabled: true,
                    max_iterations_per_tick: 6,
                    max_failures: 3,
                    notify_user_id: std::env::var("AUTONOMOUS_LOOP_USER")
                        .unwrap_or_else(|_| "default".to_string()),
                    notify_channel: "gateway".to_string(),
                    allowed: Some(allowed),
                    denied,
                };

                ironclad::agent::spawn_autonomous_loop(
                    config,
                    workspace_for_loop,
                    sonnet_llm,
                    Arc::clone(&tools),
                    Some(notify_tx),
                );
                tracing::info!(
                    "🤖 autonomous loop spawned, interval={}s, model=Sonnet",
                    interval_secs
                );
            }
            (None, _) => {
                tracing::warn!(
                    "AUTONOMOUS_LOOP_ENABLED=true but Sonnet provider not built; skipping"
                );
            }
            (_, None) => {
                tracing::warn!(
                    "AUTONOMOUS_LOOP_ENABLED=true but no workspace (DB unavailable); skipping"
                );
            }
        }
    }

    // Create and run the agent
    let deps = AgentDeps {
        store,
        llm,
        safety,
        tools,
        workspace,
        extension_manager,
    };

    let agent = Agent::new(
        config.agent.clone(),
        deps,
        channels,
        Some(config.heartbeat.clone()),
        Some(context_manager),
        Some(session_manager),
    );

    tracing::info!("Agent initialized, starting main loop...");

    // Run the agent (blocks until shutdown)
    agent.run().await?;

    // Shut down the webhook server if one was started
    if let Some(ref mut server) = webhook_server {
        server.shutdown().await;
    }

    tracing::info!("Agent shutdown complete");
    Ok(())
}

/// Write the JARVIS identity files into the workspace on first boot.
/// Idempotent: only writes a path if it doesn't already exist, so users can
/// customise them in-place via memory_write or the dashboard.
async fn bootstrap_jarvis_identity(workspace: &Arc<ironclad::workspace::Workspace>) {
    // (path, content, always_overwrite). Agent-managed files are rewritten
    // every boot so changes to workspace_seed/*.md ship via `cargo build`
    // and reach the LLM on the next restart. USER.md is create-once so
    // McKale's edits survive.
    let defaults: &[(&str, &str, bool)] = &[
        ("IDENTITY.md",   include_str!("../workspace_seed/IDENTITY.md"),   true),
        ("SOUL.md",       include_str!("../workspace_seed/SOUL.md"),       true),
        ("AGENTS.md",     include_str!("../workspace_seed/AGENTS.md"),     true),
        ("AUTONOMOUS.md", include_str!("../workspace_seed/AUTONOMOUS.md"), true),
        ("USER.md",       include_str!("../workspace_seed/USER.md"),       false),
    ];
    for (path, content, always_overwrite) in defaults {
        if *always_overwrite {
            if let Err(e) = workspace.write(path, content).await {
                tracing::warn!("failed to refresh {}: {}", path, e);
            } else {
                tracing::info!("refreshed {} from seed", path);
            }
            continue;
        }
        match workspace.read(path).await {
            Ok(_) => { /* user-editable file already there, leave it alone */ }
            Err(_) => {
                if let Err(e) = workspace.write(path, content).await {
                    tracing::warn!("failed to seed {}: {}", path, e);
                } else {
                    tracing::info!("seeded {} into workspace", path);
                }
            }
        }
    }

    // Live-sync the rich vault docs into the workspace every boot. McKale
    // edits CLIENT-PROFILE.md and CLIENT-DOSSIER.md in Obsidian; the next
    // JARVIS restart picks them up and Workspace::system_prompt() injects
    // them into every turn. Runtime read (not include_str!) so a vault edit
    // doesn't need a recompile.
    let vault_root = std::env::var("VAULT_PATH")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(root) = vault_root {
        let vault_synced: &[(&str, &str)] = &[
            ("CLIENT-PROFILE.md", "00 System/CLIENT-PROFILE.md"),
            ("CLIENT-DOSSIER.md", "00 System/CLIENT-DOSSIER.md"),
            ("VAULT-SUMMARY.md",  "00 System/VAULT-SUMMARY.md"),
            ("WISHLIST.md",       "00 System/WISHLIST.md"),
            ("MY-TOOLKIT.md",     "00 System/MY-TOOLKIT.md"),
        ];
        for (workspace_path, vault_rel) in vault_synced {
            let src = root.join(vault_rel);
            match tokio::fs::read_to_string(&src).await {
                Ok(content) => {
                    if let Err(e) = workspace.write(workspace_path, &content).await {
                        tracing::warn!(
                            "failed to refresh {} from vault {}: {}",
                            workspace_path,
                            src.display(),
                            e
                        );
                    } else {
                        tracing::info!(
                            "refreshed {} from vault ({} bytes)",
                            workspace_path,
                            content.len()
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "vault doc {} not readable, skipping ({})",
                        src.display(),
                        e
                    );
                }
            }
        }
    } else {
        tracing::debug!(
            "VAULT_PATH not set, skipping CLIENT-PROFILE/DOSSIER sync"
        );
    }
}

/// Resolve the host:port the onboarding wizard should bind to.
///
/// Honors `GATEWAY_HOST` / `GATEWAY_PORT` so `cargo run-jarvis` and the eventual
/// full-mode gateway use the same address — restarting after onboarding swaps
/// the onboard server out for the real gateway on the exact same socket.
fn onboard_bind_addr() -> (String, u16) {
    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("GATEWAY_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(3030);
    (host, port)
}

/// Inject credentials for a channel based on naming convention.
///
/// Looks for secrets matching the pattern `{channel_name}_*` and injects them
/// as credential placeholders (e.g., `telegram_bot_token` -> `{TELEGRAM_BOT_TOKEN}`).
///
/// Returns the number of credentials injected.
async fn inject_channel_credentials(
    channel: &Arc<ironclad::channels::wasm::WasmChannel>,
    secrets: &dyn SecretsStore,
    channel_name: &str,
) -> anyhow::Result<usize> {
    // List all secrets for this user and filter by channel prefix
    let all_secrets = secrets
        .list("default")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list secrets: {}", e))?;

    let prefix = format!("{}_", channel_name);
    let mut count = 0;

    for secret_meta in all_secrets {
        // Only process secrets matching the channel prefix
        if !secret_meta.name.starts_with(&prefix) {
            continue;
        }

        // Get the decrypted value
        let decrypted = match secrets.get_decrypted("default", &secret_meta.name).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    secret = %secret_meta.name,
                    error = %e,
                    "Failed to decrypt secret for channel credential injection"
                );
                continue;
            }
        };

        // Convert secret name to placeholder format (SCREAMING_SNAKE_CASE)
        let placeholder = secret_meta.name.to_uppercase();

        tracing::debug!(
            channel = %channel_name,
            secret = %secret_meta.name,
            placeholder = %placeholder,
            "Injecting credential"
        );

        channel
            .set_credential(&placeholder, decrypted.expose().to_string())
            .await;
        count += 1;
    }

    Ok(count)
}
