//! Mission workspace lookup. Reads `00 System/Workspaces.md` from the
//! vault and returns the structured config for a named mission. Pairs
//! with the "Open Mission Workspace" mission pattern in AGENTS.md so
//! JARVIS knows which Chrome profile, desktop name, repo path, tab
//! bundle, and layout policy belong to each venture.
//!
//! Format expected in `Workspaces.md`: one `## <Name>` section per
//! mission, with key/value lines in plain `key: value` form. Lists
//! (tabs, external_apps, share_with) accept either YAML-style `- url`
//! bullets or comma-separated inline values. The parser is forgiving;
//! whatever McKale types should mostly work.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

const WORKSPACES_PATH: &str = "00 System/Workspaces.md";

fn vault_base_path() -> PathBuf {
    std::env::var("VAULT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Users\miksu\Documents\Olson"))
}

pub struct MissionLookupTool;

#[async_trait]
impl Tool for MissionLookupTool {
    fn name(&self) -> &str {
        "mission_lookup"
    }

    fn description(&self) -> &str {
        "Read the named mission's workspace config from \
         00 System/Workspaces.md. Returns chrome_profile, desktop_name, \
         repo_path, layout, left_tabs, right_tabs, external_apps, \
         share_with, prefer_reuse, category. Pass `name` to fetch one \
         mission; pass nothing (or `list: true`) to enumerate mission names. \
         JARVIS calls this at the start of an 'open mission workspace' flow."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Mission section name (e.g. 'Eustress', 'GetCSV'). Case-insensitive match."
                },
                "list": {
                    "type": "boolean",
                    "description": "If true, return just the list of all mission names. Default false."
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
        let want_list = params.get("list").and_then(|v| v.as_bool()).unwrap_or(false);
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let path = vault_base_path().join(WORKSPACES_PATH);
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!(
                "Workspaces.md not readable at {}: {}",
                path.display(),
                e
            ))
        })?;
        let missions = parse_workspaces(&content);

        if want_list || name.is_none() {
            let names: Vec<&String> = missions.iter().map(|m| &m.name).collect();
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "missions": names,
                    "count": names.len(),
                }),
                start.elapsed(),
            ));
        }

        let query = name.unwrap().to_ascii_lowercase();
        let m = missions
            .iter()
            .find(|m| m.name.to_ascii_lowercase() == query)
            .or_else(|| {
                missions
                    .iter()
                    .find(|m| m.name.to_ascii_lowercase().contains(&query))
            });

        match m {
            Some(m) => Ok(ToolOutput::success(serde_json::to_value(m).unwrap(), start.elapsed())),
            None => {
                let suggestions: Vec<&String> =
                    missions.iter().map(|m| &m.name).take(5).collect();
                Err(ToolError::ExecutionFailed(format!(
                    "mission '{}' not found in Workspaces.md. Known missions: {:?}",
                    query, suggestions
                )))
            }
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

#[derive(Debug, serde::Serialize, Default)]
pub struct Mission {
    pub name: String,
    pub chrome_profile: Option<String>,
    pub desktop_name: Option<String>,
    pub repo_path: Option<String>,
    pub category: Option<String>,
    pub layout: Option<String>,
    pub left_tabs: Vec<String>,
    pub right_tabs: Vec<String>,
    pub external_apps: Vec<String>,
    pub share_with: Vec<String>,
    pub prefer_reuse: Option<bool>,
    pub required_logins: Vec<String>,
}

/// Forgiving parser. Walks the markdown line by line, switches mission
/// on each `## ` header, and stores any `key: value` line under the
/// current mission. List-typed fields (`left_tabs`, etc.) also accept
/// indented `- value` bullets that follow the field name.
fn parse_workspaces(content: &str) -> Vec<Mission> {
    let mut out: Vec<Mission> = Vec::new();
    let mut current: Option<Mission> = None;
    let mut last_list_field: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // New mission section.
        if let Some(rest) = trimmed.strip_prefix("## ") {
            // Top-level # is also section starts in some files; only treat
            // ## as a mission header so the frontmatter and # title don't
            // create empty mission entries.
            if let Some(m) = current.take() {
                if !m.name.is_empty() {
                    out.push(m);
                }
            }
            current = Some(Mission {
                name: rest.trim().to_string(),
                ..Default::default()
            });
            last_list_field = None;
            continue;
        }

        let Some(m) = current.as_mut() else {
            continue;
        };

        // Continuation of a list field.
        if let Some(bullet) = trimmed.strip_prefix("- ") {
            if let Some(field) = &last_list_field {
                push_list(m, field, bullet.trim());
            }
            continue;
        }

        // `key: value` line.
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim().to_ascii_lowercase();
            let value = trimmed[colon + 1..].trim();
            last_list_field = None;
            match key.as_str() {
                "chrome_profile" => m.chrome_profile = Some(value.to_string()),
                "desktop_name" => m.desktop_name = Some(value.to_string()),
                "repo_path" => m.repo_path = Some(value.to_string()),
                "category" => m.category = Some(value.to_string()),
                "layout" => m.layout = Some(value.to_string()),
                "prefer_reuse" => m.prefer_reuse = Some(parse_bool(value)),
                "left_tabs" | "right_tabs" | "external_apps" | "share_with"
                | "required_logins" => {
                    last_list_field = Some(key.clone());
                    // Inline comma-separated values: `left_tabs: a, b, c`.
                    if !value.is_empty() && value != "[]" {
                        for item in split_inline_list(value) {
                            push_list(m, &key, &item);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(m) = current.take() {
        if !m.name.is_empty() {
            out.push(m);
        }
    }
    out
}

fn push_list(m: &mut Mission, field: &str, value: &str) {
    let v = value.trim().to_string();
    if v.is_empty() {
        return;
    }
    match field {
        "left_tabs" => m.left_tabs.push(v),
        "right_tabs" => m.right_tabs.push(v),
        "external_apps" => m.external_apps.push(v),
        "share_with" => m.share_with.push(v),
        "required_logins" => m.required_logins.push(v),
        _ => {}
    }
}

fn split_inline_list(s: &str) -> Vec<String> {
    // Strip surrounding brackets if any, then comma-split.
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_mission() {
        let md = r#"
# Title

## Eustress
chrome_profile: tech
desktop_name: Eustress
prefer_reuse: true
layout: chrome_left_app_right
left_tabs:
  - https://github.com/WeaveITMeta/EustressEngine
  - https://github.com/WeaveITMeta/EustressEngine/pulls
right_tabs: https://discord.gg/eustress

## GetCSV
chrome_profile: marketing
share_with: [BookDaddy, Veluxe]
"#;
        let m = parse_workspaces(md);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "Eustress");
        assert_eq!(m[0].chrome_profile.as_deref(), Some("tech"));
        assert_eq!(m[0].left_tabs.len(), 2);
        assert_eq!(m[0].right_tabs.len(), 1);
        assert_eq!(m[0].prefer_reuse, Some(true));
        assert_eq!(m[1].name, "GetCSV");
        assert_eq!(m[1].share_with, vec!["BookDaddy", "Veluxe"]);
    }
}
