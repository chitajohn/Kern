//! `kern init` — first-run scaffolding (SPEC.md §16).
//!
//! Creates `$KERN_HOME` (default `~/.kern`), generates the API bearer token
//! at `$KERN_HOME/token` (only when absent), and scaffolds a commented
//! `agent.yaml` in the current directory (only when absent). Nothing is
//! overwritten; the command is safe to run repeatedly.

use std::path::{Path, PathBuf};

use crate::client::default_home;

/// The scaffolded agent spec. The `mock` provider runs with zero setup so a
/// first `kern run agent.yaml` works immediately; the comments point at the
/// real providers.
pub const AGENT_YAML_TEMPLATE: &str = r#"# Kern agent specification (schema v1) — see SPEC.md §9.
#
# `kern run agent.yaml` creates and starts this agent. The mock provider
# needs no API key, so the scaffolded agent runs out of the box; switch the
# provider to openai|anthropic|ollama and set the matching key in your
# environment (OPENAI_API_KEY, ANTHROPIC_API_KEY, or a local Ollama).
version: 1
name: my-agent
description: "My first Kern agent."
model:
  provider: mock
  model: test
tools:
  - filesystem
permissions:
  filesystem:
    read:
      allow: [./]
    write:
      allow: [./]
runtime:
  checkpoint_interval: 30s
"#;

/// Run `kern init`. Returns the paths it created for the summary.
pub fn run(home: Option<PathBuf>) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    let home = home.unwrap_or_else(default_home);
    std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;

    let token_path = home.join("token");
    let token_created = if token_path.exists() {
        None
    } else {
        let token = generate_token();
        write_token(&token_path, &token)
            .map_err(|e| format!("write {}: {e}", token_path.display()))?;
        Some(token_path.clone())
    };

    let spec_path = Path::new("agent.yaml");
    let spec_created = if spec_path.exists() {
        None
    } else {
        std::fs::write(spec_path, AGENT_YAML_TEMPLATE)
            .map_err(|e| format!("write {}: {e}", spec_path.display()))?;
        Some(spec_path.to_path_buf())
    };

    Ok((home, token_created, spec_created))
}

/// 64 hex chars (two v4 uuids) — plenty of entropy for a local bearer token.
fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{token}\n"))?;
    // The token is a local secret: owner read/write only where the platform
    // supports it. Windows ACLs are out of scope; the file is at least not
    // world-readable there by default via umask behavior we do not touch.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
