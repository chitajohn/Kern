//! Captured subprocess execution (the `shell` builtin's transport layer).
//!
//! `run_captured` owns the process-lifecycle contract (SPEC.md §11.1/§11.3):
//!
//! - **Kill-on-drop, tree-wide:** the child is spawned in its own process
//!   group on unix and into a job object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`)
//!   on Windows. A guard's `Drop` sends `SIGKILL` to the whole group /
//!   `TerminateJobObject` to the job, so a
//!   backgrounded grandchild (`sh -c 'sleep 1000 &'`) cannot outlive the
//!   tool call on either platform. The future may be cancelled by the executor's `tool_timeout`
//!   or daemon shutdown at any point; dropping the future MUST kill the
//!   child **and its descendants**, never leak a stray process.
//!   `tokio::process::Child` alone does NOT kill on drop, so the guard is
//!   load-bearing. A child that deliberately `setsid`s out of the group can
//!   still escape on platforms without a pid namespace; the bwrap backend
//!   closes even that (documented limitation, ARCHITECTURE.md §22).
//! - **Timeout:** `RunLimits::timeout` bounds the whole run → `TOOL_TIMEOUT`.
//! - **Output cap:** stdout and stderr are each capped at `output_cap`
//!   (default 1 MiB per SPEC). Exceeding the cap kills the child and fails
//!   with `TOOL_FAILED` — output is never silently truncated.
//! - Both streams are drained concurrently (a full pipe on either side would
//!   otherwise deadlock the child).
//!
//! This module is sandbox-agnostic: `SystemRunner` spawns directly, while
//! `kern-core`'s sandboxed runner applies the OS sandbox to the same
//! `tokio::process::Command` before calling `run_captured`.

use std::time::Duration;

use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::error::ToolError;

/// Default per-stream output cap (SPEC.md §11.1: "default 1 MiB").
pub const DEFAULT_OUTPUT_CAP: usize = 1024 * 1024;

/// Resource limits for a captured run.
#[derive(Debug, Clone)]
pub struct RunLimits {
    pub timeout: Duration,
    pub output_cap: usize,
    /// Address-space cap in bytes for the tool process: enforced as
    /// `RLIMIT_AS` by the sandbox backends that apply rlimits (bwrap,
    /// landlock, rlimits), and as the job-object `ProcessMemoryLimit` on
    /// Windows. `None` = no memory cap (the default — a memory limit is an
    /// explicit per-agent operator choice).
    pub memory_limit_bytes: Option<u64>,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            output_cap: DEFAULT_OUTPUT_CAP,
            memory_limit_bytes: None,
        }
    }
}

/// The normalized result of a completed run.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// Exit code; `None` when the process was killed by a signal.
    pub code: Option<i32>,
}

/// Send `SIGKILL` to every process in `pid`'s process group (the direct
/// child is the group leader, so `kill(-pid)` covers the whole tree,
/// including backgrounded grandchildren).
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: plain libc kill(2) with a valid (negative) pid. Errors are
    // ignored: the group may already be gone.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// The Windows process-tree primitive: an unnamed job object per captured
