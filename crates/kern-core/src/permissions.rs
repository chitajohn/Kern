//! Permission engine (SPEC.md §10) — the authoritative policy layer.
//!
//! Evaluation for a request `(resource_class, resource, action)`:
//!
//! 1. Select the rule set for the class (`filesystem`, `network`, `memory`,
//!    `shell`).
//! 2. Match the **most specific** rule (longest canonical path prefix /
//!    exact normalized host / glob with the longest literal prefix).
//! 3. Precedence within a match: `deny` > `ask` > `allow`.
//! 4. No match ⇒ `deny` (default deny).
//! 5. `filesystem`: the target is canonicalized (symlinks followed, `.`/`..`
//!    resolved) — using the SAME `kern_tool::path` helpers as the filesystem
//!    tool, so the two layers cannot drift. Escape from an allowed root ⇒
//!    deny.
//! 6. `network`: host normalized (lowercase, trailing dot stripped,
//!    IDN→punycode, IPv6 bracketed, optional `:port`). Host rules are EXACT
//!    (the spec says "exact host"); wildcards are rejected at construction.
//! 7. `memory`: glob-match keys; a key must match an allow rule to pass.
//!
//! The engine is authoritative: tools enforce their own containment as
//! defense in depth, but the runtime never executes a tool that the engine
//! denies, and `ask` routes to the human approval flow (`agent.waiting`).

use std::path::{Path, PathBuf};

use glob::Pattern;

use crate::config::{NetworkRules, PermissionsConfig, RuleList};
use crate::error::{ErrorCode, KernError, Result};

/// The effect a matching rule produces. Precedence: `deny` > `ask` > `allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    fn beats(self, other: Effect) -> bool {
        (self as u8) > (other as u8)
    }
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Effect::Allow => "allow",
            Effect::Ask => "ask",
            Effect::Deny => "deny",
        })
    }
}

/// The engine's verdict for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub effect: Effect,
    pub reason: String,
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        self.effect == Effect::Allow
    }

    pub fn is_ask(&self) -> bool {
        self.effect == Effect::Ask
    }

    pub fn is_deny(&self) -> bool {
        self.effect == Effect::Deny
    }
}

/// Filesystem action dimension (`filesystem:read` / `filesystem:write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAction {
    Read,
    Write,
}

/// Memory action dimension (`memory:read` / `memory:write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Read,
    Write,
}

/// A compiled path rule: either a canonical literal prefix or a glob.
#[derive(Debug, Clone)]
enum PathRule {
    /// Literal: `target.starts_with(root)`, component-wise.
    Prefix {
        root: PathBuf,
        specificity: usize,
        effect: Effect,
        pattern: String,
    },
    /// Glob: `Pattern::matches_path(target)`. `root` is the canonicalized
    /// literal prefix of the pattern (what `fs_roots` hands the filesystem
    /// tool for containment); it must agree with what the compiled glob
    /// matches, or a symlinked ancestor (macOS /var -> /private/var) would
    /// make the tool's root and the engine's rule disagree.
    Glob {
        glob: Pattern,
        root: PathBuf,
        specificity: usize,
        effect: Effect,
        pattern: String,
    },
}

impl PathRule {
    fn specificity(&self, target: &Path) -> Option<usize> {
        match self {
            PathRule::Prefix {
                root, specificity, ..
            } => {
                if kern_tool::path::is_within(target, root) {
                    Some(*specificity)
                } else {
                    None
                }
            }
            PathRule::Glob {
                glob, specificity, ..
            } => {
                if glob.matches_path(target) {
                    Some(*specificity)
                } else {
                    None
                }
            }
        }
    }
}

/// A compiled host rule (exact match on the normalized host).
#[derive(Debug, Clone)]
struct HostRule {
    host: String,
    effect: Effect,
}

/// A compiled memory glob rule.
#[derive(Debug, Clone)]
struct GlobRule {
    glob: Pattern,
    specificity: usize,
    effect: Effect,
    pattern: String,
}

impl GlobRule {
    fn specificity(&self, key: &str) -> Option<usize> {
        if self.glob.matches(key) {
            Some(self.specificity)
        } else {
            None
        }
    }
}

