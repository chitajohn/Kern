//! `kern doctor` — environment health (SPEC.md §16).
//!
//! Checks, from most to least critical:
//!
//! 1. **Store integrity** — opening the store runs the SQLite integrity
//!    check. `STORAGE_LOCKED` is a pass: it means a daemon holds the store
//!    (and verified integrity at its startup).
//! 2. **Daemon / API** — `GET /health` reports the running runtime's version,
//!    storage schema version, and sandbox backend. A down daemon is a fail
//!    with an actionable hint (agents cannot run without it).
//! 3. **Provider keys** — which model providers are configured. Missing keys
//!    are warnings, not failures: `mock` and a local `ollama` need none.
//!
//! Exit: `0` when every check passed, `1` when any check failed.

use std::path::PathBuf;

use kern_core::error::ErrorCode;
use kern_core::sandbox;
use kern_core::store::Store;

use crate::client::Client;
use crate::client::ClientError;
use crate::table;

/// One doctor row.
enum Verdict {
    Pass(String),
    Warn(String),
    Fail(String),
}

/// Run all checks; returns the number of failed checks.
pub async fn run(home: Option<PathBuf>) -> Result<usize, String> {
    let home = home.unwrap_or_else(crate::client::default_home);
    let mut rows: Vec<(&str, Verdict)> = Vec::new();

    // 1. Store integrity (local, independent of the daemon).
    match Store::open(&home) {
        Ok(_) => rows.push((
            "store",
            Verdict::Pass(format!("{} integrity ok", home.join("state.db").display())),
        )),
        Err(err) if err.code() == ErrorCode::StorageLocked => rows.push((
            "store",
            Verdict::Pass("locked by a running daemon (integrity verified at its startup)".into()),
        )),
        Err(err) => {
            rows.push(("store", Verdict::Fail(err.to_string())));
        }
    }

    // 2. Daemon / API.
    match Client::from_env() {
        Ok(client) => match client.health().await {
            Ok(health) => {
                let version = health["version"].as_str().unwrap_or("?");
                let schema = health["schema_version"].as_i64().unwrap_or(-1);
                let backend = health["sandbox"].as_str().unwrap_or("?");
                rows.push((
                    "daemon",
                    Verdict::Pass(format!(
                        "reachable at {} — kern {version}, schema v{schema}",
                        client.base_url()
                    )),
                ));
                rows.push((
                    "sandbox",
                    Verdict::Pass(format!("strongest available backend: {backend}")),
                ));
            }
            Err(ClientError::Unreachable(addr)) => {
                rows.push((
                    "daemon",
                    Verdict::Fail(format!(
                        "no daemon at {addr} — start one with `kern daemon`"
                    )),
                ));
                rows.push((
                    "sandbox",
                    Verdict::Pass(format!(
                        "strongest available backend: {}",
                        sandbox::backend_name()
                    )),
                ));
            }
            Err(err) => rows.push(("daemon", Verdict::Fail(err.to_string()))),
        },
        Err(err) => rows.push(("daemon", Verdict::Fail(err.to_string()))),
    }

    // 3. Provider keys (warnings; mock/ollama need none).
    for (key, provider) in [
        ("OPENAI_API_KEY", "openai"),
        ("ANTHROPIC_API_KEY", "anthropic"),
    ] {
        let set = std::env::var_os(key).is_some_and(|v| !v.is_empty());
        if set {
            rows.push((provider, Verdict::Pass(format!("{key} set"))));
        } else {
            rows.push((
                provider,
                Verdict::Warn(format!("{key} unset (mock/ollama still work)")),
            ));
        }
    }

    // Print a summary line per check plus the sandbox note.
    let mut failed = 0usize;
    let mut body: Vec<Vec<String>> = Vec::new();
    for (name, verdict) in &rows {
        let (status, detail) = match verdict {
            Verdict::Pass(detail) => ("ok", detail.clone()),
            Verdict::Warn(detail) => ("warn", detail.clone()),
            Verdict::Fail(detail) => {
                failed += 1;
                ("FAIL", detail.clone())
            }
        };
        body.push(vec![name.to_string(), status.to_string(), detail]);
    }
    table::print_table(&["check", "status", "detail"], &body);

    if failed == 0 {
        println!("doctor: all checks passed");
    } else {
        println!("doctor: {failed} check(s) failed");
    }
    Ok(failed)
}
