//! Supervisor-imposed resource limits (M5, ADR-012): the rlimits bubblewrap
//! itself does not provide. Applied with `pre_exec` to the sandbox child
//! before it execs; the wall-clock timeout lives in `exec.rs`.
//!
//! RLIMIT_NPROC is counted **per real user** on Linux (threads included), so
//! a fixed cap would break as soon as the owner's desktop has more threads
//! than the cap — and unprivileged user-namespace creation itself fails when
//! the limit is already exceeded. The cap is therefore computed per run as
//! *current thread count + a bounded margin*: plenty of room for the sandbox,
//! while a fork bomb stops after `SANDBOX_NPROC_MARGIN` extra processes.

use std::process::Command as StdCommand;

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use crate::config::SandboxConfig;

/// Extra processes a sandboxed run may create beyond the user's current
/// thread count (see the module note).
pub const SANDBOX_NPROC_MARGIN: u64 = 64;
/// Fallback cap when `/proc` cannot be inspected.
pub const SANDBOX_NPROC_FALLBACK: u64 = 4096;
/// Largest file a sandboxed script may write (per file, RLIMIT_FSIZE).
pub const SANDBOX_FSIZE_BYTES: u64 = 64 * 1024 * 1024;

/// The supervisor's resource policy for one sandboxed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Address-space cap (RLIMIT_AS).
    pub memory_bytes: u64,
    /// CPU-seconds cap (RLIMIT_CPU); a margin over the wall clock so the
    /// timeout normally fires first with a clearer error.
    pub cpu_secs: u64,
    /// Per-file size cap (RLIMIT_FSIZE).
    pub fsize_bytes: u64,
    /// Wall-clock cap enforced by the supervisor.
    pub wall_secs: u64,
}

impl Limits {
    pub fn from_config(config: &SandboxConfig) -> Self {
        Self {
            memory_bytes: config.memory_mb.saturating_mul(1024 * 1024),
            cpu_secs: config.timeout_secs.saturating_add(5),
            fsize_bytes: SANDBOX_FSIZE_BYTES,
            wall_secs: config.timeout_secs,
        }
    }
}

/// Apply the rlimits to a child command via `pre_exec` (unix only; the M5
/// host requirement is Linux).
#[cfg(unix)]
pub fn apply(cmd: &mut StdCommand, limits: Limits) {
    let nproc = nproc_limit();
    // SAFETY: `pre_exec` runs between fork and exec. The closure only calls
    // async-signal-safe `setrlimit` and creates no formatted strings.
    unsafe {
        cmd.pre_exec(move || {
            set_rlimit(libc::RLIMIT_AS, limits.memory_bytes)?;
            set_rlimit(libc::RLIMIT_CPU, limits.cpu_secs)?;
            set_rlimit(libc::RLIMIT_NPROC, nproc)?;
            set_rlimit(libc::RLIMIT_FSIZE, limits.fsize_bytes)?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub fn apply(_cmd: &mut StdCommand, _limits: Limits) {
    // Linux is the documented host requirement (C11); this stub keeps the
    // crate compiling elsewhere while the startup check fails closed.
}

/// Current thread count of the real user plus the sandbox margin.
#[cfg(unix)]
fn nproc_limit() -> u64 {
    let uid = unsafe { libc::getuid() };
    nproc_limit_from(count_user_threads(uid))
}

/// The cap given a (possibly unavailable) current thread count.
fn nproc_limit_from(current: Option<u64>) -> u64 {
    match current {
        Some(count) => count.saturating_add(SANDBOX_NPROC_MARGIN),
        None => SANDBOX_NPROC_FALLBACK,
    }
}

/// Count processes/threads owned by `uid` via `/proc`; `None` when `/proc`
/// is unavailable.
#[cfg(unix)]
fn count_user_threads(uid: libc::uid_t) -> Option<u64> {
    let mut threads: u64 = 0;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(format!("/proc/{pid}")) else {
            continue;
        };
        if std::os::unix::fs::MetadataExt::uid(&metadata) != uid {
            continue;
        }
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task"))
            .map(|tasks| tasks.flatten().count() as u64)
            .unwrap_or(1);
        threads = threads.saturating_add(tasks);
    }
    Some(threads)
}

#[cfg(unix)]
fn set_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    let rc = unsafe { libc::setrlimit(resource, &limit) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_derive_from_config_with_a_cpu_margin() {
        let config = SandboxConfig {
            timeout_secs: 10,
            memory_mb: 128,
            ..SandboxConfig::default()
        };
        let limits = Limits::from_config(&config);
        assert_eq!(limits.memory_bytes, 128 * 1024 * 1024);
        assert_eq!(limits.wall_secs, 10);
        assert_eq!(limits.cpu_secs, 15);
        assert_eq!(limits.fsize_bytes, SANDBOX_FSIZE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn the_process_cap_leaves_room_above_the_current_thread_count() {
        assert_eq!(nproc_limit_from(Some(100)), 100 + SANDBOX_NPROC_MARGIN);
        assert_eq!(nproc_limit_from(None), SANDBOX_NPROC_FALLBACK);
        // The live count is always at least the margin (the current process
        // itself exists), so the computed cap must exceed it.
        assert!(nproc_limit() > SANDBOX_NPROC_MARGIN);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_limit_reaches_the_child() {
        // `ulimit -v` reports the inherited RLIMIT_AS in KiB; this proves the
        // pre_exec hook applies before exec (no bubblewrap needed).
        let mut command = StdCommand::new("/bin/sh");
        command.args(["-c", "ulimit -v"]);
        apply(
            &mut command,
            Limits {
                memory_bytes: 64 * 1024 * 1024,
                cpu_secs: 5,
                fsize_bytes: 1024 * 1024,
                wall_secs: 5,
            },
        );
        let output = tokio::process::Command::from(command)
            .output()
            .await
            .expect("spawn sh");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "65536", "RLIMIT_AS must be inherited");
    }
}