/// The compiled, immutable policy for one agent.
#[derive(Debug, Clone)]
pub struct PermissionEngine {
    fs_read: Vec<PathRule>,
    fs_write: Vec<PathRule>,
    network: Vec<HostRule>,
    mem_read: Vec<GlobRule>,
    mem_write: Vec<GlobRule>,
    shell_allowed: bool,
    /// The agent's workspace root: the base for relative path RULES, and the
    /// fallback base for relative path TARGETS when no rule root exists.
    workspace: PathBuf,
}

/// Characters that make a rule pattern a glob rather than a literal.
fn is_glob_char(c: char) -> bool {
    matches!(c, '*' | '?' | '[')
}

/// Whether a host rule string carries a glob metacharacter outside a
/// bracketed IPv6 literal. `[2001:db8::1]:8080` is exact-host syntax — the
/// brackets delimit the address, they are not a character class — while any
/// other `[` is a glob class start and `*`/`?` are always globs. Bracketed
/// IPv6 rules were previously rejected as "wildcards", making IPv6+port
/// rules unconfigurable.
fn host_rule_has_glob(raw: &str) -> bool {
    if !raw.contains('[') {
        return raw.contains('*') || raw.contains('?');
    }
    if let Some(rest) = raw.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let after = &rest[end + 1..];
            if !host.is_empty() && host.contains(':') {
                // Bracketed IPv6 literal: only the port part may carry globs.
                return after.contains('*') || after.contains('?') || after.contains('[');
            }
        }
    }
    // Any other `[` is a glob class.
    true
}

/// Specificity: the number of literal characters before the first glob
/// metacharacter. Longer literal prefix ⇒ more specific. Uniform across
/// path, host, and key rules so mixed rule sets compare consistently.
fn literal_prefix_len(pattern: &str) -> usize {
    pattern
        .char_indices()
        .find(|(_, c)| is_glob_char(*c))
        .map(|(i, _)| i)
        .unwrap_or(pattern.len())
}

impl PermissionEngine {
    /// Compile the agent's `permissions:` block against `workspace` (the
    /// agent's workspace root — relative path rules resolve against it).
    /// Invalid rules (bad host, unusable glob) fail with `CONFIG_INVALID`:
    /// configuration must be explicit and validated.
    pub fn from_config(permissions: &PermissionsConfig, workspace: &Path) -> Result<Self> {
        let fs = permissions.filesystem.as_ref();
        Ok(Self {
            fs_read: build_path_rules(fs.and_then(|f| f.read.as_ref()), workspace)?,
            fs_write: build_path_rules(fs.and_then(|f| f.write.as_ref()), workspace)?,
            network: build_host_rules(permissions.network.as_ref())?,
            mem_read: build_glob_rules(permissions.memory.as_ref().and_then(|m| m.read.as_ref()))?,
            mem_write: build_glob_rules(
                permissions.memory.as_ref().and_then(|m| m.write.as_ref()),
            )?,
            shell_allowed: permissions.shell_enabled(),
            workspace: workspace.to_path_buf(),
        })
    }

