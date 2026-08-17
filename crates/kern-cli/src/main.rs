//! `kern` — the Kern control interface (SPEC.md §16).
//!
//! The CLI talks to the daemon's local HTTP API only — it never touches the
//! database. `kern daemon` runs the runtime; every other command is a thin
//! client over `kern_core::api` (except `init`/`doctor`, which additionally
//! inspect local environment state).
//!
//! Exit codes (SPEC.md §16): `0` success, `1` runtime/client error, `2`
//! usage error (clap).

mod client;
mod daemon;
mod doctor;
mod init;
mod table;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::client::{Client, ClientError};

#[derive(Debug, Parser)]
#[command(
    name = "kern",
    version = kern_core::version::KERN_VERSION,
    about = "Kern — the open-source runtime for reliable, long-running AI agents.",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Data directory (default: $KERN_HOME or ~/.kern).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print version and API address.
    Version,
    /// Run the runtime daemon in the foreground.
    Daemon {
        /// Data directory (default: $KERN_HOME or ~/.kern).
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Scaffold agent.yaml, create $KERN_HOME, and generate the API token.
    Init {
        /// Data directory to create (default: $KERN_HOME or ~/.kern).
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Create and start an agent from a spec file.
    Run {
        /// Path to an agent.yaml spec.
        spec: PathBuf,
        /// Tail the run to completion; exit 0 on completed, 1 on failed.
        #[arg(short, long)]
        wait: bool,
    },
    /// Environment health: store integrity, daemon, sandbox, provider keys.
    Doctor,
    /// Table of agents: name, id, state, updated.
    Ps,
    /// Tail an agent's events (follow with -f).
    Logs {
        /// Agent name or id.
        target: String,
        /// Keep polling for new events.
        #[arg(short, long)]
        follow: bool,
        /// Number of past events to print first.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Agent detail, checkpoint summary, and last error.
    Inspect {
        /// Agent name or id.
        target: String,
    },
    /// Show an agent's schedule and next run time.
    Schedule {
        /// Agent name or id.
        target: String,
    },
    /// Checkpoint + pause an agent at its next safe point.
    Pause {
        /// Agent name or id.
        target: String,
    },
    /// Restore the latest checkpoint and resume a paused/recovering agent.
    Resume {
        /// Agent name or id.
        target: String,
    },
    /// Write a checkpoint now.
    Checkpoint {
        /// Agent name or id.
        target: String,
    },
    /// Terminate an agent.
    Terminate {
        /// Agent name or id.
        target: String,
    },
    /// List the builtin tools.
    Tools,
    /// List registered model providers.
    Models,
    /// List pending permission decisions.
    Permissions,
    /// Grant a pending permission request.
    Grant {
        /// Permission request id.
        request_id: String,
    },
    /// Deny a pending permission request.
    Deny {
        /// Permission request id.
        request_id: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("kern: {err}");
            1
        }
    };
    std::process::exit(code);
}

async fn dispatch(cli: Cli) -> Result<i32, String> {
    // `--home` is the CLI-level data-directory override. Mirror it into
    // `KERN_HOME` so every downstream resolver (API token, `default_home`,
    // daemon/init/doctor) honors it consistently — regression: client
    // commands resolved the token from `~/.kern` and returned 401 with a
    // custom `--home`. An explicit subcommand `--home` still wins where it
    // exists (it is read before `default_home()` is consulted).
    if let Some(home) = &cli.home {
        std::env::set_var("KERN_HOME", home);
    }
    let home = cli.home.clone();
    match cli.command {
        Command::Version => cmd_version().await,
        Command::Daemon { home } => {
            // Structured JSON logs land in $KERN_HOME/logs/runtime.jsonl
            // (SPEC.md §17 data-dir layout). Both sinks redact secrets.
            let home = home.unwrap_or_else(client::default_home);
            let log_path = home.join("logs").join("runtime.jsonl");
            let _ = kern_core::telemetry::init("info", Some(&log_path));
            daemon::run(Some(home)).await?;
            Ok(0)
        }
        Command::Init { home } => cmd_init(home),
        Command::Run { spec, wait } => cmd_run(&spec, wait).await,
        Command::Doctor => cmd_doctor(home).await,
        Command::Ps => cmd_ps().await,
        Command::Logs {
            target,
            follow,
            limit,
        } => cmd_logs(&target, follow, limit).await,
        Command::Inspect { target } => cmd_inspect(&target).await,
        Command::Schedule { target } => cmd_schedule(&target).await,
        Command::Pause { target } => cmd_lifecycle(&target, "pause", "paused").await,
        Command::Resume { target } => cmd_lifecycle(&target, "resume", "resumed").await,
        Command::Checkpoint { target } => cmd_checkpoint(&target).await,
        Command::Terminate { target } => cmd_lifecycle(&target, "terminate", "terminated").await,
        Command::Tools => cmd_tools().await,
        Command::Models => cmd_models().await,
        Command::Permissions => cmd_permissions().await,
        Command::Grant { request_id } => cmd_decide(&request_id, true).await,
        Command::Deny { request_id } => cmd_decide(&request_id, false).await,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn cmd_version() -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    println!("kern {}", kern_core::version::KERN_VERSION);
    println!(
        "api  http://{}",
        client.base_url().trim_start_matches("http://")
    );
    // Live daemon info when reachable; not an error when it is not.
    if let Ok(health) = client.health().await {
        println!(
            "daemon kern {} (schema v{}, sandbox: {})",
            health["version"].as_str().unwrap_or("?"),
            health["schema_version"].as_i64().unwrap_or(-1),
            health["sandbox"].as_str().unwrap_or("?"),
        );
    } else {
        println!("daemon not running (start it with `kern daemon`)");
    }
    Ok(0)
}

fn cmd_init(home: Option<PathBuf>) -> Result<i32, String> {
    let (home, token, spec) = init::run(home)?;
    println!("created {}", home.display());
    match token {
        Some(path) => println!("wrote API token to {} (bearer auth)", path.display()),
        None => println!(
            "API token already present ({} untouched)",
            home.join("token").display()
        ),
    }
    match spec {
        Some(path) => println!(
            "scaffolded {} — edit it, then `kern run agent.yaml`",
            path.display()
        ),
        None => println!("agent.yaml already exists (left untouched)"),
    }
    println!("start the runtime with `kern daemon`");
    Ok(0)
}

async fn cmd_doctor(home: Option<PathBuf>) -> Result<i32, String> {
    let failed = doctor::run(home).await?;
    Ok(if failed == 0 { 0 } else { 1 })
}

/// `kern run agent.yaml` — validate locally (fast, line-referencing errors),
/// create + start via the API, and with `--wait` tail the run to completion.
async fn cmd_run(spec_path: &PathBuf, wait: bool) -> Result<i32, String> {
    let yaml = std::fs::read_to_string(spec_path)
        .map_err(|e| format!("read {}: {e}", spec_path.display()))?;
    let spec = kern_core::config::parse_agent_spec(&yaml)
        .map_err(|e| format!("invalid {}: {e}", spec_path.display()))?;
    let spec_value = serde_json::to_value(&spec).map_err(|e| format!("serialize spec: {e}"))?;

    let client = Client::from_env().map_err(|e| e.to_string())?;
    let agent = client
        .create_agent(&spec_value)
        .await
        .map_err(display_client)?;
    let id = agent["id"]
        .as_str()
        .ok_or("create response missing id")?
        .to_string();
    let name = agent["name"].as_str().unwrap_or(&id).to_string();
    println!("created agent {name} ({id})");

    let started = client
        .lifecycle(&id, "start")
        .await
        .map_err(display_client)?;
    let execution_id = started["execution_id"]
        .as_str()
        .ok_or("start response missing execution_id")?
        .to_string();
    println!("started execution {execution_id}");

    if !wait {
        return Ok(0);
    }
    run_until_terminal(&client, &id, &name).await
}

/// Poll state + stream events until the agent is terminal.
async fn run_until_terminal(client: &Client, id: &str, name: &str) -> Result<i32, String> {
    let mut after = 0i64;
    let mut hinted_waiting = false;
    loop {
        let agent = client.get_agent(id).await.map_err(display_client)?;
        let state = agent["lifecycle_state"].as_str().unwrap_or("?").to_string();

        let events = client
            .agent_events(id, after, 200)
            .await
            .map_err(display_client)?;
        for event in &events {
            if let Some(seq) = event["seq"].as_i64() {
                after = after.max(seq + 1);
            }
            println!("  {}", table::format_event(event));
        }

        match state.as_str() {
            "completed" => {
                println!("agent {name} completed");
                return Ok(0);
            }
            "failed" | "terminated" => {
                if let Some(last_error) = agent["last_error"].as_str() {
                    if !last_error.is_empty() {
                        println!("last error: {last_error}");
                    }
                }
                println!("agent {name} {state}");
                return Ok(1);
            }
            "waiting" if !hinted_waiting => {
                hinted_waiting = true;
                println!(
                    "agent is waiting for a permission decision — see `kern permissions` and `kern grant <id>`"
                );
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn cmd_ps() -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let agents = client.list_agents().await.map_err(display_client)?;
    if agents.is_empty() {
        println!("no agents (create one with `kern run agent.yaml`)");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = agents
        .iter()
        .map(|a| {
            vec![
                a["name"].as_str().unwrap_or("?").to_string(),
                table::short_id(a["id"].as_str().unwrap_or("?")),
                a["lifecycle_state"].as_str().unwrap_or("?").to_string(),
                a["updated_at"].as_str().unwrap_or("?").to_string(),
            ]
        })
        .collect();
    table::print_table(&["NAME", "ID", "STATE", "UPDATED"], &rows);
    Ok(0)
}

async fn cmd_logs(target: &str, follow: bool, limit: usize) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let id = client.resolve_agent(target).await.map_err(display_client)?;
    let events = client
        .agent_events(&id, 0, limit)
        .await
        .map_err(display_client)?;
    let mut after = 0i64;
    for event in &events {
        if let Some(seq) = event["seq"].as_i64() {
            after = after.max(seq + 1);
        }
        println!("{}", table::format_event(event));
    }
    if !follow {
        return Ok(0);
    }
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = client
            .agent_events(&id, after, 200)
            .await
            .map_err(display_client)?;
        for event in &events {
            if let Some(seq) = event["seq"].as_i64() {
                after = after.max(seq + 1);
            }
            println!("{}", table::format_event(event));
        }
    }
}

async fn cmd_inspect(target: &str) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let id = client.resolve_agent(target).await.map_err(display_client)?;
    let agent = client.get_agent(&id).await.map_err(display_client)?;
    let checkpoints = client.list_checkpoints(&id).await.map_err(display_client)?;

    println!("agent {}", agent["name"].as_str().unwrap_or("?"));
    println!("  id:             {}", agent["id"].as_str().unwrap_or("?"));
    println!(
        "  state:          {}",
        agent["lifecycle_state"].as_str().unwrap_or("?")
    );
    if let Some(next) = agent["next_run_at"].as_str() {
        println!(
            "  next_run_at:    {} ({})",
            next,
            table::relative_time(next, chrono::Utc::now())
        );
    }
    println!(
        "  created:        {}",
        agent["created_at"].as_str().unwrap_or("?")
    );
    println!(
        "  updated:        {}",
        agent["updated_at"].as_str().unwrap_or("?")
    );
    let last_error = agent["last_error"].as_str().unwrap_or("");
    println!(
        "  last_error:     {}",
        if last_error.is_empty() {
            "(none)"
        } else {
            last_error
        }
    );
    println!(
        "  executions:     {}   checkpoints: {}",
        agent["execution_count"].as_i64().unwrap_or(0),
        agent["checkpoint_count"].as_i64().unwrap_or(0)
    );
    if let Some(latest) = checkpoints.first() {
        println!(
            "  latest cp:      seq {} ({})",
            latest["seq"].as_i64().unwrap_or(0),
            latest["created_at"].as_str().unwrap_or("?")
        );
    }

    // Compact spec summary from the stored config.
    if let Some(config) = agent["config"].as_object() {
        if let Some(model) = config.get("model") {
            println!(
                "  model:          {} / {}",
                model["provider"].as_str().unwrap_or("?"),
                model["model"].as_str().unwrap_or("?")
            );
        }
        if let Some(tools) = config.get("tools").and_then(string_array) {
            println!("  tools:          {}", tools.join(", "));
        }
        let schedule = describe_schedule(config.get("schedule"));
        println!("  schedule:       {schedule}");
    }
    Ok(0)
}

fn string_array(v: &serde_json::Value) -> Option<Vec<String>> {
    v.as_array().map(|arr| {
        arr.iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect()
    })
}

async fn cmd_schedule(target: &str) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let id = client.resolve_agent(target).await.map_err(display_client)?;
    let agent = client.get_agent(&id).await.map_err(display_client)?;
    println!("agent {}", agent["name"].as_str().unwrap_or("?"));
    let schedule = describe_schedule(agent["config"].get("schedule"));
    println!("  schedule:       {schedule}");
    match agent["next_run_at"].as_str() {
        Some(next) => println!(
            "  next run:       {next} ({})",
            table::relative_time(next, chrono::Utc::now())
        ),
        None => println!("  next run:       (never)"),
    }
    Ok(0)
}

/// Human summary of a stored `schedule:` block.
fn describe_schedule(schedule: Option<&serde_json::Value>) -> String {
    let Some(schedule) = schedule else {
        return "(none)".to_string();
    };
    let mut parts = Vec::new();
    if let Some(every) = schedule["every"].as_str() {
        parts.push(format!("every {every}"));
    } else if let Some(cron) = schedule["cron"].as_str() {
        parts.push(format!("cron {cron}"));
    } else if let Some(at) = schedule["at"].as_str() {
        parts.push(format!("at {at}"));
    } else {
        parts.push("(no rule)".to_string());
    }
    if let Some(tz) = schedule["timezone"].as_str() {
        parts.push(format!("tz {tz}"));
    }
    if let Some(skip) = schedule["skip_if_running"].as_bool() {
        if skip {
            parts.push("skip_if_running".to_string());
        }
    }
    parts.join(", ")
}

async fn cmd_lifecycle(target: &str, action: &str, past_tense: &str) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let id = client.resolve_agent(target).await.map_err(display_client)?;
    let name = client
        .get_agent(&id)
        .await
        .map_err(display_client)?
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&id)
        .to_string();
    client
        .lifecycle(&id, action)
        .await
        .map_err(display_client)?;
    println!("agent {name} {past_tense}");
    Ok(0)
}

