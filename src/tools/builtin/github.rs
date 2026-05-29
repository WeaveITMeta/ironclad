//! Native GitHub tools.
//!
//! Read-only against `api.github.com` using a fine-grained PAT from
//! `GITHUB_PAT`. Defaults the org scope to `GITHUB_DEFAULT_ORG` (set in
//! `.env`) so the LLM doesn't have to repeat it on every call. All tools
//! here are `requires_approval: false` because they're pure reads — the
//! approval banner stays out of the way for triage / search / inspection
//! flows. If you ever add write tools (create issue, merge PR, etc.) flip
//! that to true.
//!
//! Why native instead of MCP: the official `@modelcontextprotocol/server-github`
//! is stdio-only and Iron Clad's MCP client speaks HTTP. Wrapping with
//! `mcp-proxy` works but adds a moving part. Five focused REST endpoints
//! are easier to read and faster to call directly.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

const USER_AGENT: &str = "ironclad-jarvis/0.3";
const API_BASE: &str = "https://api.github.com";

fn pat() -> Result<String, ToolError> {
    std::env::var("GITHUB_PAT").map_err(|_| {
        ToolError::ExecutionFailed(
            "GITHUB_PAT not set in .env — JARVIS can't talk to GitHub yet".into(),
        )
    })
}

fn default_org() -> Option<String> {
    std::env::var("GITHUB_DEFAULT_ORG").ok().filter(|s| !s.trim().is_empty())
}

fn client() -> Result<Client, ToolError> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::ExecutionFailed(format!("reqwest build: {e}")))
}

async fn get_json(url: &str) -> Result<Value, ToolError> {
    match get_json_opt(url).await? {
        Some(v) => Ok(v),
        None => Err(ToolError::ExecutionFailed(format!(
            "GitHub returned 404 Not Found for {url}"
        ))),
    }
}

/// Same as `get_json` but treats 404 as `Ok(None)` so the caller can
/// fall back (e.g. try `/users/{name}/repos` after `/orgs/{name}/repos`
/// 404s for a personal account masquerading as an org).
async fn get_json_opt(url: &str) -> Result<Option<Value>, ToolError> {
    let token = pat()?;
    let resp = client()?
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("GET {url}: {e}")))?;
    let code = resp.status();
    if code == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !code.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::ExecutionFailed(format!(
            "GitHub returned {code} for {url}: {body}"
        )));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("decode {url}: {e}")))
        .map(Some)
}

// ============================================================
// github_list_repos
// ============================================================

pub struct GithubListReposTool;

#[async_trait]
impl Tool for GithubListReposTool {
    fn name(&self) -> &str {
        "github_list_repos"
    }