    /// Evaluate a filesystem access. The target is canonicalized with the
    /// same helpers the filesystem tool uses; symlink escapes land outside
    /// every allowed root and are denied.
    ///
    /// A relative target resolves against the FIRST allow/ask root of the
    /// action (mirroring the filesystem tool's "first root" rule, SPEC §11.3),
    /// falling back to the workspace root when no root exists — never the
    /// daemon's cwd.
    pub fn evaluate_path(&self, path: &Path, action: FsAction) -> Decision {
        let rules = match action {
            FsAction::Read => &self.fs_read,
            FsAction::Write => &self.fs_write,
        };
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base = self
                .fs_roots(action)
                .into_iter()
                .next()
                .unwrap_or_else(|| self.workspace.clone());
            base.join(path)
        };
        let target = match kern_tool::path::canonicalize_path(&abs) {
            Ok(target) => target,
            Err(e) => return deny(format!("cannot resolve path '{}': {e}", path.display())),
        };
        decide_path(rules, &target)
    }

    /// Evaluate a network request against a host (`host[:port]`, case- and
    /// IDN-normalized identically to rule construction). Port semantics:
    /// a rule **without** a port matches any port on that host; a rule
    /// **with** a port (`api.github.com:443`) matches only requests to that
    /// exact port. A request whose port is unknown matches only port-less
    /// rules (fail-closed). The engine passes `host:port` (default port
    /// filled from the URL scheme), so port-scoped rules are enforceable.
    pub fn evaluate_host(&self, host: &str) -> Decision {
        let Some(normalized) = normalize_host(host) else {
            return deny(format!("invalid host {host:?}"));
        };
        let (req_host, req_port) = split_host_port(&normalized);
        let mut best: Option<(usize, Effect, String)> = None;
        for rule in &self.network {
            let (rule_host, rule_port) = split_host_port(&rule.host);
            if rule_host == req_host && (rule_port.is_none() || rule_port == req_port) {
                pick_best(&mut best, 1, rule.effect, &rule.host);
            }
        }
        finish(best, "network")
    }

    /// Evaluate a memory key access (glob rules).
    pub fn evaluate_key(&self, key: &str, action: KeyAction) -> Decision {
        let rules = match action {
            KeyAction::Read => &self.mem_read,
            KeyAction::Write => &self.mem_write,
        };
        let mut best: Option<(usize, Effect, String)> = None;
        for rule in rules {
            if let Some(sp) = rule.specificity(key) {
                pick_best(&mut best, sp, rule.effect, &rule.pattern);
            }
        }
        finish(best, "memory")
    }

    /// Whether the `shell` tool is enabled for this agent (the sandbox gate
    /// is separate — SPEC §12 fail-closed construction lives in `sandbox`).
    pub fn shell_allowed(&self) -> bool {
        self.shell_allowed
    }

    /// The normalized exact hosts the `http` tool's allowlist is built from
    /// (defense in depth — the engine's rule matching stays authoritative).
    /// Ask rules are included: a granted ask must pass the tool's own
    /// allowlist too (the engine remains the precise gate).
    pub fn network_allow_hosts(&self) -> Vec<String> {
        self.network
            .iter()
            .filter(|r| matches!(r.effect, Effect::Allow | Effect::Ask))
            .map(|r| r.host.clone())
            .collect()
    }
    /// The literal allow/ask roots for the filesystem tool's containment
    /// check (defense in depth — the engine's rule matching, including
    /// globs, remains authoritative). Ask rules are included: a granted ask
    /// must pass the tool's own root containment too. Glob rules contribute
    /// their literal-prefix directory so `./workspace/**` yields the
    /// workspace root.
    pub fn fs_roots(&self, action: FsAction) -> Vec<PathBuf> {
        let rules = match action {
            FsAction::Read => &self.fs_read,
            FsAction::Write => &self.fs_write,
        };
        let mut roots: Vec<PathBuf> = Vec::new();
        for rule in rules {
            match rule {
                PathRule::Prefix {
                    root,
                    effect: Effect::Allow | Effect::Ask,
                    ..
                } => {
                    if !roots.iter().any(|r| r == root) {
                        roots.push(root.clone());
                    }
                }
                // The stored root is the canonicalized literal prefix of the
                // compiled glob; a pattern with no literal prefix (e.g. `**`)
                // contributes no root.
                PathRule::Glob {
                    root,
                    effect: Effect::Allow | Effect::Ask,
                    ..
                } if !root.as_os_str().is_empty() && !roots.iter().any(|r| r == root) => {
                    roots.push(root.clone());
                }
                _ => {}
            }
        }
        roots
    }
}

fn decide_path(rules: &[PathRule], target: &Path) -> Decision {
    let mut best: Option<(usize, Effect, String)> = None;
    for rule in rules {
        if let Some(sp) = rule.specificity(target) {
            let (effect, pattern) = match rule {
                PathRule::Prefix {
                    effect, pattern, ..
                } => (*effect, pattern),
                PathRule::Glob {
                    effect, pattern, ..
                } => (*effect, pattern),
            };
            pick_best(&mut best, sp, effect, pattern);
        }
    }
    finish(best, "filesystem")
}

/// Track the most specific rule; on ties, the higher-precedence effect wins.
fn pick_best(best: &mut Option<(usize, Effect, String)>, sp: usize, effect: Effect, pattern: &str) {
    match best {
        None => *best = Some((sp, effect, pattern.to_string())),
        Some((best_sp, best_effect, _)) => {
            if sp > *best_sp || (sp == *best_sp && effect.beats(*best_effect)) {
                *best = Some((sp, effect, pattern.to_string()));
            }
        }
    }
}

