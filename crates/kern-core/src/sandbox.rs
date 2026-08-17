//! Sandbox backends (SPEC.md §12) — the OS-level isolation layer for the
//! `shell` tool.
//!
//! | Platform | Backend | `sandbox: required` without it |
//! |----------|---------|--------------------------------|
//! | Linux | `bubblewrap` (`bwrap`) | agent fails to start (`SandboxError::Unavailable`) |
//! | Linux (no `bwrap`) | **`landlock`** (kernel LSM: read-everywhere, write only
//!   workspace + `/tmp`; in-process, no external binary) | agent fails to start |
//! | Linux (no `bwrap`, no landlock) | rlimits only (`best-effort` fallback) | agent fails to start |
//! | macOS | `sandbox-exec` (seatbelt) | agent fails to start |
//! | Windows | none in v0.1 | agent fails to start |
//!
//! Construction is **fail-closed**: `required` demands the strongest backend
//! for the platform; `best-effort` degrades to rlimits (Linux) or a logged
//! no-op; `off` is an explicit operator choice with no isolation.
//!
//! Honest limitations (documented in README, not papered over):
//! - Linux `bwrap`: namespaces + read-only root + writable workspace +
//!   rlimits + dropped capabilities + `--die-with-parent`. A seccomp filter
//!   is NOT applied in v0.1.
//! - Linux `landlock` (the no-`bwrap` tier, Linux ≥ 5.13): kernel-enforced
//!   write containment (read anywhere, write only the agent workspace and
//!   `/tmp`) plus the rlimits below. Landlock has NO network domain and no
//!   memory caps — a hostile shell can still `curl` anywhere it can reach;
//!   namespaces (bwrap) remain the only full isolation.
//! - The rlimit fallback limits CPU/FSIZE/NOFILE (and AS when a tool memory
//!   cap is configured) only — no network isolation.
//! - macOS `sandbox-exec` is deprecated by Apple and this profile is
//!   untested on CI (no macOS runner) — treated accordingly.
//! - Windows has no backend in v0.1; `required` fails closed.
//!
//! The `filesystem` and `http` tools enforce their own path/host constraints
//! on ALL platforms (defense in depth), independent of this layer.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kern_tool::builtins::shell::shell_spec;
use kern_tool::{CommandOutput, CommandRequest, CommandRunner, RunLimits, ToolError};

use crate::config::SandboxMode;
use crate::error::{ErrorCode, KernError};

/// Structured sandbox failure (SPEC §13: `SANDBOX_UNAVAILABLE` /
/// `SANDBOX_FAILURE`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox backend unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox failure: {0}")]
    Failed(String),
}

impl SandboxError {
    pub fn code(&self) -> &'static str {
        match self {
            SandboxError::Unavailable(_) => "SANDBOX_UNAVAILABLE",
            SandboxError::Failed(_) => "SANDBOX_FAILURE",
        }
    }
}

impl From<SandboxError> for KernError {
    fn from(err: SandboxError) -> Self {
        let code = match &err {
            SandboxError::Unavailable(_) => ErrorCode::SandboxUnavailable,
            SandboxError::Failed(_) => ErrorCode::SandboxFailure,
        };
        KernError::new(code, err.to_string())
    }
}

/// A platform isolation backend.
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &'static str;
    fn capability(&self) -> &'static str;
    /// Rewrite a command for execution under this backend: `wrap` returns
    /// the wrapper program + arguments (e.g. `bwrap ... -- sh -c cmd`).
    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError>;
    /// Child-side setup attached before exec (e.g. `pre_exec` rlimits).
    /// Runs in the child after fork, before exec.
    fn prepare_child(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        let _ = cmd;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Linux: bubblewrap
// ---------------------------------------------------------------------------

/// `bwrap` backend (SPEC §12): namespaces, read-only root, writable
/// workspace only, rlimits, dropped capabilities, `--die-with-parent`.
pub struct LinuxBwrap {
    workspace: PathBuf,
    cpu_seconds: u64,
    fsize_bytes: u64,
    memory_bytes: Option<u64>,
}

impl LinuxBwrap {
    pub fn new(workspace: PathBuf, cpu_seconds: u64, fsize_bytes: u64) -> Self {
        Self {
            workspace,
            cpu_seconds,
            fsize_bytes,
            memory_bytes: None,
        }
    }

    pub fn with_memory_limit(mut self, bytes: Option<u64>) -> Self {
        self.memory_bytes = bytes;
        self
    }
}

impl Sandbox for LinuxBwrap {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn capability(&self) -> &'static str {
        "namespaces (net/pid/mount/ipc/uts), read-only root, writable workspace, \
         rlimits, dropped capabilities, die-with-parent; no seccomp filter in v0.1"
    }

    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError> {
        let path =
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
        let workspace = self.workspace.to_string_lossy().into_owned();

        let mut wrapper: Vec<OsString> = vec![
            "--die-with-parent".into(),
            "--unshare-all".into(),
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--bind".into(),
            workspace.clone().into(),
            workspace.clone().into(),
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--clearenv".into(),
            "--setenv".into(),
            "PATH".into(),
            path,
            "--setenv".into(),
            "HOME".into(),
            workspace.into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--rlimit".into(),
            "NOFILE".into(),
            "256".into(),
            "--rlimit".into(),
            "FSIZE".into(),
            self.fsize_bytes.to_string().into(),
            "--rlimit".into(),
            "CPU".into(),
            self.cpu_seconds.to_string().into(),
        ];
        if let Some(bytes) = self.memory_bytes {
            wrapper.push("--rlimit".into());
            wrapper.push("AS".into());
            wrapper.push(bytes.to_string().into());
        }
        wrapper.push("--".into());
        wrapper.push(program.to_os_string());
        wrapper.extend(args.iter().cloned());
        Ok((OsString::from("bwrap"), wrapper))
    }
}

