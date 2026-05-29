//! Tool-name → friendly-name mapping for the sub-agents panel.
//!
//! McKale's directive: instead of showing "tool_exec" or the raw tool
//! identifier (e.g. `memory_search`, `playwright_marketing_browser_navigate`)
//! on each sub-agent card, give the agent a category and a name with
//! personality and purpose. The user sees "Memory Diver · MEMORY" or
//! "Web Scout · WEB" instead of a bare snake_case identifier, and the
//! card is tinted to its category color so a glance tells you what
//! kind of work is in flight.
//!
//! Mapping is regex/prefix-based so we degrade gracefully for tools we
//! haven't explicitly cataloged.

#[derive(Clone, Debug)]
pub struct AgentIdentity {
    /// The personality name shown in the card header.
    pub name: &'static str,
    /// Short uppercase category label rendered as a chip on the card.
    pub category: &'static str,
    /// Hex-ish RGB tint (r, g, b) used to color the card border + dot.
    pub rgb: (u8, u8, u8),
}

const MEMORY: AgentIdentity = AgentIdentity {
    name: "Memory Diver",
    category: "MEMORY",
    rgb: (34, 211, 238),
};
const VAULT: AgentIdentity = AgentIdentity {
    name: "Vault Keeper",
    category: "VAULT",
    rgb: (167, 139, 250),
};
const FILES: AgentIdentity = AgentIdentity {
    name: "Scribe",
    category: "FILES",
    rgb: (251, 191, 36),
};
const SHELL: AgentIdentity = AgentIdentity {
    name: "Operator",
    category: "SYSTEM",
    rgb: (244, 114, 182),
};
const WEB: AgentIdentity = AgentIdentity {
    name: "Web Scout",
    category: "WEB",
    rgb: (52, 211, 153),
};
const PLAYWRIGHT: AgentIdentity = AgentIdentity {
    name: "Browser Pilot",
    category: "WEB",
    rgb: (52, 211, 153),
};
const GITHUB: AgentIdentity = AgentIdentity {
    name: "Repo Sentry",
    category: "GITHUB",
    rgb: (139, 195, 247),
};
const WINDOWS: AgentIdentity = AgentIdentity {
    name: "Window Conductor",
    category: "DESKTOP",
    rgb: (147, 197, 253),
};
const EUSTRESS: AgentIdentity = AgentIdentity {
    name: "Eustress Engine",
    category: "SIM",
    rgb: (192, 132, 252),
};
const INFO: AgentIdentity = AgentIdentity {
    name: "Lookup",
    category: "INFO",
    rgb: (165, 243, 252),
};
const CODE: AgentIdentity = AgentIdentity {
    name: "Engineer",
    category: "CODE",
    rgb: (251, 113, 133),
};
const AGENT: AgentIdentity = AgentIdentity {
    name: "Sub-Commander",
    category: "AGENT",
    rgb: (236, 254, 255),
};
const JOBS: AgentIdentity = AgentIdentity {
    name: "Foreman",
    category: "JOBS",
    rgb: (251, 146, 60),
};
const GENERIC: AgentIdentity = AgentIdentity {
    name: "Worker",
    category: "TOOL",
    rgb: (165, 243, 252),
};
const ZENITH: AgentIdentity = AgentIdentity {
    name: "Zenith Coach",
    category: "ZENITH",
    rgb: (250, 204, 21),
};