/// run. Closing the last handle terminates every process still inside it,
/// which is the job-object equivalent of the unix process-group `SIGKILL`.
#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// An owned job-object handle.
    ///
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means Drop is load-bearing: it
    /// terminates any surviving member (grandchildren included) when the
    /// handle closes. `ProcessMemoryLimit` adds the address-space cap that
    /// `RLIMIT_AS` provides on unix.
    pub struct JobObject(HANDLE);

    // SAFETY: a job-object handle is a kernel handle with no thread
    // affinity; every operation here is a thread-safe kernel call.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub fn create(memory_limit: Option<u64>) -> io::Result<Self> {
            // SAFETY: null security attributes and an unnamed job object.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            // SAFETY: the extended-limit struct is plain old data; zeroing is
            // its empty configuration.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(bytes) = memory_limit {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.ProcessMemoryLimit = bytes.min(usize::MAX as u64) as usize;
            }
            // SAFETY: `info` outlives the call and the declared size matches
            // the struct SetInformationJobObject reads.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                // `job` drops here and closes the handle.
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Move `pid` into the job. Called immediately after spawn; the narrow
        /// pre-assignment window is a documented limitation (ARCHITECTURE §22).
        pub fn assign(&self, pid: u32) -> io::Result<()> {
            // SAFETY: plain handle acquisition; the process handle is closed
            // exactly once below.
            let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let ok = unsafe { AssignProcessToJobObject(self.0, process) };
            let err = io::Error::last_os_error();
            // SAFETY: `process` is owned by this scope and not used again.
            unsafe { CloseHandle(process) };
            if ok == 0 {
                Err(err)
            } else {
                Ok(())
            }
        }

        /// Terminate every process in the job.
        pub fn terminate(&self) {
            // SAFETY: valid job handle; a failure only means the job is empty.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: the handle is owned here and closed exactly once.
            // KILL_ON_JOB_CLOSE terminates any surviving member at this point.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A child that is always killed (tree-wide) when dropped (see module docs).
///
/// Public because MCP servers are long-lived children with the same
/// containment contract: dropping the `Arc<Tool>` that owns the server must
/// take the whole process tree down (an `npx` launcher leaves a `node`
/// grandchild that a direct-child kill would orphan).
pub struct ContainedChild {
    child: Child,
    /// Windows: the job owning this run's process tree.
    #[cfg(windows)]
    job: Option<job::JobObject>,
}

impl ContainedChild {
    /// Spawn `cmd` into a fresh process group (unix) or job object (Windows),
    /// so `kill_tree` can reach every descendant.
    pub fn spawn(mut cmd: Command, memory_limit: Option<u64>) -> std::io::Result<Self> {
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn()?;
        #[cfg(unix)]
        let _ = memory_limit;
        #[cfg(windows)]
        let job = Self::attach_job(&child, memory_limit);
        Ok(Self {
            child,
            #[cfg(windows)]
            job,
        })
    }

    /// Best-effort job assignment: a failure is logged and the run proceeds
    /// with direct-child-only termination (never a failed tool call).
    #[cfg(windows)]
    fn attach_job(child: &Child, memory_limit: Option<u64>) -> Option<job::JobObject> {
        let job = match job::JobObject::create(memory_limit) {
            Ok(job) => job,
            Err(err) => {
                tracing::warn!(%err, "job object unavailable; a timed-out tool can only kill its direct child");
                return None;
            }
        };
        let Some(pid) = child.id() else {
            // Exited before assignment: nothing left to contain.
            return Some(job);
        };
        if let Err(err) = job.assign(pid) {
            tracing::warn!(
                %err,
                pid,
                "could not assign the tool process to its job object; grandchild kill is best-effort"
            );
            return None;
        }
        Some(job)
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Windows only: whether the run is inside a job object. `false` means
    /// assignment failed (logged) and termination degrades to the direct child.
    #[cfg(windows)]
    pub fn job_attached(&self) -> bool {
        self.job.is_some()
    }

    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// Kill the direct child and its entire tree. `start_kill` is sync (unlike
    /// `kill`), so this is safe in `Drop`.
    pub fn kill_tree(&mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.start_kill();
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            kill_process_group(pid);
        }
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        // Best-effort: the process may already be reaped.
        self.kill_tree();
    }
}

/// Spawn `cmd` (stdout/stderr are forced to piped), run it under `limits`,
/// and return the captured output. Errors map to `TOOL_FAILED`; a timeout
/// maps to `TOOL_TIMEOUT`.
pub async fn run_captured(
    mut cmd: Command,
    limits: &RunLimits,
) -> Result<CommandOutput, ToolError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Spawns into its own process group / job object so the whole tree
    // (including backgrounded grandchildren) can be killed atomically.
    let mut child = ContainedChild::spawn(cmd, limits.memory_limit_bytes)
        .map_err(|e| ToolError::Failed(format!("spawn failed: {e}")))?;
    let stdout = child
        .take_stdout()
        .expect("stdout must be piped before spawn");
    let stderr = child
        .take_stderr()
        .expect("stderr must be piped before spawn");

    let inner = async {
        // Drain both streams concurrently to avoid pipe deadlock.
        let (out_res, err_res) = tokio::join!(
            read_capped(stdout, limits.output_cap),
            read_capped(stderr, limits.output_cap),
        );
        let (stdout, stdout_truncated) =
            out_res.map_err(|e| ToolError::Failed(format!("read stdout: {e}")))?;
        let (stderr, stderr_truncated) =
            err_res.map_err(|e| ToolError::Failed(format!("read stderr: {e}")))?;

        if stdout_truncated || stderr_truncated {
            child.kill_tree();
            let _ = child.wait().await;
            return Err(ToolError::Failed(format!(
                "command output exceeds the {} byte cap",
                limits.output_cap
            )));
        }

        let status = child
            .wait()
            .await
            .map_err(|e| ToolError::Failed(format!("wait: {e}")))?;
        Ok(CommandOutput {
            stdout,
            stderr,
            code: status.code(),
        })
    };

    match tokio::time::timeout(limits.timeout, inner).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(err),
        Err(_elapsed) => {
            // The inner future was dropped (the guard's Drop killed the
            // tree); re-kill and reap for a clean exit before reporting.
            child.kill_tree();
            let _ = child.wait().await;
            Err(ToolError::Timeout(limits.timeout))
        }
    }
}