// ---------------------------------------------------------------------------
// Linux: rlimit fallback (best-effort only)
// ---------------------------------------------------------------------------

/// Best-effort fallback when `bwrap` is missing (SPEC §12): CPU, file-size,
/// and fd-count limits applied in the child before exec. No network or
/// memory isolation — documented honestly.
///
/// Linux-only: `prepare_child` uses `setrlimit`/`pre_exec`, which do not
/// exist on other platforms (the weakest tier is `None` there).
#[cfg(target_os = "linux")]
pub struct LinuxRlimits {
    cpu_seconds: u64,
    fsize_bytes: u64,
    nofile: u64,
    memory_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
impl Default for LinuxRlimits {
    fn default() -> Self {
        Self {
            cpu_seconds: 60,
            fsize_bytes: 16 * 1024 * 1024,
            nofile: 512,
            memory_bytes: None,
        }
    }
}

#[cfg(target_os = "linux")]
impl Sandbox for LinuxRlimits {
    fn name(&self) -> &'static str {
        "rlimits"
    }

    fn capability(&self) -> &'static str {
        "rlimits only (CPU, file size, fd count, optional address space); no network isolation"
    }

    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError> {
        Ok((program.to_os_string(), args.to_vec()))
    }

    fn prepare_child(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        use std::os::unix::process::CommandExt;
        let cpu = self.cpu_seconds;
        let fsize = self.fsize_bytes;
        let nofile = self.nofile;
        let memory = self.memory_bytes;
        // SAFETY: `pre_exec` runs in the child after fork, before exec.
        // Only async-signal-safe operations are permitted there; `setrlimit`
        // qualifies. Any error aborts the exec and fails the spawn.
        unsafe {
            cmd.pre_exec(move || {
                setrlimit_safe(libc::RLIMIT_CPU, cpu, cpu)?;
                setrlimit_safe(libc::RLIMIT_FSIZE, fsize, fsize)?;
                setrlimit_safe(libc::RLIMIT_NOFILE, nofile, nofile)?;
                if let Some(bytes) = memory {
                    setrlimit_safe(libc::RLIMIT_AS, bytes, bytes)?;
                }
                Ok(())
            });
        }
        Ok(())
    }
}

// The resource-constant type differs per platform (`c_uint` on Linux gnu,
// `c_int` on BSD/macOS), so the helper is parameterized per target.
#[cfg(target_os = "linux")]
fn setrlimit_safe(
    resource: libc::__rlimit_resource_t,
    soft: u64,
    hard: u64,
) -> std::io::Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: standard libc call with a valid rlimit struct.
    let rc = unsafe { libc::setrlimit(resource, &rlim) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------
// macOS: seatbelt (sandbox-exec)
// ---------------------------------------------------------------------------

/// Seatbelt backend via `sandbox-exec` (SPEC §12; deprecated by Apple,
/// documented). The generated profile denies by default, allows reads
/// everywhere, writes only in the workspace, and denies all network.
pub struct MacSeatbelt {
    workspace: PathBuf,
}

impl Sandbox for MacSeatbelt {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn capability(&self) -> &'static str {
        "seatbelt profile: deny-default, read-only root except workspace, no network \
         (sandbox-exec is deprecated by Apple and untested on CI)"
    }

    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError> {
        let workspace = self.workspace.to_string_lossy().into_owned();
        let profile = format!(
            "(version 1)\n\
             (deny default)\n\
             (import \"system.sb\")\n\
             (allow process-exec)\n\
             (allow process-fork)\n\
             (allow file-read*)\n\
             (allow file-write* (subpath \"{workspace}\"))\n\
             (deny network*)\n\
             (allow mach-lookup (global-name \"com.apple.system.logger\"))\n"
        );
        let mut wrapper: Vec<OsString> = vec!["-p".into(), profile.into(), program.to_os_string()];
        wrapper.extend(args.iter().cloned());
        Ok((OsString::from("sandbox-exec"), wrapper))
    }
}

