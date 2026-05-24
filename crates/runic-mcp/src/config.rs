//! Config: `~/.runic/mcp.json` parsing.
//!
//! Wire format matches Claude Desktop / Cursor's `mcpServers` shape for
//! stdio servers, and extends it with a `url` field for HTTP servers.
//! The parser discriminates via the `url` key — present means HTTP,
//! absent means stdio. Both shapes are exposed as a single
//! [`McpServerConfig`] enum that callers pattern-match on.
//!
//! Example combining both:
//! ```json
//! {
//!   "mcpServers": {
//!     "filesystem": {
//!       "command": "uvx",
//!       "args": ["mcp-server-filesystem", "/tmp"]
//!     },
//!     "remote-api": {
//!       "url": "https://mcp.example.com/messages",
//!       "headers": { "Authorization": "Bearer abc" }
//!     }
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    /// Map of server-name → server-config. The name shows up in the
    /// prefixed tool registry name (`mcp__{server}__{tool}`).
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

/// One MCP server entry. Variants discriminate on the presence of `url`
/// vs `command` (untagged), so users can mix stdio and HTTP entries freely
/// in the same `mcpServers` map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// HTTP transport — `url` field present.
    Http(HttpServerConfig),
    /// Stdio transport — `command` field present.
    Stdio(StdioServerConfig),
}

/// Public-facing shape: this match arm is the structural pattern we expose
/// to callers in [`McpServerConfig::shared`]/[`McpServerConfig::transport_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StdioServerConfig {
    /// Executable to spawn (resolved via PATH unless absolute).
    pub command: String,
    /// Arguments passed verbatim.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra env vars to pass to the child. Merged on top of the parent
    /// process's environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// When `true` (default), this server can be reused across sessions
    /// via `SharedMcpPool`. Set `false` for stateful servers (browser
    /// sessions, IDE handles) that must be spawned per-session.
    #[serde(default = "default_shared")]
    pub shared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    /// Full URL to the MCP endpoint (e.g. `https://example.com/mcp`).
    pub url: String,
    /// Extra headers to include on every request (auth tokens, tenant
    /// ids, etc.). Sent in addition to the spec-required Content-Type
    /// and Accept headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Same semantics as the stdio `shared` field.
    #[serde(default = "default_shared")]
    pub shared: bool,
}

fn default_shared() -> bool {
    true
}

impl McpServerConfig {
    pub fn shared(&self) -> bool {
        match self {
            Self::Stdio(c) => c.shared,
            Self::Http(c) => c.shared,
        }
    }

    pub fn transport_kind(&self) -> TransportKind {
        match self {
            Self::Stdio(_) => TransportKind::Stdio,
            Self::Http(_) => TransportKind::Http,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl McpConfig {
    pub async fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let p: &Path = path.as_ref();
        let raw = tokio::fs::read_to_string(p)
            .await
            .map_err(|source| ConfigError::Io {
                path: p.to_path_buf(),
                source,
            })?;
        Self::parse(&raw, p)
    }

    pub async fn try_load_from_path(
        path: impl AsRef<Path>,
    ) -> Result<Option<Self>, ConfigError> {
        let p: &Path = path.as_ref();
        match tokio::fs::read_to_string(p).await {
            Ok(raw) => Self::parse(&raw, p).map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: p.to_path_buf(),
                source,
            }),
        }
    }

    fn parse(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        serde_json::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mcp_servers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_stdio_only_config() {
        let raw = r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "ghp_abc" }
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("test.json")).unwrap();
        let github = cfg.mcp_servers.get("github").unwrap();
        match github {
            McpServerConfig::Stdio(c) => {
                assert_eq!(c.command, "npx");
                assert_eq!(c.args.len(), 2);
                assert_eq!(c.env.get("GITHUB_TOKEN").map(String::as_str), Some("ghp_abc"));
                assert!(c.shared);
            }
            other => panic!("expected Stdio variant, got {other:?}"),
        }
        assert_eq!(github.transport_kind(), TransportKind::Stdio);
    }

    #[test]
    fn parses_http_only_config() {
        let raw = r#"{
            "mcpServers": {
                "remote-api": {
                    "url": "https://mcp.example.com/messages",
                    "headers": { "Authorization": "Bearer abc" }
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("test.json")).unwrap();
        let remote = cfg.mcp_servers.get("remote-api").unwrap();
        match remote {
            McpServerConfig::Http(c) => {
                assert_eq!(c.url, "https://mcp.example.com/messages");
                assert_eq!(c.headers.get("Authorization").map(String::as_str), Some("Bearer abc"));
                assert!(c.shared);
            }
            other => panic!("expected Http variant, got {other:?}"),
        }
        assert_eq!(remote.transport_kind(), TransportKind::Http);
    }

    #[test]
    fn parses_mixed_stdio_and_http() {
        let raw = r#"{
            "mcpServers": {
                "local": { "command": "echo", "args": ["hi"] },
                "remote": { "url": "https://example.com/mcp" }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("t.json")).unwrap();
        assert_eq!(cfg.len(), 2);
        assert_eq!(
            cfg.mcp_servers.get("local").unwrap().transport_kind(),
            TransportKind::Stdio
        );
        assert_eq!(
            cfg.mcp_servers.get("remote").unwrap().transport_kind(),
            TransportKind::Http
        );
    }

    #[test]
    fn empty_object_parses_as_empty_config() {
        let cfg = McpConfig::parse("{}", Path::new("t.json")).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_mcp_servers_object_is_fine() {
        let cfg = McpConfig::parse(r#"{"mcpServers":{}}"#, Path::new("t.json")).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn shared_false_is_respected_for_stdio() {
        let raw = r#"{
            "mcpServers": {
                "browser": {
                    "command": "node",
                    "args": ["server.js"],
                    "shared": false
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("t.json")).unwrap();
        assert!(!cfg.mcp_servers.get("browser").unwrap().shared());
    }

    #[test]
    fn shared_false_is_respected_for_http() {
        let raw = r#"{
            "mcpServers": {
                "session": {
                    "url": "https://example.com/mcp",
                    "shared": false
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("t.json")).unwrap();
        assert!(!cfg.mcp_servers.get("session").unwrap().shared());
    }

    #[test]
    fn malformed_json_yields_parse_error() {
        let err = McpConfig::parse("{not json", Path::new("/oops")).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => assert_eq!(path, PathBuf::from("/oops")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_from_path_reads_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{"mcpServers":{}}"#).await.unwrap();
        let cfg = McpConfig::load_from_path(&path).await.unwrap();
        assert!(cfg.is_empty());
    }

    #[tokio::test]
    async fn try_load_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cfg = McpConfig::try_load_from_path(&path).await.unwrap();
        assert!(cfg.is_none());
    }

    #[tokio::test]
    async fn try_load_propagates_non_notfound_errors() {
        let dir = tempdir().unwrap();
        let result = McpConfig::try_load_from_path(dir.path()).await;
        assert!(result.is_err());
    }
}
