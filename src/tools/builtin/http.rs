//! HTTP request tool.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::context::JobContext;
use crate::safety::LeakDetector;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Hosts that the `http` tool may fetch from without a per-call
/// approval banner. Everything NOT on this list (and not on the
/// `IRONCLAD_HTTP_ALLOWLIST` env-var extension) forces an approval
/// prompt EVERY time, even when the session has "Always approved" the
/// http tool. Mirrors the shell allowlist's stance: one blanket
/// "Always" click can never silently unlock arbitrary egress.
///
/// Match semantics: exact host equality OR suffix-match preceded by
/// a dot, so an entry like `elevenlabs.io` covers
/// `api.elevenlabs.io` but NOT `evil-elevenlabs.io`.
///
/// Curated set: the model-provider APIs the gateway itself calls
/// (Anthropic, OpenAI), TTS (ElevenLabs), and GitHub / GitLab where
/// JARVIS's read-only github_* tools live. Add venture domains via
/// the env-var extension (see HTTP_ALLOWLIST_EXTRA below) so the
/// static list stays minimal.
pub static HTTP_ALLOWLIST_HOSTS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        // Model + embedding APIs the gateway uses directly
        "api.anthropic.com",
        "api.openai.com",
        // ElevenLabs TTS
        "api.elevenlabs.io",
        "elevenlabs.io",
        // GitHub (api + raw + clone + release artifacts)
        "api.github.com",
        "github.com",
        "raw.githubusercontent.com",
        "objects.githubusercontent.com",
        "codeload.github.com",
        // GitLab (Voltec)
        "gitlab.com",
    ]
});