// ---------------------------------------------------------------------------
// No sandbox
// ---------------------------------------------------------------------------

/// Explicit `sandbox: off` (operator choice) or the honest no-backend
/// result on platforms with no v0.1 backend.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn capability(&self) -> &'static str {
        "no OS-level isolation (explicit `sandbox: off` or unavailable backend)"
    }

    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError> {
        Ok((program.to_os_string(), args.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Linux: Landlock (kernel LSM, no external binary)
// ---------------------------------------------------------------------------

/// Raw Landlock syscall bindings (Linux ≥ 5.13). Designed async-signal-safe:
/// rule attributes live on the stack and constant paths are passed as static
/// NUL-terminated byte strings, so nothing in the child allocates (error
/// formatting allocates, which is benign in practice — glibc/musl reset the
/// malloc lock at fork). ABI negotiation happens once, in the daemon, never
/// in the child.
#[cfg(target_os = "linux")]
mod landlock {
    use std::io;
    use std::sync::OnceLock;

    const RULE_PATH_BENEATH: u32 = 1;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    // Access-right bits (stable kernel ABI values).
    const ACCESS_EXECUTE: u64 = 1 << 0;
    const ACCESS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_READ_FILE: u64 = 1 << 2;
    const ACCESS_READ_DIR: u64 = 1 << 3;
    const ACCESS_REMOVE_DIR: u64 = 1 << 4;
    const ACCESS_REMOVE_FILE: u64 = 1 << 5;
    const ACCESS_MAKE_CHAR: u64 = 1 << 6;
    const ACCESS_MAKE_DIR: u64 = 1 << 7;
    const ACCESS_MAKE_REG: u64 = 1 << 8;
    const ACCESS_MAKE_SOCK: u64 = 1 << 9;
    const ACCESS_MAKE_FIFO: u64 = 1 << 10;
    const ACCESS_MAKE_BLOCK: u64 = 1 << 11;
    const ACCESS_MAKE_SYM: u64 = 1 << 12;
    // ABI 2 (5.19): rename/link. ABI 3 (6.2): truncate. ABI 4 (6.7): ioctl dev.
    const ACCESS_REFER: u64 = 1 << 13;
    const ACCESS_TRUNCATE: u64 = 1 << 14;
    const ACCESS_IOCTL_DEV: u64 = 1 << 15;

    /// All ABI-1 rights (read + write + create + remove).
    fn base_mask() -> u64 {
        ACCESS_EXECUTE
            | ACCESS_WRITE_FILE
            | ACCESS_READ_FILE
            | ACCESS_READ_DIR
            | ACCESS_REMOVE_DIR
            | ACCESS_REMOVE_FILE
            | ACCESS_MAKE_CHAR
            | ACCESS_MAKE_DIR
            | ACCESS_MAKE_REG
            | ACCESS_MAKE_SOCK
            | ACCESS_MAKE_FIFO
            | ACCESS_MAKE_BLOCK
            | ACCESS_MAKE_SYM
    }

    /// The access-rights mask this kernel can handle, probed **empirically**
    /// and cached. The ABI-version probe alone is not trustworthy (some
    /// kernels report a version whose bits they then reject — observed on
    /// 6.8.0-87-generic, which claims ABI 4 but refuses IOCTL_DEV), so we
    /// attempt `create_ruleset` with descending masks and keep the largest
    /// that works. `None` = unsupported or blocked by an outer sandbox.
    pub fn available_mask() -> Option<u64> {
        static MASK: OnceLock<Option<u64>> = OnceLock::new();
        *MASK.get_or_init(|| {
            let candidates = [
                base_mask() | ACCESS_REFER | ACCESS_TRUNCATE | ACCESS_IOCTL_DEV,
                base_mask() | ACCESS_REFER | ACCESS_TRUNCATE,
                base_mask() | ACCESS_REFER,
                base_mask(),
            ];
            candidates
                .into_iter()
                .find(|&mask| create_ruleset(mask).is_ok())
        })
    }

    pub fn create_ruleset(handled: u64) -> io::Result<i32> {
        let attr = RulesetAttr {
            handled_access_fs: handled,
            ..Default::default()
        };
        // SAFETY: `attr` is a valid, properly-zeroed kernel struct of the
        // expected size; the syscall copies it before returning.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd as i32)
        }
    }

    /// Add a path-beneath rule. `path` must be a static NUL-terminated byte
    /// string or a captured `CString` — no allocation happens here.
    fn add_path_rule(ruleset: i32, allowed: u64, path: *const libc::c_char) -> io::Result<()> {
        // SAFETY: `path` is valid NUL-terminated; open is async-signal-safe.
        let fd = unsafe { libc::open(path, libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let attr = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: fd,
        };
        // SAFETY: `ruleset` is a live ruleset fd and `attr` is a valid struct.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        // SAFETY: closing the fd we opened above.
        unsafe { libc::close(fd) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn restrict_self(ruleset: i32) -> io::Result<()> {
        // SAFETY: plain syscall with a valid ruleset fd.
        let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Apply a read-everywhere / write-only-`writable` policy to the CURRENT
    /// process (and, by inheritance, its children). Runs inside `pre_exec`.
    /// Any failure is a hard error so the spawn fails closed.
    pub fn apply_policy(handled: u64, writable: &[std::ffi::CString]) -> io::Result<()> {
        let read_exec = ACCESS_EXECUTE | ACCESS_READ_FILE | ACCESS_READ_DIR;
        let ruleset = create_ruleset(handled)?;
        let result = (|| {
            // Read/execute everywhere: a single rule on "/" covers the tree.
            add_path_rule(ruleset, read_exec, c"/".as_ptr())?;
            for path in writable {
                add_path_rule(ruleset, handled, path.as_ptr())?;
            }
            // Commands expect a writable null sink. On ABI ≥ 3 every `>`
            // redirect opens O_TRUNC, which needs the TRUNCATE right.
            let null_access = ACCESS_WRITE_FILE | (handled & ACCESS_TRUNCATE);
            add_path_rule(ruleset, null_access, c"/dev/null".as_ptr())?;
            restrict_self(ruleset)
        })();
        // SAFETY: closing the ruleset fd we own.
        unsafe { libc::close(ruleset) };
        result
    }
}

/// Landlock backend (Linux ≥ 5.13, no external binary): kernel-enforced
/// write containment for the shell tool. Read/execute allowed anywhere;
/// writes only in the agent workspace, `/tmp`, and `/dev/null`; plus the
/// same rlimits as the fallback backend. No network or memory isolation
/// (Landlock has no network domain) — documented honestly.
#[cfg(target_os = "linux")]
pub struct LinuxLandlock {
    _workspace: PathBuf,
    cpu_seconds: u64,
    fsize_bytes: u64,
    nofile: u64,
    memory_bytes: Option<u64>,
    handled: u64,
    writable: Vec<std::ffi::CString>,
}

#[cfg(target_os = "linux")]
impl LinuxLandlock {
    /// `None` when the running kernel does not offer a usable Landlock mask.
    pub fn new(
        workspace: PathBuf,
        cpu_seconds: u64,
        fsize_bytes: u64,
        nofile: u64,
    ) -> Option<Self> {
        Self::with_memory_limit(workspace, cpu_seconds, fsize_bytes, nofile, None)
    }

    /// `None` when the running kernel does not offer a usable Landlock mask.
    pub fn with_memory_limit(
        workspace: PathBuf,
        cpu_seconds: u64,
        fsize_bytes: u64,
        nofile: u64,
        memory_bytes: Option<u64>,
    ) -> Option<Self> {
        let handled = landlock::available_mask()?;
        // The workspace must exist: the child's pre-exec `open(O_PATH)` on it
        // is what anchors the write rule. Created here (daemon side) so a
        // missing directory fails construction loudly, not mid-spawn.
        std::fs::create_dir_all(&workspace).ok()?;
        let mut writable = vec![workspace.clone()];
        writable.push(PathBuf::from("/tmp"));
        let writable = writable
            .into_iter()
            .filter_map(|p| {
                use std::os::unix::ffi::OsStrExt;
                std::ffi::CString::new(p.as_os_str().as_bytes()).ok()
            })
            .collect();
        Some(Self {
            _workspace: workspace,
            cpu_seconds,
            fsize_bytes,
            nofile,
            memory_bytes,
            handled,
            writable,
        })
    }
}

#[cfg(target_os = "linux")]
impl Sandbox for LinuxLandlock {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn capability(&self) -> &'static str {
        "kernel LSM: read-everywhere, write workspace+/tmp only; rlimits \
         (CPU, file size, fd count, optional address space); no network isolation"
    }

    fn wrap(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<(OsString, Vec<OsString>), SandboxError> {
        Ok((program.to_os_string(), args.to_vec()))
    }

    fn prepare_child(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        use std::os::unix::process::CommandExt;
        let cpu = self.cpu_seconds;
        let fsize = self.fsize_bytes;
        let nofile = self.nofile;
        let memory = self.memory_bytes;
        let handled = self.handled;
        let writable = self.writable.clone();
        // SAFETY: pre_exec runs in the child after fork, before exec. The
        // closure applies rlimits (async-signal-safe syscalls) and then the
        // Landlock ruleset (open/add_rule/restrict_self syscalls; captured
        // CStrings mean no allocation in the child). Any failure aborts the
        // exec and fails the spawn — the boundary never degrades silently.
        unsafe {
            cmd.pre_exec(move || {
                setrlimit_safe(libc::RLIMIT_CPU, cpu, cpu)?;
                setrlimit_safe(libc::RLIMIT_FSIZE, fsize, fsize)?;
                setrlimit_safe(libc::RLIMIT_NOFILE, nofile, nofile)?;
                if let Some(bytes) = memory {
                    setrlimit_safe(libc::RLIMIT_AS, bytes, bytes)?;
                }
                landlock::apply_policy(handled, &writable)
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Detection and construction
// ---------------------------------------------------------------------------

// Only the unix backends probe PATH for an external binary; on Windows the
// only tier is the no-op fallback, so this has no callers there.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// The strongest backend for this platform, if one is available.
fn strongest_backend(workspace: &Path, limits: &RunLimits) -> Option<Arc<dyn Sandbox>> {
    #[cfg(target_os = "linux")]
    {
        binary_on_path("bwrap")
            .then(|| {
                Arc::new(
                    LinuxBwrap::new(
                        workspace.to_path_buf(),
                        limits.timeout.as_secs().max(1) + 30, // headroom for slow shutdown
                        limits.output_cap.saturating_mul(4).max(1024 * 1024) as u64,
                    )
                    .with_memory_limit(limits.memory_limit_bytes),
                ) as Arc<dyn Sandbox>
            })
            // No bwrap: the Landlock LSM (kernel ≥ 5.13) gives real write
            // containment with zero install requirements. Mask probed once.
            .or_else(|| {
                LinuxLandlock::with_memory_limit(
                    workspace.to_path_buf(),
                    limits.timeout.as_secs().max(1) + 30,
                    limits.output_cap.saturating_mul(4).max(1024 * 1024) as u64,
                    512,
                    limits.memory_limit_bytes,
                )
                .map(|s| Arc::new(s) as Arc<dyn Sandbox>)
            })
    }
    #[cfg(target_os = "macos")]
    {
        // The seatbelt profile derives from the workspace only; rlimits are
        // not applied by this backend (limits is unused here).
        let _ = limits;
        binary_on_path("sandbox-exec").then(|| {
            Arc::new(MacSeatbelt {
                workspace: workspace.to_path_buf(),
            }) as Arc<dyn Sandbox>
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, limits);
        None
    }
}

/// The weakest backend for the platform (best-effort degradation). Carries
/// the same limits as the stronger tiers — the resource boundary must not
/// depend on which backend is installed.
fn weakest_backend(limits: &RunLimits) -> Option<Arc<dyn Sandbox>> {
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(LinuxRlimits {
            memory_bytes: limits.memory_limit_bytes,
            ..LinuxRlimits::default()
        }) as Arc<dyn Sandbox>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = limits;
        None
    }
}

fn unavailable_message() -> String {
    #[cfg(target_os = "linux")]
    {
        "bubblewrap (`bwrap`) or the kernel Landlock LSM (Linux ≥ 5.13) is required for \
         `sandbox: required` on Linux; neither is available on this system"
            .to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "`sandbox-exec` is required for `sandbox: required` on macOS but was not found on PATH \
         (it is deprecated by Apple)"
            .to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "no sandbox backend is available on this platform in v0.1 (documented limitation); \
         `sandbox: required` fails closed"
            .to_string()
    }
}

/// Construct the sandbox for an agent, fail-closed per SPEC §12.
///
/// - `required`: the strongest backend or `SandboxError::Unavailable` —
///   the agent does not start.
/// - `best-effort`: strongest, else the rlimit fallback (Linux), else a
///   logged no-op.
/// - `off`: explicit no-isolation operator choice.
///
/// The strongest sandbox backend available on this platform, as a stable
/// label for `GET /health` and `kern doctor` (honest per SPEC §12 — a label
/// says what the runtime WOULD use, never that a boundary is stronger than
/// enforced).
pub fn backend_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        if binary_on_path("bwrap") {
            "bubblewrap"
        } else if landlock::available_mask().is_some() {
            "landlock (read-everywhere, write workspace+/tmp)"
        } else {
            "rlimits-only (best-effort)"
        }
    }
    #[cfg(target_os = "macos")]
    {
        if binary_on_path("sandbox-exec") {
            "seatbelt"
        } else {
            "none"
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "none"
    }
}

pub fn construct(
    mode: SandboxMode,
    workspace: &Path,
    limits: &RunLimits,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    match mode {
        SandboxMode::Off => Ok(Arc::new(NoSandbox)),
        SandboxMode::Required => strongest_backend(workspace, limits)
            .ok_or_else(|| SandboxError::Unavailable(unavailable_message())),
        SandboxMode::BestEffort => {
            if let Some(backend) =
                strongest_backend(workspace, limits).or_else(|| weakest_backend(limits))
            {
                Ok(backend)
            } else {
                tracing::warn!(
                    "no sandbox backend available on this platform; best-effort shell runs \
                     WITHOUT OS-level isolation (in-tool containment only)"
                );
                Ok(Arc::new(NoSandbox))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sandboxed command runner (the seam into the shell tool)
// ---------------------------------------------------------------------------

/// A `CommandRunner` that applies the agent's sandbox to every `sh -c`
/// invocation before spawning.
pub struct SandboxedRunner {
    sandbox: Arc<dyn Sandbox>,
    limits: RunLimits,
}

impl SandboxedRunner {
    pub fn new(sandbox: Arc<dyn Sandbox>, limits: RunLimits) -> Self {
        Self { sandbox, limits }
    }

    pub fn sandbox_name(&self) -> &'static str {
        self.sandbox.name()
    }
}

#[async_trait]
impl CommandRunner for SandboxedRunner {
    async fn run_command(&self, req: CommandRequest) -> Result<CommandOutput, ToolError> {
        let (program, args) = shell_spec(&req.command);
        let (program, args) = self
            .sandbox
            .wrap(&program, &args)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", self.sandbox.name())))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(cwd) = req.cwd {
            cmd.current_dir(cwd);
        }
        // Credential boundary: tool
        // subprocesses NEVER inherit the daemon's environment wholesale —
        // only a non-secret allowlist passes through. Backend-independent by
        // design: the boundary must not depend on which OS mechanism is
        // installed (bwrap's --clearenv already scrubs, landlock/rlimits/
        // none do not).
        scrub_tool_env(&mut cmd);
        self.sandbox
            .prepare_child(cmd.as_std_mut())
            .map_err(|e| ToolError::Failed(format!("{}: {e}", self.sandbox.name())))?;
        kern_tool::process::run_captured(cmd, &self.limits).await
    }
}

/// Non-secret variables allowed into tool subprocess environments. Provider
/// keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) and `KERN_TOKEN` are
/// deliberately absent — an agent must never read the daemon's credentials.
const TOOL_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "TERM",
    "TZ",
    "USER",
    "LOGNAME",
    "SHELL",
    "EDITOR",
    "PAGER",
];

/// Replace the child's environment with the allowlist, copying values from
/// the daemon's environment. `env_clear` drops every secret; nothing is
/// inherited by default.
fn scrub_tool_env(cmd: &mut tokio::process::Command) {
    cmd.env_clear();
    for key in TOOL_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shell-tool tests are Linux-only (the backend tiers they exercise
    // are Linux), so the tool imports and the context helper are too.
    #[cfg(target_os = "linux")]
    use kern_tool::builtins::shell::ShellTool;
    #[cfg(target_os = "linux")]
    use kern_tool::Tool;

    #[cfg(target_os = "linux")]
    fn ctx<'a>() -> kern_tool::ToolContext<'a> {
        kern_tool::ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    /// GitHub-hosted runners deny child-side `setrlimit`/Landlock syscalls
    /// (their sandbox returns EPERM at spawn), so kernel-enforced behavior
    /// cannot be exercised there. Probe the host once per test: when sandbox
    /// spawns are refused, the real-process tests skip with a message — on
    /// any host that permits them (local dev, self-hosted runners) the full
    /// contract is asserted. The runtime itself still fails closed on such
    /// hosts (a sandbox that cannot be applied must not degrade silently);
    /// only the tests are environment-aware.
    #[cfg(unix)]
    async fn host_denies_sandbox_spawn() -> bool {
        let dir = tempfile::tempdir().unwrap();
        let Ok(sandbox) = construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default())
        else {
            return true; // no backend at all: real-process tests cannot run
        };
        let runner = SandboxedRunner::new(sandbox, RunLimits::default());
        matches!(
            runner
                .run_command(CommandRequest {
                    command: "true".to_string(),
                    cwd: None,
                })
                .await,
            Err(e) if e.to_string().contains("Operation not permitted")
        )
    }

    /// Skip guard for the real-process sandbox tests (see
    /// [`host_denies_sandbox_spawn`]): `true` when the host refuses sandbox
    /// spawns and the kernel-enforced assertion cannot be exercised.
    #[cfg(unix)]
    async fn sandbox_spawn_denied() -> bool {
        if host_denies_sandbox_spawn().await {
            eprintln!(
                "host denies sandbox spawns (EPERM — e.g. GitHub-hosted runners); \
                 skipping kernel-enforced assertion"
            );
            true
        } else {
            false
        }
    }

    #[test]
    fn off_mode_is_no_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = construct(SandboxMode::Off, dir.path(), &RunLimits::default()).unwrap();
        assert_eq!(sandbox.name(), "none");
        let (program, args) = sandbox.wrap(OsStr::new("sh"), &[]).unwrap();
        assert_eq!(program, OsStr::new("sh"));
        assert!(args.is_empty());
    }

    #[test]
    fn required_mode_fails_closed_without_backend() {
        let dir = tempfile::tempdir().unwrap();
        let result = construct(SandboxMode::Required, dir.path(), &RunLimits::default());
        match result {
            Ok(sandbox) => {
                assert!(["bwrap", "seatbelt", "landlock"].contains(&sandbox.name()));
            }
            Err(err) => assert_eq!(err.code(), "SANDBOX_UNAVAILABLE"),
        }
    }

    #[test]
    fn best_effort_degrades_on_linux() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox =
            construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default()).unwrap();
        #[cfg(target_os = "linux")]
        {
            assert!(["bwrap", "landlock", "rlimits"].contains(&sandbox.name()));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(["seatbelt", "none"].contains(&sandbox.name()));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn memory_cap_applies_as_rlimit_as() {
        // Resource governance: a configured tool memory cap must be
        // enforced by the backend's child rlimits (`ulimit -v` finite), on
        // whatever Linux tier best-effort selects.
        if sandbox_spawn_denied().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let limits = RunLimits {
            memory_limit_bytes: Some(16 * 1024 * 1024),
            ..RunLimits::default()
        };
        let sandbox = construct(SandboxMode::BestEffort, dir.path(), &limits)
            .expect("best-effort always constructs on linux");
        let runner = SandboxedRunner::new(sandbox, limits);

        let out = runner
            .run_command(CommandRequest {
                command: "ulimit -v".to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        let value: u64 = out
            .stdout
            .trim()
            .parse()
            .expect("ulimit -v prints a number");
        assert_eq!(
            value,
            16 * 1024,
            "RLIMIT_AS must be the configured cap (KiB)"
        );
    }

    #[test]
    fn bwrap_wrap_carries_the_memory_rlimit() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = LinuxBwrap::new(dir.path().to_path_buf(), 60, 1024)
            .with_memory_limit(Some(8 * 1024 * 1024));
        let (program, args) = sandbox.wrap(OsStr::new("sh"), &[]).unwrap();
        assert_eq!(program, OsStr::new("bwrap"));
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = args
            .windows(2)
            .position(|w| w[0] == "--rlimit" && w[1] == "AS");
        assert!(pos.is_some(), "bwrap args must carry --rlimit AS: {args:?}");
        assert_eq!(args[pos.unwrap() + 2], "8388608");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rlimit_fallback_applies_limits_to_real_processes() {
        // Probe the actual child limits through the rlimit backend: `ulimit`
        // must report finite values, not "unlimited".
        if sandbox_spawn_denied().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sandbox = construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default())
            .expect("best-effort always constructs on linux");
        let runner = SandboxedRunner::new(sandbox, RunLimits::default());

        let out = runner
            .run_command(CommandRequest {
                command: "ulimit -f; ulimit -t; ulimit -n".to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines.len(), 3, "stdout: {}", out.stdout);
        for (i, label) in [(0, "file size"), (1, "cpu"), (2, "open files")] {
            assert_ne!(
                lines[i].trim(),
                "unlimited",
                "{label} limit must be finite: {}",
                out.stdout
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_shell_tool_runs_commands() {
        // End-to-end: the sandboxed runner behind the shell tool. Uses
        // best-effort (rlimits on this box), so this runs everywhere.
        if sandbox_spawn_denied().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sandbox = construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default())
            .expect("best-effort always constructs on linux");
        let runner: Arc<dyn CommandRunner> =
            Arc::new(SandboxedRunner::new(sandbox, RunLimits::default()));
        let tool = ShellTool::new(runner);

        let out = tool
            .run(
                &serde_json::json!({ "command": "printf 'sandboxed-ok'" }),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"], "sandboxed-ok");
        assert_eq!(out["code"], 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_runner_enforces_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default())
            .expect("best-effort always constructs on linux");
        let runner = SandboxedRunner::new(
            sandbox,
            RunLimits {
                timeout: std::time::Duration::from_millis(150),
                ..RunLimits::default()
            },
        );
        if sandbox_spawn_denied().await {
            return;
        }
        let err = runner
            .run_command(CommandRequest {
                command: "sleep 30".to_string(),
                cwd: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
    }

    #[test]
    fn sandbox_error_maps_to_kern_error() {
        let err: KernError = SandboxError::Unavailable("no backend".into()).into();
        assert_eq!(err.code(), ErrorCode::SandboxUnavailable);
        let err: KernError = SandboxError::Failed("boom".into()).into();
        assert_eq!(err.code(), ErrorCode::SandboxFailure);
    }

    /// Credential boundary: a
    /// secret in the daemon's environment must never reach a tool subprocess,
    /// while allowlisted vars (PATH) still pass through. Runs under whatever
    /// backend best-effort selects — the boundary is backend-independent.
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_env_is_scrubbed_of_secrets() {
        // Unique name: never collides with a real variable, and parallel
        // tests cannot interfere (each uses its own key).
        if sandbox_spawn_denied().await {
            return;
        }
        const SECRET: &str = "KERN_TEST_TOOL_SECRET_XYZ";
        std::env::set_var(SECRET, "super-secret-value");
        let dir = tempfile::tempdir().unwrap();
        let sandbox = construct(SandboxMode::BestEffort, dir.path(), &RunLimits::default())
            .expect("best-effort always constructs");
        let runner = SandboxedRunner::new(sandbox, RunLimits::default());

        let out = runner
            .run_command(CommandRequest {
                command: format!("printf '%s' \"${{{SECRET}:-unset}}|${{PATH:+path-kept}}\""),
                cwd: None,
            })
            .await
            .unwrap();
        assert!(
            out.stdout.contains("unset"),
            "secret must not reach the tool env (got: {})",
            out.stdout
        );
        assert!(
            out.stdout.contains("path-kept"),
            "allowlisted PATH must pass through (got: {})",
            out.stdout
        );
    }

    /// Landlock: the kernel-enforced write boundary. Writes inside the
    /// workspace and /tmp succeed; a write anywhere else fails closed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_blocks_writes_outside_workspace() {
        if landlock::available_mask().is_none() {
            eprintln!("landlock unavailable on this kernel; skipping");
            return;
        }
        if sandbox_spawn_denied().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let sandbox = Arc::new(
            LinuxLandlock::new(ws.clone(), 60, 16 * 1024 * 1024, 512).expect("mask probe ok"),
        );
        let runner = SandboxedRunner::new(sandbox, RunLimits::default());

        // Inside the workspace: allowed.
        let out = runner
            .run_command(CommandRequest {
                command: format!(
                    "echo ok > '{}'/in-ws.txt; cat '{}'/in-ws.txt; echo t > /dev/null",
                    ws.display(),
                    ws.display()
                ),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "ok", "stderr: {}", out.stderr);

        // /tmp is also writable (bwrap parity).
        let out = runner
            .run_command(CommandRequest {
                command: "echo t > /tmp/kern-landlock-probe; cat /tmp/kern-landlock-probe; rm /tmp/kern-landlock-probe"
                    .to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "t");

        // Outside every writable root: denied by the kernel — the shell's
        // open fails and the file must not exist. (Note: the tempdir above
        // lives under /tmp, which IS writable by design, so the probe target
        // is /etc — never writable under this policy, and Landlock restricts
        // root too.)
        let outside = format!("/etc/kern-landlock-probe-{}", std::process::id());
        let out = runner
            .run_command(CommandRequest {
                command: format!("echo nope > '{outside}' && cat '{outside}'"),
                cwd: None,
            })
            .await
            .unwrap();
        assert_ne!(
            out.code,
            Some(0),
            "write outside the workspace must fail: {out:?}"
        );
        assert!(
            !Path::new(&outside).exists(),
            "file outside the workspace must not exist"
        );

        // Read-everywhere still works (e.g. system files).
        let out = runner
            .run_command(CommandRequest {
                command: "cat /etc/hostname > /dev/null && echo read-ok".to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "read-ok");
    }

    /// Landlock: a read-only probe of system files succeeds (read
    /// everywhere) — proving the ruleset anchors on "/" rather than the
    /// workspace only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_allows_system_reads() {
        if landlock::available_mask().is_none() {
            eprintln!("landlock unavailable on this kernel; skipping");
            return;
        }
        if sandbox_spawn_denied().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let sandbox = Arc::new(
            LinuxLandlock::new(ws.clone(), 60, 16 * 1024 * 1024, 512).expect("mask probe ok"),
        );
        let runner = SandboxedRunner::new(sandbox, RunLimits::default());
        let out = runner
            .run_command(CommandRequest {
                command: "head -c 16 /proc/self/status | wc -c".to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "16");
    }
}
