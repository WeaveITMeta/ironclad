//! Built-in tools that come with the agent.

mod echo;
mod ecommerce;
pub mod extension_tools;
mod file;
mod github;
mod http;
mod job;
mod json;
mod marketplace;
mod memory;
mod mission_lookup;
mod os_control;
mod restaurant;
mod shell;
mod spawn_agent;
mod taskrabbit;
mod time;
mod tools_inventory;
mod vault;
#[cfg(target_os = "windows")]
mod windows_desktop;
#[cfg(target_os = "windows")]
mod windows_window;

pub use echo::EchoTool;
pub use ecommerce::EcommerceTool;
pub use extension_tools::{
    ToolActivateTool, ToolAuthTool, ToolInstallTool, ToolListTool, ToolRemoveTool, ToolSearchTool,
};
pub use file::{ApplyPatchTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use github::{
    GithubGetPrTool, GithubListIssuesTool, GithubListPrsTool, GithubListReposTool,
    GithubRecentCommitsTool,
};
pub use http::HttpTool;
pub use job::{CancelJobTool, CreateJobTool, JobStatusTool, ListJobsTool};
pub use json::JsonTool;
pub use marketplace::MarketplaceTool;
pub use memory::{MemoryReadTool, MemorySearchTool, MemoryTreeTool, MemoryWriteTool};
pub use mission_lookup::MissionLookupTool;
pub use os_control::{OpenAppTool, OpenUrlTool};
pub use restaurant::RestaurantTool;
pub use shell::ShellTool;
pub use spawn_agent::SpawnAgentTool;
pub use taskrabbit::TaskRabbitTool;
pub use time::TimeTool;
pub use tools_inventory::{generate_inventory, ListMyToolsTool};
pub use vault::{
    VaultDeleteTool, VaultListTool, VaultMoveTool, VaultReadTool, VaultSearchTool,
    VaultWriteTool,
};
#[cfg(target_os = "windows")]
pub use windows_desktop::{
    WindowsListDesktopsTool, WindowsMoveWindowToDesktopTool, WindowsNewDesktopTool,
    WindowsSwitchDesktopTool,
};
#[cfg(target_os = "windows")]
pub use windows_window::{WindowsListMonitorsTool, WindowsSnapWindowTool};
