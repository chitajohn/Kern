//! Daemon runtime configuration (`SPEC.md §17`).
//!
//! Read from environment variables with explicit defaults. Env wins over any
//! future `$KERN_HOME/kern.toml` (which is not parsed until the daemon lands).
//! Secrets (provider API keys, the API token) are deliberately not
//! part of this struct: they are read from the environment at use time and
//! never persisted or logged (`SPEC.md §14.3`).

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::error::{ErrorCode, KernError, Result};

/// Default API bind address (`SPEC.md §17`).
pub const DEFAULT_API_ADDR: &str = "127.0.0.1:8787";
/// Default concurrency caps.
pub const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 8;
pub const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 16;
/// Default Ollama endpoint.
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// Non-secret daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Data dir (`state.db`, `logs/`, `workspace/` live here).
    pub home: PathBuf,
    /// API bind address (loopback by default).
    pub api_addr: SocketAddr,
    /// tracing level for `telemetry::init`.
    pub log_level: String,
    /// Global agent concurrency cap.
    pub max_concurrent_agents: usize,
    /// Global tool process cap.
    pub max_concurrent_tools: usize,
    pub ollama_base_url: String,
    pub openai_base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
}

impl RuntimeConfig {
    /// Load from the environment, applying `SPEC.md §17` defaults.
    pub fn from_env() -> Result<Self> {
        let home = env_home()?;
        let api_addr = env_or("KERN_API_ADDR", DEFAULT_API_ADDR)
            .parse::<SocketAddr>()
            .map_err(|e| {
                KernError::new(
                    ErrorCode::ConfigInvalid,
                    format!("invalid KERN_API_ADDR {:?}: {e}", env_or("KERN_API_ADDR", DEFAULT_API_ADDR)),
                )
            })?;

        let max_concurrent_agents = parse_positive("KERN_MAX_CONCURRENT_AGENTS", DEFAULT_MAX_CONCURRENT_AGENTS)?;
        let max_concurrent_tools = parse_positive("KERN_MAX_CONCURRENT_TOOLS", DEFAULT_MAX_CONCURRENT_TOOLS)?;

        Ok(Self {
            home,
            api_addr,
            log_level: env_or("KERN_LOG", "info"),
            max_concurrent_agents,
            max_concurrent_tools,
            ollama_base_url: env_or("OLLAMA_BASE_URL", DEFAULT_OLLAMA_BASE_URL),
            openai_base_url: env_opt("OPENAI_BASE_URL"),
            anthropic_base_url: env_opt("ANTHROPIC_BASE_URL"),
        })
    }
}

/// `$KERN_HOME`, else `~/.kern` (platform-aware home resolution).
fn env_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("KERN_HOME") {
        if home.is_empty() {
            return Err(KernError::new(
                ErrorCode::ConfigInvalid,
                "KERN_HOME must not be empty",
            ));
        }
        return Ok(PathBuf::from(home));
    }
    #[cfg(windows)]
    let base = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME");
    let base = base.ok_or_else(|| {
        KernError::new(
            ErrorCode::ConfigInvalid,
            "cannot determine home directory: set KERN_HOME or HOME",
        )
    })?;
    Ok(PathBuf::from(base).join(".kern"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn parse_positive(key: &str, default: usize) -> Result<usize> {
    match std::env::var(key) {
        Ok(raw) => {
            let value: usize = raw.parse().map_err(|e| {
                KernError::new(
                    ErrorCode::ConfigInvalid,
                    format!("invalid {key} {raw:?}: {e} (expected a positive integer)"),
                )
            })?;
            if value == 0 {
                return Err(KernError::new(
                    ErrorCode::ConfigInvalid,
                    format!("{key} must be greater than zero"),
                ));
            }
            Ok(value)
        }
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_all() {
        for key in [
            "KERN_HOME",
            "KERN_API_ADDR",
            "KERN_LOG",
            "KERN_MAX_CONCURRENT_AGENTS",
            "KERN_MAX_CONCURRENT_TOOLS",
            "OLLAMA_BASE_URL",
            "OPENAI_BASE_URL",
            "ANTHROPIC_BASE_URL",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn defaults_apply_when_env_is_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_all();
        let cfg = RuntimeConfig::from_env().unwrap();
        assert_eq!(cfg.api_addr.to_string(), DEFAULT_API_ADDR);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.max_concurrent_agents, 8);
        assert_eq!(cfg.max_concurrent_tools, 16);
        assert_eq!(cfg.ollama_base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(cfg.openai_base_url, None);
        assert!(cfg.home.to_string_lossy().ends_with(".kern"));
    }

    #[test]
    fn env_overrides_apply() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_all();
        std::env::set_var("KERN_API_ADDR", "127.0.0.1:9999");
        std::env::set_var("KERN_LOG", "debug");
        std::env::set_var("KERN_MAX_CONCURRENT_AGENTS", "3");
        std::env::set_var("KERN_MAX_CONCURRENT_TOOLS", "5");
        std::env::set_var("OLLAMA_BASE_URL", "http://localhost:8080");
        std::env::set_var("OPENAI_BASE_URL", "https://gateway.example.com/v1");

        let cfg = RuntimeConfig::from_env().unwrap();
        assert_eq!(cfg.api_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.max_concurrent_agents, 3);
        assert_eq!(cfg.max_concurrent_tools, 5);
        assert_eq!(cfg.ollama_base_url, "http://localhost:8080");
        assert_eq!(
            cfg.openai_base_url.as_deref(),
            Some("https://gateway.example.com/v1")
        );
    }

    #[test]
    fn kern_home_wins_over_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_all();
        std::env::set_var("KERN_HOME", "/tmp/kern-test-home");
        let cfg = RuntimeConfig::from_env().unwrap();
        assert_eq!(cfg.home, PathBuf::from("/tmp/kern-test-home"));
    }

    #[test]
    fn invalid_api_addr_fails() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_all();
        std::env::set_var("KERN_API_ADDR", "not-an-addr");
        let err = RuntimeConfig::from_env().unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("KERN_API_ADDR"), "{}", err.message);
    }

    #[test]
    fn invalid_concurrency_fails() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_all();
        for bad in ["0", "-1", "abc"] {
            std::env::set_var("KERN_MAX_CONCURRENT_AGENTS", bad);
            let err = RuntimeConfig::from_env().unwrap_err();
            assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{bad:?} must fail");
        }
    }
}
