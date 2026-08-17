//! Structured logging and secret redaction (ARCHITECTURE.md §12.2).
//!
//! Two sinks in v0.1: human-readable console output and a JSON-lines file
//! (`$KERN_HOME/logs/runtime.jsonl`). A redaction layer ensures secrets never
//! reach either sink. Redaction is defense in depth: never log env values,
//! tokens, or `Authorization` headers; tool arguments are only logged under an
//! explicit `log_tool_args` opt-in.
//!
//! The guard is WIRED, not just defined: both formatters route every rendered
//! event line through [`redact`] before writing (SPEC.md §14.4).

use std::fmt;
use std::fs::File;
use std::path::Path;

use tracing::{Event, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{format, FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::error::Result;

/// Environment variables whose values must never appear in logs.
pub const SECRET_ENV_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "KERN_TOKEN",
    "AWS_SECRET_ACCESS_KEY",
    "AZURE_OPENAI_API_KEY",
    "GEMINI_API_KEY",
];

/// Credential prefix patterns, most specific first. A match is only masked when
/// followed by at least `MIN_KEY_LENGTH` key characters, so ordinary words like
/// "skill" are never corrupted.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("sk-ant-", "sk-ant-***"), // Anthropic-style keys
    ("sk-", "sk-***"),         // OpenAI-style keys
    ("Bearer ", "Bearer ***"), // Authorization header values
];

/// Minimum key-ish characters after a pattern prefix before we mask it.
const MIN_KEY_LENGTH: usize = 16;

/// Initialize the global tracing subscriber: console layer + optional JSON file
/// layer. The first call wins; later calls are no-ops (`try_init` semantics).
pub fn init(level: &str, json_log_path: Option<&Path>) -> Result<()> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let console = tracing_subscriber::fmt::layer()
        .event_format(RedactFormat(format::Format::default().with_target(false)))
        .with_filter(filter.clone());

    // `try_init` returns an error only if a subscriber is already installed,
    // which is fine: initialization is idempotent (first call wins).
    let result = match json_log_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::error::KernError::internal(format!(
                        "cannot create log directory {}: {e}",
                        parent.display()
                    ))
                })?;
            }
            let file = File::create(path).map_err(|e| {
                crate::error::KernError::internal(format!(
                    "cannot open log file {}: {e}",
                    path.display()
                ))
            })?;
            let json_layer = tracing_subscriber::fmt::layer()
                .event_format(RedactFormat(format::Format::default().json()))
                .with_writer(std::sync::Mutex::new(file))
                .with_filter(filter);
            tracing_subscriber::registry()
                .with(console)
                .with(json_layer)
                .try_init()
        }
        None => tracing_subscriber::registry().with(console).try_init(),
    };
    let _ = result;
    Ok(())
}

/// A formatter wrapper that routes every rendered event line through
/// [`redact`] before it reaches the sink (SPEC.md §14.4).
///
/// This is the load-bearing redaction guard: `redact` alone cannot help if
/// nothing calls it. Both sinks (console and the JSON log file) wrap their
/// default formatter with this, so a stray env value, token, or
/// `Authorization` header can never be written unredacted — even when a
/// `tracing::error!` embeds it in a field value.
#[derive(Debug, Clone)]
struct RedactFormat<F>(F);

impl<S, N, F> FormatEvent<S, N> for RedactFormat<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Render into a scratch buffer first, redact the complete line, then
        // write it. A single pass over the whole line catches secrets in the
        // message AND in structured field values alike.
        let mut buf = String::new();
        self.0.format_event(ctx, Writer::new(&mut buf), event)?;
        write!(writer, "{}", redact(&buf))
    }
}

/// Redact known secret values from an arbitrary string.
///
/// Strategy, in order:
/// 1. Replace the values of known secret env vars wherever they appear.
/// 2. Mask credential-prefix patterns (`sk-…`, `sk-ant-…`, `Bearer …`) that are
///    followed by enough key characters.
pub fn redact(input: &str) -> String {
    let mut out = input.to_string();

    for key in SECRET_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                out = out.replace(&value, "***");
            }
        }
    }

    for (pattern, replacement) in SECRET_PATTERNS {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        loop {
            match rest.find(pattern) {
                Some(pos) => {
                    result.push_str(&rest[..pos]);
                    let after = pos + pattern.len();
                    let tail = &rest[after..];
                    let end = tail
                        .find(|c: char| {
                            !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
                        })
                        .unwrap_or(tail.len());
                    if end >= MIN_KEY_LENGTH {
                        result.push_str(replacement);
                        rest = &tail[end..];
                    } else {
                        // Too short to be a credential: keep the literal prefix.
                        result.push_str(&rest[pos..after]);
                        rest = tail;
                    }
                }
                None => {
                    result.push_str(rest);
                    break;
                }
            }
        }
        out = result;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env mutation is not thread-safe: serialize the env-touching tests (the
    // redaction layer reads env at format time, so a racing test's value could
    // mask or unmask another test's secret).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn redacts_env_secret_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        const KEY: &str = "sk-test-abcdefghijklmnopqrstuvwxyz123456";
        std::env::set_var("OPENAI_API_KEY", KEY);
        let input = format!("the key is {KEY} and then more");
        let out = redact(&input);
        assert!(!out.contains(KEY), "secret leaked: {out}");
        assert!(out.contains("***"));
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn redacts_openai_style_keys() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz123456";
        assert_eq!(redact(&format!("key={key} done")), "key=sk-*** done");
    }

    #[test]
    fn redacts_anthropic_style_keys() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456";
        assert_eq!(redact(&format!("key={key} done")), "key=sk-ant-*** done");
    }

    #[test]
    fn redacts_bearer_tokens() {
        let token = "abcdefghijklmnopqrstuvwxyz123456";
        assert_eq!(
            redact(&format!("Authorization: Bearer {token}")),
            "Authorization: Bearer ***"
        );
    }

    #[test]
    fn does_not_corrupt_ordinary_words() {
        assert_eq!(redact("skill"), "skill");
        assert_eq!(redact("sketchbook"), "sketchbook");
        assert_eq!(redact("bearer of bad news"), "bearer of bad news");
    }

    #[test]
    fn writes_json_logs_to_file() {
        // One init per test process: the global subscriber is installed on the
        // first call, so both the normal-message and the redaction assertions
        // live in the SAME test (a second parallel init would race for the
        // subscriber and its log file would stay empty).
        let _guard = ENV_LOCK.lock().unwrap();
        const KEY: &str = "sk-test-layer-redaction-abcdefghijklmnop";
        std::env::set_var("OPENAI_API_KEY", KEY);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("runtime.jsonl");
        init("debug", Some(&path)).unwrap();

        tracing::info!(agent_id = "abc", "hello from test");
        // The guard must be WIRED, not just defined: a message carrying an env
        // secret value must not reach the log file even in a field value.
        tracing::info!(value = %KEY, "connecting with provider key");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("hello from test"),
            "missing message: {content}"
        );
        assert!(content.contains("abc"), "missing field: {content}");
        assert!(
            !content.contains(KEY),
            "the log layer leaked the secret: {content}"
        );
        assert!(
            content.contains("***"),
            "expected the redaction marker in the log: {content}"
        );
        std::env::remove_var("OPENAI_API_KEY");
    }
}