/// Optional extra allowlist entries from the env var
/// `IRONCLAD_HTTP_ALLOWLIST`, comma-separated. Use this to add venture
/// domains, configured MCP server hosts, or any extras without
/// recompiling — e.g.
///   IRONCLAD_HTTP_ALLOWLIST=eustress.io,getcsv.io,mcp-server.local
/// Whitespace around each entry is trimmed; entries are lowercased.
/// Read once at first access (LazyLock) since env vars don't change
/// mid-process.
static HTTP_ALLOWLIST_EXTRA: LazyLock<Vec<String>> = LazyLock::new(|| {
    std::env::var("IRONCLAD_HTTP_ALLOWLIST")
        .map(|s| {
            s.split(',')
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpApproval {
    /// Host is on the allowlist — fire without a banner.
    AlwaysAllow,
    /// Host is NOT allowlisted — force an approval banner every time,
    /// overriding any session-level "Always approved" decision.
    AlwaysAsk,
}

/// Classify a URL against the HTTP allowlist. Returns AlwaysAsk on
/// malformed URLs, missing host, or any host not matched by the
/// static or env-extended allowlist.
pub fn classify_url(url: &str) -> HttpApproval {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return HttpApproval::AlwaysAsk;
    };
    let Some(host) = parsed.host_str() else {
        return HttpApproval::AlwaysAsk;
    };
    let host = host.to_lowercase();

    for allowed in HTTP_ALLOWLIST_HOSTS.iter() {
        if matches_host(&host, allowed) {
            return HttpApproval::AlwaysAllow;
        }
    }
    for allowed in HTTP_ALLOWLIST_EXTRA.iter() {
        if matches_host(&host, allowed.as_str()) {
            return HttpApproval::AlwaysAllow;
        }
    }
    HttpApproval::AlwaysAsk
}

/// True iff `host` equals `allowed` exactly OR is a subdomain of it
/// (i.e. ends with `.{allowed}`). Prevents `evil-elevenlabs.io` from
/// matching `elevenlabs.io`.
fn matches_host(host: &str, allowed: &str) -> bool {
    host == allowed || host.ends_with(&format!(".{allowed}"))
}

/// Tool for making HTTP requests.
pub struct HttpTool {
    client: Client,
}

impl HttpTool {
    /// Create a new HTTP tool.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }
}

fn validate_url(url: &str) -> Result<reqwest::Url, ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ToolError::InvalidParameters(format!("invalid URL: {}", e)))?;

    if parsed.scheme() != "https" {
        return Err(ToolError::NotAuthorized(
            "only https URLs are allowed".to_string(),
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidParameters("URL missing host".to_string()))?;

    let host_lower = host.to_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".localhost") {
        return Err(ToolError::NotAuthorized(
            "localhost is not allowed".to_string(),
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_disallowed_ip(&ip) {
            return Err(ToolError::NotAuthorized(
                "private or local IPs are not allowed".to_string(),
            ));
        }
    }

    Ok(parsed)
}

fn is_disallowed_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || *v4 == std::net::Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || v6.is_unspecified()
        }
    }
}

impl Default for HttpTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &str {
        "http"
    }

    fn description(&self) -> &str {
        "Make HTTP requests to external APIs. Supports GET, POST, PUT, DELETE methods."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                    "description": "HTTP method"
                },
                "url": {
                    "type": "string",
                    "description": "The URL to request"
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "HTTP headers to include"
                },
                "body": {
                    "description": "Request body (for POST/PUT/PATCH)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Request timeout in seconds (default: 30)"
                }
            },
            "required": ["method", "url"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let method = params
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParameters("missing 'method' parameter".to_string())
            })?;

        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'url' parameter".to_string()))?;
        let parsed_url = validate_url(url)?;

        // Parse headers
        let headers: HashMap<String, String> = params
            .get("headers")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let headers_vec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Build request
        let mut request = match method.to_uppercase().as_str() {
            "GET" => self.client.get(parsed_url.clone()),
            "POST" => self.client.post(parsed_url.clone()),
            "PUT" => self.client.put(parsed_url.clone()),
            "DELETE" => self.client.delete(parsed_url.clone()),
            "PATCH" => self.client.patch(parsed_url.clone()),
            _ => {
                return Err(ToolError::InvalidParameters(format!(
                    "unsupported method: {}",
                    method
                )));
            }
        };

        // Add headers
        for (key, value) in headers {
            request = request.header(&key, &value);
        }

        // Add body if present
        let body_bytes = if let Some(body) = params.get("body") {
            let bytes = serde_json::to_vec(body)
                .map_err(|e| ToolError::InvalidParameters(format!("invalid body JSON: {}", e)))?;
            request = request.json(body);
            Some(bytes)
        } else {
            None
        };

        // Leak detection on outbound request (url/headers/body)
        let detector = LeakDetector::new();
        detector
            .scan_http_request(parsed_url.as_str(), &headers_vec, body_bytes.as_deref())
            .map_err(|e| ToolError::NotAuthorized(format!("{}", e)))?;

        // Execute request
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                ToolError::Timeout(Duration::from_secs(30))
            } else {
                ToolError::ExternalService(e.to_string())
            }
        })?;

        let status = response.status().as_u16();
        let headers: HashMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
            .collect();

        // Get response body
        let body_text = response.text().await.map_err(|e| {
            ToolError::ExternalService(format!("failed to read response body: {}", e))
        })?;

        // Try to parse as JSON, fall back to string
        let body: serde_json::Value = serde_json::from_str(&body_text)
            .unwrap_or_else(|_| serde_json::Value::String(body_text.clone()));

        let result = serde_json::json!({
            "status": status,
            "headers": headers,
            "body": body
        });

        Ok(ToolOutput::success(result, start.elapsed()).with_raw(body_text))
    }

    fn estimated_duration(&self, _params: &serde_json::Value) -> Option<Duration> {
        Some(Duration::from_secs(5)) // Average HTTP request time
    }

    fn requires_sanitization(&self) -> bool {
        true // External data always needs sanitization
    }

    fn requires_approval(&self) -> bool {
        true // HTTP requests go to external services, require user approval
    }

    /// Per-call approval gating against the static HTTP allowlist +
    /// `IRONCLAD_HTTP_ALLOWLIST` env extension. Allowlisted hosts
    /// (Anthropic / OpenAI / ElevenLabs / GitHub / GitLab + any
    /// venture domains in the env var) skip the banner; everything
    /// else forces approval EVERY time — even when the session has
    /// "Always approved" the http tool. The intent: a single Always-
    /// click on a benign call (e.g. github.com) can't silently grant
    /// exfiltration to attacker.com under prompt injection.
    fn approval_override(&self, params: &serde_json::Value) -> Option<bool> {
        let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("");
        match classify_url(url) {
            HttpApproval::AlwaysAllow => Some(false),
            HttpApproval::AlwaysAsk => Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_url_rejects_http() {
        let err = validate_url("http://example.com").unwrap_err();
        assert!(err.to_string().contains("https"));
    }

    #[test]
    fn test_validate_url_rejects_localhost() {
        let err = validate_url("https://localhost:8080").unwrap_err();
        assert!(err.to_string().contains("localhost"));
    }

    #[test]
    fn test_validate_url_accepts_https_public() {
        let url = validate_url("https://example.com").unwrap();
        assert_eq!(url.host_str(), Some("example.com"));
    }

    #[test]
    fn classify_allowlist_exact_match() {
        // Hosts from the static allowlist should auto-allow.
        assert_eq!(
            classify_url("https://api.anthropic.com/v1/messages"),
            HttpApproval::AlwaysAllow
        );
        assert_eq!(
            classify_url("https://api.openai.com/v1/embeddings"),
            HttpApproval::AlwaysAllow
        );
        assert_eq!(
            classify_url("https://api.elevenlabs.io/v1/text-to-speech/xyz"),
            HttpApproval::AlwaysAllow
        );
        assert_eq!(
            classify_url("https://api.github.com/repos/foo/bar"),
            HttpApproval::AlwaysAllow
        );
        assert_eq!(
            classify_url("https://raw.githubusercontent.com/foo/bar/main/README"),
            HttpApproval::AlwaysAllow
        );
    }

    #[test]
    fn classify_allowlist_subdomain_match() {
        // The allowlist entry `elevenlabs.io` should match any
        // legitimate subdomain via the dot-anchored suffix rule.
        assert_eq!(
            classify_url("https://api.elevenlabs.io/v1/voices"),
            HttpApproval::AlwaysAllow
        );
        assert_eq!(
            classify_url("https://docs.github.com/en/rest"),
            HttpApproval::AlwaysAllow
        );
    }

    #[test]
    fn classify_arbitrary_egress_always_asks() {
        // The wishlist's central concern: anything not on the
        // allowlist must prompt EVERY time, no matter what.
        assert_eq!(
            classify_url("https://attacker.com/exfil"),
            HttpApproval::AlwaysAsk
        );
        assert_eq!(
            classify_url("https://pastebin.com/raw/abc"),
            HttpApproval::AlwaysAsk
        );
        assert_eq!(
            classify_url("https://discord.com/api/webhooks/123/abc"),
            HttpApproval::AlwaysAsk
        );
    }

    #[test]
    fn classify_substring_lookalike_does_not_match() {
        // `evil-elevenlabs.io` ends with `elevenlabs.io` as a
        // substring but NOT as a dot-anchored suffix. The host
        // matcher must reject lookalike domains.
        assert_eq!(
            classify_url("https://evil-elevenlabs.io/steal"),
            HttpApproval::AlwaysAsk
        );
        assert_eq!(
            classify_url("https://notgithub.com/foo"),
            HttpApproval::AlwaysAsk
        );
    }

    #[test]
    fn classify_handles_mixed_case() {
        // URL parser preserves case in the path but lowercases the
        // host. classify_url lowercases defensively too in case the
        // parser ever changes; verify mixed-case host still matches.
        assert_eq!(
            classify_url("https://API.ANTHROPIC.COM/v1/messages"),
            HttpApproval::AlwaysAllow
        );
    }

    #[test]
    fn classify_malformed_url_asks() {
        // No URL → no schema known → safer to ask.
        assert_eq!(classify_url("not a url"), HttpApproval::AlwaysAsk);
        assert_eq!(classify_url(""), HttpApproval::AlwaysAsk);
    }

    #[test]
    fn approval_override_wires_to_classifier() {
        let tool = HttpTool::new();
        let allow = serde_json::json!({"url": "https://api.anthropic.com/v1/messages"});
        let ask = serde_json::json!({"url": "https://attacker.com/exfil"});
        let missing = serde_json::json!({});
        // Allowlist hits skip approval; everything else forces it
        // — including missing url (defaults to "" → AlwaysAsk).
        assert_eq!(tool.approval_override(&allow), Some(false));
        assert_eq!(tool.approval_override(&ask), Some(true));
        assert_eq!(tool.approval_override(&missing), Some(true));
    }

    #[test]
    fn matches_host_dot_anchored() {
        // Direct check of the suffix matcher used by classify_url.
        assert!(matches_host("api.elevenlabs.io", "elevenlabs.io"));
        assert!(matches_host("elevenlabs.io", "elevenlabs.io"));
        assert!(!matches_host("evil-elevenlabs.io", "elevenlabs.io"));
        assert!(!matches_host("elevenlabs.io.evil.com", "elevenlabs.io"));
    }
}