/// Read up to `cap` bytes; `(bytes, true)` when the stream exceeded the cap
/// (the remainder is discarded and the caller kills the child).
async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> std::io::Result<(String, bool)> {
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() + n > cap {
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The platform shell for a script: cmd.exe on Windows (where `;` is not
    /// a separator and `>&2`/`exit N` have no POSIX meaning), `sh -c` on
    /// unix. Scripts must be written in the platform's syntax.
    fn sh_cmd(script: &str) -> Command {
        #[cfg(windows)]
        {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", script]);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            cmd
        }
    }

    /// Pick the script for the current platform (POSIX sh vs cmd.exe). The
    /// unused-parameter prefix keeps clippy's `unused_variables` quiet for
    /// whichever branch is compiled out.
    fn script(_unix: &'static str, _windows: &'static str) -> &'static str {
        #[cfg(windows)]
        {
            _windows
        }
        #[cfg(not(windows))]
        {
            _unix
        }
    }

    fn piped(cmd: &mut Command) {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_code() {
        let mut cmd = sh_cmd(script(
            "echo out; echo err >&2; exit 3",
            "echo out & echo err 1>&2 & exit /b 3",
        ));
        piped(&mut cmd);
        let out = run_captured(cmd, &RunLimits::default()).await.unwrap();
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
        assert_eq!(out.code, Some(3));
    }

    #[tokio::test]
    async fn timeout_kills_and_reports_timeout() {
        // cmd.exe has no `sleep`; `ping -n 6 127.0.0.1` takes ~5s (the first
        // ping is immediate, then one per second).
        let mut cmd = sh_cmd(script("sleep 5", "ping -n 6 127.0.0.1 >NUL"));
        piped(&mut cmd);
        let limits = RunLimits {
            timeout: Duration::from_millis(150),
            ..RunLimits::default()
        };
        let err = run_captured(cmd, &limits).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
    }

    #[tokio::test]
    async fn output_cap_fails_not_truncates() {
        // cmd.exe has no `printf`; `echo` of 20 digits (+ CRLF) also exceeds
        // the 16-byte cap.
        let mut cmd = sh_cmd(script(
            "printf '0123456789%.0s' 1 2 3 4 5 6 7 8 9 10 11 12",
            "echo 01234567890123456789",
        ));
        piped(&mut cmd);
        let limits = RunLimits {
            output_cap: 16,
            timeout: Duration::from_secs(5),
            ..RunLimits::default()
        };
        let err = run_captured(cmd, &limits).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("output exceeds"), "{err}");
    }

    #[tokio::test]
    async fn missing_program_fails_cleanly() {
        let mut cmd = Command::new("kern-no-such-binary-xyz");
        cmd.arg("--nope");
        piped(&mut cmd);
        let err = run_captured(cmd, &RunLimits::default()).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("spawn"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_is_killed_when_future_dropped() {
        // External cancellation (the executor's tool_timeout dropping the
        // future) must kill the child rather than leak it. A SIGKILLed
        // process may linger as a zombie until tokio's orphan reaper runs,
        // so poll `kill -0` (which succeeds on zombies) until it fails.
        let mut cmd = sh_cmd("sleep 30");
        piped(&mut cmd);
        let child = ContainedChild::spawn(cmd, None).expect("spawn must succeed; sh is present");
        let pid = child.id().expect("spawned child has a pid");
        drop(child);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "child {pid} must be dead after drop");
    }

    /// A tool call that backgrounds a long-lived grandchild
    /// (`sh -c 'sleep 30 & ...'`) must not leak it when the call times out.
    /// Without process-group termination the grandchild survives past the
    /// tool boundary.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_grandchild_is_killed_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        // argv[0]="sh", argv[1]=pidfile.
        let script = "sleep 30 & echo $! > \"$1\"; wait";
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script, "sh", pidfile.to_str().unwrap()]);
        piped(&mut cmd);
        let limits = RunLimits {
            timeout: Duration::from_millis(200),
            ..RunLimits::default()
        };
        let err = run_captured(cmd, &limits).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");

        // Read the grandchild's pid and wait for it to die.
        let mut grandchild_pid = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(raw) = std::fs::read_to_string(&pidfile) {
                grandchild_pid = raw.trim().parse::<u32>().ok();
                if grandchild_pid.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = grandchild_pid.expect("grandchild must have written its pid");
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            gone,
            "backgrounded grandchild {pid} must be killed with the process group"
        );
    }

    /// On Windows the job object is the tree killer: cmd.exe starts
    /// PowerShell, which starts `ping` — a grandchild of our direct child — and
    /// publishes its pid. A tool timeout must take the whole job down;
    /// killing only cmd.exe would leave `ping` running.
    #[cfg(windows)]
    #[tokio::test]
    async fn background_grandchild_is_killed_on_timeout_windows() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        // `-NoNewWindow` keeps Start-Process on the CreateProcess path, so
        // `ping` is a real child of PowerShell. `-WindowStyle` would route
        // through ShellExecute, and the shell (outside our job) would create it.
        let script = format!(
            "$p = Start-Process -FilePath ping -ArgumentList '-n','120','127.0.0.1' \
             -NoNewWindow -PassThru; Set-Content -LiteralPath '{}' -Value $p.Id; \
             Wait-Process -Id $p.Id",
            pidfile.display()
        );
        let mut cmd = Command::new("cmd");
        cmd.arg("/C")
            .arg("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(&script);
        piped(&mut cmd);
        // Generous: PowerShell startup plus Start-Process must publish the pid
        // before the timeout fires, or the assertion below cannot run.
        let limits = RunLimits {
            timeout: Duration::from_secs(8),
            ..RunLimits::default()
        };
        let err = run_captured(cmd, &limits).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");

        let mut pid = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(raw) = std::fs::read_to_string(&pidfile) {
                pid = raw.trim().parse::<u32>().ok();
                if pid.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = pid.expect("grandchild must have published its pid before the timeout");

        // `TerminateJobObject` posts termination; the process is not
        // necessarily gone by the time the call returns (observed on
        // windows-latest CI). Poll with a bounded deadline, exactly like the
        // unix process-group tests.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("grandchild {pid} must be terminated with the job object");
    }

    /// Every captured run must own a job object, otherwise tree termination
    /// silently degrades to direct-child kill.
    #[cfg(windows)]
    #[tokio::test]
    async fn spawned_child_is_assigned_to_a_job() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "exit 0"]);
        piped(&mut cmd);
        let child = ContainedChild::spawn(cmd, None).expect("spawn must succeed");
        assert!(
            child.job_attached(),
            "job assignment failed on this host; the tool tree kill is degraded"
        );
    }

    /// A synchronize handle is signaled as soon as the process exits, so this
    /// has no zombie ambiguity (the Windows analogue of `kill -0`).
    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: plain handle query; the handle is closed exactly once.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let state = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            state != WAIT_OBJECT_0
        }
    }
}