/// Map a tool name to a friendly identity. Exact-match first, then
/// prefix rules for MCP namespace tools (playwright_*, eustress_*, etc.).
pub fn identify(tool: &str) -> AgentIdentity {
    // Specific named tools
    let ident = match tool {
        // Memory + Recall
        "memory_search" => Some(AgentIdentity { name: "Memory Diver", ..MEMORY }),
        "memory_read" => Some(AgentIdentity { name: "Recall", ..MEMORY }),
        "memory_write" => Some(AgentIdentity { name: "Memory Scribe", ..MEMORY }),
        "memory_tree" => Some(AgentIdentity { name: "Cartographer", ..MEMORY }),
        "feedback_log_write" => Some(AgentIdentity { name: "Coach Note", ..MEMORY }),
        "feedback_log_read" => Some(AgentIdentity { name: "Coach Recall", ..MEMORY }),

        // Vault (Eustress vault tools etc.)
        "vault_read" => Some(AgentIdentity { name: "Vault Reader", ..VAULT }),
        "vault_write" => Some(AgentIdentity { name: "Vault Scribe", ..VAULT }),
        "vault_list" => Some(AgentIdentity { name: "Vault Inventory", ..VAULT }),
        "vault_search" => Some(AgentIdentity { name: "Vault Seeker", ..VAULT }),
        "vault_delete" => Some(AgentIdentity { name: "Vault Purge", ..VAULT }),
        "vault_move" => Some(AgentIdentity { name: "Vault Mover", ..VAULT }),

        // Files / patches
        "read_file" => Some(AgentIdentity { name: "Reader", ..FILES }),
        "write_file" => Some(AgentIdentity { name: "Author", ..FILES }),
        "apply_patch" => Some(AgentIdentity { name: "Surgeon", ..FILES }),
        "list_dir" => Some(AgentIdentity { name: "Pathfinder", ..FILES }),

        // Shell + Apps
        "shell" => Some(AgentIdentity { name: "Operator", ..SHELL }),
        "open_app" => Some(AgentIdentity { name: "Launcher", ..SHELL }),
        "open_url" => Some(AgentIdentity { name: "Hyperlink", ..WEB }),

        // Info
        "time" => Some(AgentIdentity { name: "Timekeeper", ..INFO }),
        "weather" => Some(AgentIdentity { name: "Skywatcher", ..INFO }),
        "list_my_tools" => Some(AgentIdentity { name: "Self-Inventory", ..INFO }),
        "mission_lookup" => Some(AgentIdentity { name: "Mission Briefer", ..INFO }),
        "recent_logs" => Some(AgentIdentity { name: "Log Diver", ..INFO }),
        "claude_code_transcript_tail" => Some(AgentIdentity { name: "Transcript Tail", ..INFO }),
        "echo" => Some(AgentIdentity { name: "Echo", ..INFO }),
        "json" => Some(AgentIdentity { name: "JSON Wrangler", ..INFO }),

        // Code
        "build_software" => Some(AgentIdentity { name: "Engineer", ..CODE }),

        // Web / HTTP
        "http" => Some(AgentIdentity { name: "Messenger", ..WEB }),

        // Agents
        "spawn_agent" => Some(AgentIdentity { name: "Sub-Commander", ..AGENT }),

        // Jobs
        "create_job" => Some(AgentIdentity { name: "Foreman", ..JOBS }),
        "list_jobs" => Some(AgentIdentity { name: "Job Roster", ..JOBS }),
        "job_status" => Some(AgentIdentity { name: "Job Inspector", ..JOBS }),
        "cancel_job" => Some(AgentIdentity { name: "Job Abort", ..JOBS }),

        // Strategic Profits (Zenith Mind Elite) MCP
        "sp_admin" => Some(AgentIdentity { name: "SP Admin", ..ZENITH }),
        "sp_contribute" => Some(AgentIdentity { name: "Contributor", ..ZENITH }),
        "sp_launchpad" => Some(AgentIdentity { name: "Launchpad", ..ZENITH }),
        "sp_status" => Some(AgentIdentity { name: "Mirror Score", ..ZENITH }),
        "sp_submit_ai_opportunity" => Some(AgentIdentity { name: "Opportunity Scout", ..ZENITH }),
        "sp_submit_onboarding" => Some(AgentIdentity { name: "Onboarding", ..ZENITH }),
        "sp_sync" => Some(AgentIdentity { name: "SP Sync", ..ZENITH }),

        _ => None,
    };
    if let Some(i) = ident {
        return i;
    }

    // Prefix routing for MCP-namespaced tools.
    if tool.starts_with("playwright_") {
        return AgentIdentity {
            name: friendly_playwright_name(tool),
            ..PLAYWRIGHT
        };
    }
    if tool.starts_with("github_") {
        return AgentIdentity {
            name: friendly_github_name(tool),
            ..GITHUB
        };
    }
    if tool.starts_with("windows_") {
        return AgentIdentity {
            name: friendly_windows_name(tool),
            ..WINDOWS
        };
    }
    if tool.starts_with("eustress_") {
        return AgentIdentity {
            name: friendly_eustress_name(tool),
            ..EUSTRESS
        };
    }
    if tool.starts_with("memory_") {
        return MEMORY;
    }
    if tool.starts_with("vault_") {
        return VAULT;
    }
    if tool.starts_with("sp_") {
        return ZENITH;
    }

    GENERIC
}

fn friendly_playwright_name(tool: &str) -> &'static str {
    // playwright_<profile>_browser_<verb>
    let rest = tool.trim_start_matches("playwright_");
    let after_profile = rest.split_once('_').map(|(_, t)| t).unwrap_or(rest);
    match after_profile {
        "browser_navigate" | "browser_navigate_back" => "Web Navigator",
        "browser_click" => "Click Agent",
        "browser_type" | "browser_fill_form" | "browser_press_key" => "Typist",
        "browser_snapshot" | "browser_take_screenshot" => "Snapshotter",
        "browser_evaluate" | "browser_run_code_unsafe" => "JS Executor",
        "browser_wait_for" => "Page Watcher",
        "browser_close" | "browser_resize" | "browser_tabs" => "Tab Wrangler",
        "browser_network_requests" | "browser_network_request" | "browser_console_messages" => "Network Sniffer",
        _ => "Browser Pilot",
    }
}

fn friendly_github_name(tool: &str) -> &'static str {
    match tool {
        "github_list_repos" => "Repo Lister",
        "github_list_prs" | "github_get_pr" => "PR Auditor",
        "github_list_issues" => "Issue Triage",
        "github_recent_commits" => "Commit Historian",
        _ => "Repo Sentry",
    }
}

fn friendly_windows_name(tool: &str) -> &'static str {
    match tool {
        "windows_list_desktops" | "windows_switch_desktop" | "windows_new_desktop"
        | "windows_move_window_to_desktop" => "Desktop Coordinator",
        "windows_list_monitors" => "Display Mapper",
        "windows_snap_window" | "windows_focus_window" => "Window Conductor",
        "windows_type_text" | "windows_press_key" => "Input Operator",
        "windows_screenshot_foreground" => "Screen Capture",
        "windows_get_input_focus" => "Focus Sentinel",
        _ => "Desktop Agent",
    }
}

fn friendly_eustress_name(tool: &str) -> &'static str {
    if tool.contains("camera") {
        return "Eustress Camera";
    }
    if tool.contains("simulation") {
        return "Sim Controller";
    }
    if tool.contains("entity") {
        return "Entity Hand";
    }
    if tool.contains("git") {
        return "Eustress Git";
    }
    if tool.contains("script") {
        return "Script Author";
    }
    if tool.contains("recall") || tool.contains("remember") || tool.contains("memory") {
        return "Eustress Memory";
    }
    "Eustress Engine"
}