    fn description(&self) -> &str {
        "List repositories belonging to a GitHub org OR user (e.g. \
         'WeaveITMeta' may be either). Defaults to GITHUB_DEFAULT_ORG. \
         Tries `/orgs/<name>/repos` first, falls back to `/users/<name>/repos` \
         on 404 so personal accounts work transparently. Returns name, \
         description, default_branch, open_issues_count, updated_at."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": {
                    "type": "string",
                    "description": "Org login (e.g. 'WeaveITMeta'). Optional; defaults to GITHUB_DEFAULT_ORG."
                },
                "per_page": {
                    "type": "integer",
                    "description": "Page size, max 100. Default 30.",
                    "default": 30,
                    "maximum": 100
                }
            }
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let org = params
            .get("org")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(default_org)
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "missing 'org' parameter and no GITHUB_DEFAULT_ORG set".into(),
                )
            })?;
        let per_page = params.get("per_page").and_then(|v| v.as_u64()).unwrap_or(30).min(100);

        // Try the org endpoint first. If GitHub returns 404, the name is
        // almost certainly a personal account, not an organization, so
        // retry against /users/{name}/repos. This is what's happening
        // with WeaveITMeta which owns repos as a user, not an org.
        let org_url = format!("{API_BASE}/orgs/{org}/repos?per_page={per_page}");
        let (body, account_kind) = match get_json_opt(&org_url).await? {
            Some(v) => (v, "org"),
            None => {
                let user_url = format!("{API_BASE}/users/{org}/repos?per_page={per_page}");
                let v = get_json(&user_url).await?;
                (v, "user")
            }
        };

        let trimmed = body
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r.get("name"),
                            "description": r.get("description"),
                            "default_branch": r.get("default_branch"),
                            "open_issues_count": r.get("open_issues_count"),
                            "updated_at": r.get("updated_at"),
                            "private": r.get("private"),
                            "archived": r.get("archived"),
                            "url": r.get("html_url"),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(ToolOutput::success(
            serde_json::json!({
                "owner": org,
                "owner_kind": account_kind,
                "repos": trimmed,
                "count": trimmed.len(),
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// github_list_prs
// ============================================================

pub struct GithubListPrsTool;

#[async_trait]
impl Tool for GithubListPrsTool {
    fn name(&self) -> &str {
        "github_list_prs"
    }

    fn description(&self) -> &str {
        "List open pull requests across an org (or a specific repo). Uses GitHub's \
         search API so it's one call regardless of how many repos. Returns each PR's \
         title, repo, number, author, created_at, updated_at, comment count, draft \
         flag, and html_url. Use this as the first step in a triage mission."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": {
                    "type": "string",
                    "description": "Org login. Optional; defaults to GITHUB_DEFAULT_ORG."
                },
                "repo": {
                    "type": "string",
                    "description": "Optional repo name to narrow scope (still requires org). E.g. 'eustress'."
                },
                "state": {
                    "type": "string",
                    "enum": ["open", "closed", "all"],
                    "default": "open"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max PRs to return. Default 30, cap 100.",
                    "default": 30,
                    "maximum": 100
                }
            }
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let org = params
            .get("org")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(default_org)
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "missing 'org' parameter and no GITHUB_DEFAULT_ORG set".into(),
                )
            })?;
        let repo = params.get("repo").and_then(|v| v.as_str()).map(String::from);
        let state = params.get("state").and_then(|v| v.as_str()).unwrap_or("open");
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(30).min(100);

        // Build a search query. `is:pr` plus state qualifier plus org/repo scope.
        let scope = match &repo {
            Some(r) => format!("repo:{org}/{r}"),
            None => format!("org:{org}"),
        };
        let state_q = match state {
            "closed" => "is:closed",
            "all" => "", // omit
            _ => "is:open",
        };
        let q = format!("is:pr {state_q} {scope}").trim().to_string();
        let url = format!(
            "{API_BASE}/search/issues?q={}&per_page={limit}",
            urlencoding::encode(&q)
        );
        let body = get_json(&url).await?;

        #[derive(Deserialize)]
        struct SearchResp {
            total_count: u64,
            items: Vec<Value>,
        }
        let parsed: SearchResp = serde_json::from_value(body).map_err(|e| {
            ToolError::ExecutionFailed(format!("parse search response: {e}"))
        })?;

        let prs: Vec<Value> = parsed
            .items
            .iter()
            .map(|i| {
                // search/issues URL is the api.github.com one; html_url is the web URL.
                // repo can be inferred from the repository_url ("/repos/org/repo").
                let repo = i
                    .get("repository_url")
                    .and_then(|v| v.as_str())
                    .and_then(|u| u.rsplit('/').next())
                    .map(String::from);
                serde_json::json!({
                    "number": i.get("number"),
                    "title": i.get("title"),
                    "repo": repo,
                    "author": i.get("user").and_then(|u| u.get("login")),
                    "created_at": i.get("created_at"),
                    "updated_at": i.get("updated_at"),
                    "comments": i.get("comments"),
                    "draft": i.get("draft"),
                    "url": i.get("html_url"),
                    "labels": i.get("labels").and_then(|l| l.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|lb| lb.get("name").and_then(|n| n.as_str()))
                            .collect::<Vec<_>>()
                    }),
                })
            })
            .collect();

        Ok(ToolOutput::success(
            serde_json::json!({
                "org": org,
                "repo": repo,
                "state": state,
                "total": parsed.total_count,
                "returned": prs.len(),
                "prs": prs,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// github_get_pr
// ============================================================

pub struct GithubGetPrTool;

#[async_trait]
impl Tool for GithubGetPrTool {
    fn name(&self) -> &str {
        "github_get_pr"
    }

    fn description(&self) -> &str {
        "Get full details for a single pull request: title, body, author, age, base/head \
         branches, mergeable status, conflicts, review state, additions/deletions, file count. \
         Call this on the 3-5 priority PRs identified by github_list_prs, not on every PR."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": { "type": "string", "description": "Org login. Optional; defaults to GITHUB_DEFAULT_ORG." },
                "repo": { "type": "string", "description": "Repo name (required)." },
                "number": { "type": "integer", "description": "PR number (required)." }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let org = params
            .get("org")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(default_org)
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "missing 'org' parameter and no GITHUB_DEFAULT_ORG set".into(),
                )
            })?;
        let repo = params
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'repo'".into()))?;
        let number = params
            .get("number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'number'".into()))?;
        let url = format!("{API_BASE}/repos/{org}/{repo}/pulls/{number}");
        let body = get_json(&url).await?;
        let summary = serde_json::json!({
            "number": body.get("number"),
            "title": body.get("title"),
            "body": body.get("body"),
            "author": body.get("user").and_then(|u| u.get("login")),
            "state": body.get("state"),
            "draft": body.get("draft"),
            "created_at": body.get("created_at"),
            "updated_at": body.get("updated_at"),
            "merged": body.get("merged"),
            "mergeable": body.get("mergeable"),
            "mergeable_state": body.get("mergeable_state"),
            "additions": body.get("additions"),
            "deletions": body.get("deletions"),
            "changed_files": body.get("changed_files"),
            "comments": body.get("comments"),
            "review_comments": body.get("review_comments"),
            "base": body.get("base").and_then(|b| b.get("ref")),
            "head": body.get("head").and_then(|b| b.get("ref")),
            "url": body.get("html_url"),
            "labels": body.get("labels").and_then(|l| l.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|lb| lb.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
            }),
        });
        Ok(ToolOutput::success(summary, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// github_list_issues
// ============================================================

pub struct GithubListIssuesTool;

#[async_trait]
impl Tool for GithubListIssuesTool {
    fn name(&self) -> &str {
        "github_list_issues"
    }

    fn description(&self) -> &str {
        "List open issues (NOT pull requests) across an org or a specific repo. \
         Same shape as github_list_prs but filters to issue-only. Useful for bug triage."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": { "type": "string" },
                "repo": { "type": "string" },
                "state": { "type": "string", "enum": ["open", "closed", "all"], "default": "open" },
                "limit": { "type": "integer", "default": 30, "maximum": 100 }
            }
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let org = params
            .get("org")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(default_org)
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "missing 'org' parameter and no GITHUB_DEFAULT_ORG set".into(),
                )
            })?;
        let repo = params.get("repo").and_then(|v| v.as_str()).map(String::from);
        let state = params.get("state").and_then(|v| v.as_str()).unwrap_or("open");
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(30).min(100);

        let scope = match &repo {
            Some(r) => format!("repo:{org}/{r}"),
            None => format!("org:{org}"),
        };
        let state_q = match state {
            "closed" => "is:closed",
            "all" => "",
            _ => "is:open",
        };
        let q = format!("is:issue {state_q} {scope}").trim().to_string();
        let url = format!(
            "{API_BASE}/search/issues?q={}&per_page={limit}",
            urlencoding::encode(&q)
        );
        let body = get_json(&url).await?;
        #[derive(Deserialize)]
        struct SearchResp {
            total_count: u64,
            items: Vec<Value>,
        }
        let parsed: SearchResp = serde_json::from_value(body)
            .map_err(|e| ToolError::ExecutionFailed(format!("parse: {e}")))?;
        let issues: Vec<Value> = parsed
            .items
            .iter()
            .map(|i| {
                let repo = i
                    .get("repository_url")
                    .and_then(|v| v.as_str())
                    .and_then(|u| u.rsplit('/').next())
                    .map(String::from);
                serde_json::json!({
                    "number": i.get("number"),
                    "title": i.get("title"),
                    "repo": repo,
                    "author": i.get("user").and_then(|u| u.get("login")),
                    "created_at": i.get("created_at"),
                    "updated_at": i.get("updated_at"),
                    "comments": i.get("comments"),
                    "url": i.get("html_url"),
                    "labels": i.get("labels").and_then(|l| l.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|lb| lb.get("name").and_then(|n| n.as_str()))
                            .collect::<Vec<_>>()
                    }),
                })
            })
            .collect();
        Ok(ToolOutput::success(
            serde_json::json!({
                "org": org,
                "repo": repo,
                "state": state,
                "total": parsed.total_count,
                "returned": issues.len(),
                "issues": issues,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ============================================================
// github_recent_commits
// ============================================================

pub struct GithubRecentCommitsTool;

#[async_trait]
impl Tool for GithubRecentCommitsTool {
    fn name(&self) -> &str {
        "github_recent_commits"
    }

    fn description(&self) -> &str {
        "Get the most recent commits on a repo's default branch. \
         Returns sha, message, author, date. Useful for 'what just landed in repo X'."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": { "type": "string" },
                "repo": { "type": "string", "description": "Repo name (required)." },
                "limit": { "type": "integer", "default": 10, "maximum": 50 }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let org = params
            .get("org")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(default_org)
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "missing 'org' parameter and no GITHUB_DEFAULT_ORG set".into(),
                )
            })?;
        let repo = params
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'repo'".into()))?;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10).min(50);
        let url = format!("{API_BASE}/repos/{org}/{repo}/commits?per_page={limit}");
        let body = get_json(&url).await?;
        let commits: Vec<Value> = body
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|c| {
                        serde_json::json!({
                            "sha": c.get("sha"),
                            "message": c.get("commit").and_then(|m| m.get("message")),
                            "author": c.get("commit")
                                .and_then(|m| m.get("author"))
                                .and_then(|a| a.get("name")),
                            "date": c.get("commit")
                                .and_then(|m| m.get("author"))
                                .and_then(|a| a.get("date")),
                            "url": c.get("html_url"),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(ToolOutput::success(
            serde_json::json!({
                "org": org,
                "repo": repo,
                "commits": commits,
                "count": commits.len(),
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}