fn finish(best: Option<(usize, Effect, String)>, class: &str) -> Decision {
    match best {
        Some((_, effect, pattern)) => Decision {
            effect,
            reason: format!("{class} rule '{pattern}' ({effect})"),
        },
        None => deny(format!("no {class} rule matches (default deny)")),
    }
}

fn deny(reason: impl Into<String>) -> Decision {
    Decision {
        effect: Effect::Deny,
        reason: reason.into(),
    }
}

fn build_path_rules(rules: Option<&RuleList>, workspace: &Path) -> Result<Vec<PathRule>> {
    let Some(rules) = rules else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (effect, entries) in effect_entries(rules) {
        let Some(entries) = entries else { continue };
        for raw in entries {
            let normalized = resolve_pattern(raw, workspace);
            if is_glob_pattern(&normalized) {
                // The literal prefix of a glob must be canonicalized like a
                // target (deepest existing ancestor), or a symlinked ancestor
                // (macOS /var -> /private/var) would make the glob never
                // match the canonicalized evaluation target. The returned
                // root is the canonical prefix directory the filesystem tool
                // gets from `fs_roots`, so the tool's containment and the
                // engine's matching always agree.
                let (canonical, root) = canonicalize_glob_pattern(&normalized);
                let glob = Pattern::new(&canonical).map_err(|e| {
                    config_err(
                        "permissions.filesystem",
                        format!("invalid glob {raw:?}: {e}"),
                    )
                })?;
                out.push(PathRule::Glob {
                    glob,
                    root,
                    specificity: literal_prefix_len(&canonical),
                    effect,
                    pattern: raw.clone(),
                });
            } else {
                let root = kern_tool::path::canonical_root(Path::new(&normalized));
                out.push(PathRule::Prefix {
                    root,
                    specificity: literal_prefix_len(&normalized),
                    effect,
                    pattern: raw.clone(),
                });
            }
        }
    }
    Ok(out)
}

fn build_host_rules(rules: Option<&NetworkRules>) -> Result<Vec<HostRule>> {
    let Some(rules) = rules else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (effect, entries) in [
        (Effect::Allow, rules.allow.as_deref()),
        (Effect::Ask, rules.ask.as_deref()),
        (Effect::Deny, rules.deny.as_deref()),
    ] {
        let Some(entries) = entries else { continue };
        for raw in entries {
            if host_rule_has_glob(raw) {
                return Err(config_err(
                    "permissions.network",
                    format!(
                        "host rule {raw:?} contains a wildcard; network rules are exact hosts \
                         (normalized), use explicit entries instead"
                    ),
                ));
            }
            let host = normalize_host(raw).ok_or_else(|| {
                config_err("permissions.network", format!("invalid host {raw:?}"))
            })?;
            out.push(HostRule { host, effect });
        }
    }
    Ok(out)
}

fn build_glob_rules(rules: Option<&RuleList>) -> Result<Vec<GlobRule>> {
    let Some(rules) = rules else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (effect, entries) in effect_entries(rules) {
        let Some(entries) = entries else { continue };
        for raw in entries {
            let glob = Pattern::new(raw).map_err(|e| {
                config_err("permissions.memory", format!("invalid glob {raw:?}: {e}"))
            })?;
            out.push(GlobRule {
                glob,
                specificity: literal_prefix_len(raw),
                effect,
                pattern: raw.clone(),
            });
        }
    }
    Ok(out)
}

fn effect_entries(rules: &RuleList) -> [(Effect, Option<&Vec<String>>); 3] {
    [
        (Effect::Allow, rules.allow.as_ref()),
        (Effect::Ask, rules.ask.as_ref()),
        (Effect::Deny, rules.deny.as_ref()),
    ]
}

