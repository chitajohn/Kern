//! Plain-text output helpers: aligned tables and compact event rendering.
//!
//! No heavy TUI dependencies (SPEC.md §16): tables are space-aligned columns,
//! events are single human-readable lines.

use serde_json::Value;

/// Render a table with aligned columns (headers + rows of equal length).
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    if headers.is_empty() {
        return;
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let pad = |cell: &str, width: usize| -> String {
        let mut out = String::with_capacity(width);
        out.push_str(cell);
        out.extend(std::iter::repeat_n(' ', width - cell.chars().count()));
        out
    };
    let line: Vec<String> = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| pad(h, *w))
        .collect();
    println!("{}", line.join("  ").trim_end());
    for row in rows {
        let cells: Vec<String> = row.iter().zip(&widths).map(|(c, w)| pad(c, *w)).collect();
        println!("{}", cells.join("  ").trim_end());
    }
}

/// The first 8 characters of an id — enough to tell agents apart in tables.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Render one event as a compact terminal line (`kern logs`, `kern run -w`).
pub fn format_event(event: &Value) -> String {
    let kind = event["kind"].as_str().unwrap_or("?");
    let payload = &event["payload"];
    let detail = match kind {
        "agent.thinking" | "model.thinking" => payload["text"]
            .as_str()
            .map(|t| t.trim().to_string())
            .unwrap_or_default(),
        "agent.completed" => payload["final_text"]
            .as_str()
            .map(|t| t.trim().to_string())
            .unwrap_or_default(),
        "agent.failed" | "execution.failed" | "model.failed" | "checkpoint.failed" => payload
            .get("error")
            .map(|e| truncate(&e.to_string(), 120))
            .unwrap_or_default(),
        "tool.requested" | "tool.started" | "tool.completed" | "tool.failed" => {
            let tool = payload["tool_name"].as_str().unwrap_or("?");
            let call = payload["tool_call_id"].as_str().unwrap_or("?");
            match kind {
                "tool.completed" => {
                    let result = payload
                        .get("result")
                        .map(|r| truncate(&r.to_string(), 120))
                        .unwrap_or_default();
                    format!("{tool} ({call}) -> {result}")
                }
                "tool.failed" => {
                    let error = payload
                        .get("error")
                        .map(|e| truncate(&e.to_string(), 120))
                        .unwrap_or_default();
                    format!("{tool} ({call}) FAILED: {error}")
                }
                _ => {
                    let args = payload
                        .get("args")
                        .map(|a| truncate(&a.to_string(), 80))
                        .unwrap_or_default();
                    format!("{tool} ({call}) {args}")
                }
            }
        }
        "permission.requested" | "permission.ask" => {
            let resource = payload["resource"].as_str().unwrap_or("?");
            let action = payload["action"].as_str().unwrap_or("?");
            format!("{resource}: {action}")
        }
        "execution.completed" => {
            let steps = payload["steps"].as_i64().unwrap_or(0);
            let checkpoints = payload["checkpoints"].as_i64().unwrap_or(0);
            format!("{steps} steps, {checkpoints} checkpoints")
        }
        "checkpoint.created" => {
            let seq = payload["seq"].as_i64().unwrap_or(0);
            format!("seq {seq}")
        }
        "checkpoint.restored" => {
            let seq = payload["seq"].as_i64().unwrap_or(0);
            format!("seq {seq}")
        }
        _ => truncate(&payload.to_string(), 100),
    };
    if detail.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}: {detail}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Human-readable relative time ("in 42s", "3m ago").
pub fn relative_time(when: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(when) else {
        return when.to_string();
    };
    let parsed = parsed.with_timezone(&chrono::Utc);
    let delta = now.signed_duration_since(parsed);
    if delta.num_seconds() >= 0 {
        format!("{} ago", human_duration(delta.num_seconds()))
    } else {
        format!("in {}", human_duration(-delta.num_seconds()))
    }
}

fn human_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