async fn cmd_checkpoint(target: &str) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let id = client.resolve_agent(target).await.map_err(display_client)?;
    let result = client
        .lifecycle(&id, "checkpoint")
        .await
        .map_err(display_client)?;
    if let Some(checkpoint_id) = result["checkpoint_id"].as_str() {
        println!(
            "checkpoint {checkpoint_id} (seq {})",
            result["seq"].as_i64().unwrap_or(0)
        );
    } else {
        println!("checkpoint queued — the runner will checkpoint at its next safe point");
    }
    Ok(0)
}

async fn cmd_tools() -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let tools = client.tools().await.map_err(display_client)?;
    let rows: Vec<Vec<String>> = tools
        .iter()
        .map(|t| {
            vec![
                t["name"].as_str().unwrap_or("?").to_string(),
                t["permission"].as_str().unwrap_or("?").to_string(),
                t["description"].as_str().unwrap_or("").to_string(),
            ]
        })
        .collect();
    table::print_table(&["NAME", "PERMISSION", "DESCRIPTION"], &rows);
    Ok(0)
}

async fn cmd_models() -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let models = client.models().await.map_err(display_client)?;
    let rows: Vec<Vec<String>> = models
        .iter()
        .map(|m| {
            let configured = m["configured"].as_bool().unwrap_or(false);
            vec![
                m["provider"].as_str().unwrap_or("?").to_string(),
                m["models"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
                if configured { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    table::print_table(&["PROVIDER", "MODELS", "CONFIGURED"], &rows);
    Ok(0)
}

async fn cmd_permissions() -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let pending = client.pending_permissions().await.map_err(display_client)?;
    if pending.is_empty() {
        println!("no pending permission requests");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = pending
        .iter()
        .map(|r| {
            vec![
                r["id"].as_str().unwrap_or("?").to_string(),
                r["agent_id"].as_str().unwrap_or("?").to_string(),
                r["resource"].as_str().unwrap_or("?").to_string(),
                r["action"].as_str().unwrap_or("?").to_string(),
                r["requested_at"].as_str().unwrap_or("?").to_string(),
            ]
        })
        .collect();
    table::print_table(
        &["REQUEST", "AGENT", "RESOURCE", "ACTION", "REQUESTED"],
        &rows,
    );
    Ok(0)
}

async fn cmd_decide(request_id: &str, grant: bool) -> Result<i32, String> {
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let decided = client
        .decide_permission(request_id, grant)
        .await
        .map_err(display_client)?;
    let status = decided["status"].as_str().unwrap_or("?");
    println!(
        "permission request {request_id} {}",
        if status == "granted" {
            "granted"
        } else {
            "denied"
        }
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// Error rendering
// ---------------------------------------------------------------------------

/// Terminal-friendly error rendering: structured API errors stay compact;
/// the daemon-unreachable case gets the actionable hint from `Display`.
fn display_client(err: ClientError) -> String {
    err.to_string()
}