/// Canonicalize the literal prefix of a glob pattern (same ancestor walk as
/// `canonical_root`) and re-append the glob tail, so the compiled glob
/// matches canonicalized evaluation targets. Returns the pattern string and
/// the canonical literal-prefix directory (the containment root). A pattern
/// with no literal prefix is returned unchanged with an empty root.
///
/// The root is the canonical directory itself, not a re-parse of the
/// pattern string: on Windows canonicalization yields a verbatim `\\?\C:`
/// path whose `?` would otherwise be mistaken for a glob metacharacter.
fn canonicalize_glob_pattern(normalized: &str) -> (String, PathBuf) {
    let Some(dir) = glob_prefix_dir(normalized) else {
        return (normalized.to_string(), PathBuf::new());
    };
    let canonical = kern_tool::path::canonical_root(&dir);
    let dir_str = dir.to_string_lossy();
    let tail = &normalized[dir_str.len()..];
    (
        format!("{}{}", canonical.to_string_lossy(), tail),
        canonical,
    )
}

/// Resolve a path-rule pattern against `workspace` and normalize `.`/`..`
/// lexically (symlinks are resolved at canonicalization time, shared with
/// the tool). Glob metacharacters survive the join.
fn resolve_pattern(pattern: &str, workspace: &Path) -> String {
    let p = Path::new(pattern);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    kern_tool::path::normalize_lexically(&abs)
        .to_string_lossy()
        .into_owned()
}

fn is_glob_pattern(pattern: &str) -> bool {
    pattern.chars().any(is_glob_char)
}

/// The directory implied by a glob allow rule's literal prefix, for tool
/// roots: `./workspace/**` → `./workspace`, `./workspace/sub/*.md` →
/// `./workspace/sub`.
fn glob_prefix_dir(pattern: &str) -> Option<PathBuf> {
    let p = Path::new(pattern);
    let mut prefix = PathBuf::new();
    for component in p.components() {
        let name = component.as_os_str().to_string_lossy();
        if name.chars().any(is_glob_char) {
            break;
        }
        prefix.push(component.as_os_str());
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Normalize a host rule or request target (SPEC §10.6): lowercase, strip
/// trailing dots, IDN→punycode, bracket IPv6 literals, keep an optional
/// numeric `:port`. Returns `None` for unparseable input.
pub fn normalize_host(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    // Bracket form first: [2001:db8::1] or [2001:db8::1]:443.
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let port = after.strip_prefix(':').map(str::to_string);
        (host, port)
    } else {
        // Plain form. A single colon is host:port (port must be numeric);
        // multiple colons are an unbracketed IPv6 literal (ports then
        // require brackets, per RFC 3986) — never split them.
        match raw.matches(':').count() {
            0 => (raw, None),
            1 => {
                let (h, p) = raw.split_once(':').expect("one colon");
                if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                    return None; // non-numeric port is not a valid host:port
                }
                (h, Some(p.to_string()))
            }
            _ => (raw, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let mut normalized = host.to_ascii_lowercase();
    while normalized.ends_with('.') {
        normalized.pop();
    }
    if normalized.is_empty() {
        return None;
    }
    let host_part = if normalized.contains(':') {
        // IPv6 literal (unbracketed input): wrap it.
        format!("[{normalized}]")
    } else {
        // IDN → punycode; on failure keep the ASCII form (already lowercase).
        idna::domain_to_ascii(&normalized).unwrap_or(normalized)
    };
    match port {
        Some(p) => Some(format!("{host_part}:{p}")),
        None => Some(host_part),
    }
}

/// Split a `normalize_host` output (possibly `host:port` or `[v6]:port`)
/// into its host and optional numeric port. Inputs are already normalized,
/// so only the forms `normalize_host` can produce need to be parsed.
fn split_host_port(normalized: &str) -> (&str, Option<&str>) {
    if let Some(rest) = normalized.strip_prefix('[') {
        // [v6] or [v6]:port
        match rest.find(']') {
            Some(end) => {
                let host = &normalized[..=end];
                let after = &rest[end + 1..];
                let port = after.strip_prefix(':').filter(|p| !p.is_empty());
                (host, port)
            }
            None => (normalized, None),
        }
    } else if let Some((h, p)) = normalized.split_once(':') {
        // host:port (single-colon form from `normalize_host`; the port is
        // numeric by construction). An unbracketed IPv6 literal (multiple
        // colons, no port) falls through to the else arm.
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            (h, Some(p))
        } else {
            (normalized, None)
        }
    } else {
        (normalized, None)
    }
}

fn config_err(field: &str, message: impl Into<String>) -> KernError {
    let mut detail = serde_json::Map::new();
    detail.insert(
        "field".to_string(),
        serde_json::Value::String(field.to_string()),
    );
    KernError::new(
        ErrorCode::ConfigInvalid,
        format!("{field}: {}", message.into()),
    )
    .with_detail(serde_json::Value::Object(detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_agent_spec;
    use std::path::Path;

    fn engine(yaml_permissions: &str) -> PermissionEngine {
        let yaml = format!(
            "version: 1\nname: t\nmodel:\n  provider: mock\n  model: m\ntools:\n  - noop\npermissions:\n{yaml_permissions}\n"
        );
        let spec = parse_agent_spec(&yaml).expect("config must parse");
        PermissionEngine::from_config(&spec.permissions, Path::new("/workspace"))
            .expect("engine must compile")
    }

    #[test]
    fn default_deny_with_no_rules() {
        let e = engine("");
        let p = Path::new("/workspace/x.txt");
        assert!(e.evaluate_path(p, FsAction::Read).is_deny());
        assert!(e.evaluate_host("api.github.com").is_deny());
        assert!(e.evaluate_key("goal", KeyAction::Read).is_deny());
        assert!(!e.shell_allowed());
        assert!(e.fs_roots(FsAction::Read).is_empty());
    }

    #[test]
    fn allow_matches_inside_root_denies_outside() {
        let e = engine("  filesystem:\n    read:\n      allow: [/workspace]\n");
        let inside = Path::new("/workspace/sub/deep/file.txt");
        let d = e.evaluate_path(inside, FsAction::Read);
        assert!(d.is_allow(), "{d:?}");
        // Same rule set is per-action: write has no rules.
        assert!(e.evaluate_path(inside, FsAction::Write).is_deny());
        // Outside the root.
        let outside = Path::new("/etc/passwd");
        assert!(e.evaluate_path(outside, FsAction::Read).is_deny());
    }

    #[test]
    fn most_specific_rule_wins() {
        let e = engine(
            "  filesystem:\n    read:\n      allow: [/workspace]\n      deny: [/workspace/secret]\n",
        );
        let d = e.evaluate_path(Path::new("/workspace/secret/x"), FsAction::Read);
        assert!(d.is_deny(), "{d:?}");
        assert!(d.reason.contains("/workspace/secret"), "{d:?}");
        // A sibling path still matches the broader allow.
        assert!(e
            .evaluate_path(Path::new("/workspace/notes/x"), FsAction::Read)
            .is_allow());
    }

    #[test]
    fn deny_beats_ask_beats_allow_at_same_specificity() {
        let e = engine(
            "  filesystem:\n    read:\n      allow: [/workspace/shared]\n      ask: [/workspace/shared]\n",
        );
        assert!(e
            .evaluate_path(Path::new("/workspace/shared/x"), FsAction::Read)
            .is_ask());

        let e = engine(
            "  filesystem:\n    read:\n      allow: [/workspace/shared]\n      ask: [/workspace/shared]\n      deny: [/workspace/shared]\n",
        );
        assert!(e
            .evaluate_path(Path::new("/workspace/shared/x"), FsAction::Read)
            .is_deny());
    }

    #[test]
    fn ask_rule_alone_asks() {
        let e = engine("  filesystem:\n    read:\n      ask: [/workspace/shared]\n");
        let d = e.evaluate_path(Path::new("/workspace/shared/x"), FsAction::Read);
        assert!(d.is_ask(), "{d:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_denied_by_engine() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        symlink(outside.path().join("secret.txt"), tmp.path().join("leak")).unwrap();

        let e = engine(&format!(
            "  filesystem:\n    read:\n      allow: [{}]\n",
            tmp.path().display()
        ));
        let d = e.evaluate_path(&tmp.path().join("leak"), FsAction::Read);
        assert!(d.is_deny(), "symlink escape must be denied: {d:?}");
        // The real file, directly, is allowed.
        let direct = outside.path().join("secret.txt");
        assert!(
            e.evaluate_path(&direct, FsAction::Read).is_deny(),
            "outside file must be denied"
        );
    }

    #[test]
    fn glob_path_rules_match_canonical_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/readme.md"), "x").unwrap();
        std::fs::write(ws.join("sub/notes.txt"), "x").unwrap();
        std::fs::write(ws.join("top.md"), "x").unwrap();

        // `**` recurses into subdirectories; `*` alone does not cross `/`.
        let e = engine(&format!(
            "  filesystem:\n    read:\n      allow: [{}/*.md, {}/**/*.md]\n",
            tmp.path().display(),
            tmp.path().display()
        ));
        assert!(e
            .evaluate_path(&ws.join("sub/readme.md"), FsAction::Read)
            .is_allow());
        assert!(e
            .evaluate_path(&ws.join("top.md"), FsAction::Read)
            .is_allow());
        assert!(e
            .evaluate_path(&ws.join("sub/notes.txt"), FsAction::Read)
            .is_deny());
    }

    #[test]
    fn relative_rules_resolve_against_workspace() {
        let e =
            engine("  filesystem:\n    read:\n      allow: [./ws]\n      deny: [./ws/secret]\n");
        assert!(e
            .evaluate_path(Path::new("/workspace/ws/a.txt"), FsAction::Read)
            .is_allow());
        assert!(e
            .evaluate_path(Path::new("/workspace/ws/secret/a.txt"), FsAction::Read)
            .is_deny());
        assert!(e
            .evaluate_path(Path::new("/workspace/other/a.txt"), FsAction::Read)
            .is_deny());
    }

    #[test]
    fn host_normalization_and_exact_match() {
        let e = engine("  network:\n    allow: [api.github.com]\n");
        for ok in [
            "api.github.com",
            "API.GITHUB.COM",
            "api.github.com.",
            "API.GitHub.COM.",
        ] {
            let d = e.evaluate_host(ok);
            assert!(d.is_allow(), "{ok:?} must match: {d:?}");
        }
        assert!(e.evaluate_host("github.com").is_deny());
        assert!(e.evaluate_host("api.github.com.evil.com").is_deny());
    }

    #[test]
    fn host_port_and_ipv6_normalization() {
        let e = engine("  network:\n    allow: [api.github.com:443, 2001:db8::1]\n");
        assert!(e.evaluate_host("api.github.com:443").is_allow());
        assert!(e.evaluate_host("api.github.com:8443").is_deny());
        // Unbracketed IPv6 input normalizes into the bracketed form.
        assert!(e.evaluate_host("2001:db8::1").is_allow());
        assert!(e.evaluate_host("[2001:db8::1]").is_allow());
        // A port-less rule matches any port on the host (consistent with
        // domain hosts: `api.github.com` allows any port). The old behavior
        // denied v6+port requests while allowing domain+any-port — the
        // inconsistency was part of the port-rule bug (F1).
        assert!(e.evaluate_host("[2001:db8::1]:8080").is_allow());

        // A port-scoped IPv6 rule (bracket form, quoted for YAML) matches
        // only that exact port.
        let p = engine("  network:\n    allow: [\"[2001:db8::1]:8080\"]\n");
        assert!(p.evaluate_host("[2001:db8::1]:8080").is_allow());
        assert!(p.evaluate_host("[2001:db8::1]:9090").is_deny());
        assert!(p.evaluate_host("2001:db8::1").is_deny());
    }

    #[test]
    fn idn_hosts_normalize_to_punycode() {
        let e = engine("  network:\n    allow: [xn--bcher-kva.example]\n");
        // bücher.example (IDN) normalizes to the punycode rule.
        assert!(e.evaluate_host("bücher.example").is_allow());
        assert!(e.evaluate_host("BÜCHER.EXAMPLE.").is_allow());
        assert!(e.evaluate_host("bucher.example").is_deny());
    }

    #[test]
    fn network_deny_overrides_allow() {
        let e = engine("  network:\n    allow: [api.github.com]\n    deny: [api.github.com]\n");
        assert!(e.evaluate_host("api.github.com").is_deny());
    }

    #[test]
    fn host_wildcards_rejected_at_construction() {
        // Network rules are exact hosts (SPEC §10: "exact host"); wildcards
        // fail fast at construction rather than silently under-approximating.
        // Quoted: unquoted `*` would be a YAML alias, not our validation.
        let yaml = "version: 1\nname: t\nmodel:\n  provider: mock\n  model: m\ntools:\n  - noop\npermissions:\n  network:\n    allow: [\"*.github.com\"]\n";
        let spec = parse_agent_spec(yaml).unwrap();
        let err =
            PermissionEngine::from_config(&spec.permissions, Path::new("/workspace")).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("wildcard"), "{}", err.message);
    }

    #[test]
    fn memory_glob_rules() {
        let e = engine(
            "  memory:\n    read:\n      allow: [notes.*, \"*\"]\n      deny: [notes.secret]\n    write:\n      allow: [\"*\"]\n",
        );
        assert!(e.evaluate_key("notes.a", KeyAction::Read).is_allow());
        // deny wins at the same specificity as the `notes.*` allow.
        assert!(e.evaluate_key("notes.secret", KeyAction::Read).is_deny());
        assert!(e.evaluate_key("goal", KeyAction::Read).is_allow());
        // Write has its own rule set.
        assert!(e.evaluate_key("anything", KeyAction::Write).is_allow());
        assert!(e.evaluate_key("anything", KeyAction::Read).is_allow());
    }

    #[test]
    fn memory_glob_specificity_prefers_longer_literal() {
        let e = engine("  memory:\n    read:\n      allow: [\"*\"]\n      deny: [notes.*]\n");
        // `notes.*` (6 literal chars) beats `*` (0) for notes keys.
        assert!(e.evaluate_key("notes.a", KeyAction::Read).is_deny());
        assert!(e.evaluate_key("goal", KeyAction::Read).is_allow());
    }

    #[test]
    fn fs_roots_derive_from_allow_rules() {
        // Rules are canonicalized like targets, so the assertions compare
        // against canonicalized paths. A real tempdir keeps this
        // platform-independent: on Windows a non-existent root-relative path
        // walks up to the drive root (`/workspace` -> `C:\workspace`), which
        // would make a literal-path expectation wrong for the wrong reason.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        let ws_str = ws.to_string_lossy().into_owned();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(ws.join("out")).unwrap();
        let e = engine(&format!(
            "  filesystem:\n    read:\n      allow: [{ws_str}, {ws_str}/sub/**]\n      deny: [{ws_str}/secret]\n    write:\n      allow: [{ws_str}/out]\n"
        ));
        let read_roots = e.fs_roots(FsAction::Read);
        assert_eq!(read_roots.len(), 2, "{read_roots:?}");
        let ws_canon = std::fs::canonicalize(&ws).unwrap();
        assert!(read_roots.contains(&ws_canon), "{read_roots:?}");
        assert!(read_roots.contains(&ws_canon.join("sub")), "{read_roots:?}");

        let write_roots = e.fs_roots(FsAction::Write);
        assert_eq!(write_roots, vec![ws_canon.join("out")]);
    }

    #[test]
    fn shell_gating_is_explicit() {
        let yaml = "version: 1\nname: t\nmodel:\n  provider: mock\n  model: m\ntools:\n  - shell\npermissions:\n  shell:\n    enabled: true\n    sandbox: off\n";
        let spec = parse_agent_spec(yaml).unwrap();
        let e = PermissionEngine::from_config(&spec.permissions, Path::new("/workspace")).unwrap();
        assert!(e.shell_allowed());
    }

    #[test]
    fn invalid_host_rule_fails_closed() {
        let yaml = "version: 1\nname: t\nmodel:\n  provider: mock\n  model: m\ntools:\n  - noop\npermissions:\n  network:\n    allow: [\"api.github.com:notaport\"]\n";
        let spec = parse_agent_spec(yaml).unwrap();
        // "api.github.com:notaport" — the suffix is not numeric, so it is
        // treated as part of the host; IDN normalization rejects it.
        let err =
            PermissionEngine::from_config(&spec.permissions, Path::new("/workspace")).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        let _ = err;
    }

    #[test]
    fn normalize_host_edge_cases() {
        assert_eq!(normalize_host("EXAMPLE.com"), Some("example.com".into()));
        assert_eq!(normalize_host("example.com."), Some("example.com".into()));
        assert_eq!(
            normalize_host("example.com:8080"),
            Some("example.com:8080".into())
        );
        assert_eq!(normalize_host("2001:db8::1"), Some("[2001:db8::1]".into()));
        assert_eq!(
            normalize_host("[2001:db8::1]:443"),
            Some("[2001:db8::1]:443".into())
        );
        assert_eq!(normalize_host(""), None);
        assert_eq!(normalize_host("   "), None);
        assert_eq!(normalize_host("."), None);
    }
}
