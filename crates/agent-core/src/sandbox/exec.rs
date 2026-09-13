//! Phase B of the Python tool (M5, ADR-012/013): one script run inside a
//! bubblewrap user namespace (network off by default, only the workspace
//! writable, env read-only, supervised limits). The script is fed to
//! `python -` on stdin, so artifacts are exactly the files the script itself
//! produced.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::error::{ApiError, ApiErrorKind, Result};

use super::limits::Limits;

/// Per-stream capture cap; the streams keep draining after it so the child
/// can never block on a full pipe.
pub const MAX_STREAM_BYTES: usize = 64 * 1024;

/// Read-only system trees every sandbox exposes (missing ones are skipped by
/// `--ro-bind-try`).
const SYSTEM_BINDS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/sbin", "/etc"];

/// Where a prepared environment is mounted inside the sandbox.
const ENV_MOUNT: &str = "/opt/env";
/// Where the workspace is mounted inside the sandbox.
pub const WORKSPACE_MOUNT: &str = "/workspace";

/// The result of one sandboxed script run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Signal that killed the process, when it died from one (e.g. SIGKILL
    /// from the OOM killer or SIGXCPU).
    pub signal: Option<i32>,
    /// Output exceeded [`MAX_STREAM_BYTES`] on some stream.
    pub truncated: bool,
    /// The supervisor's wall-clock timeout fired and the process was killed.
    pub timed_out: bool,
    pub duration_ms: u64,
}

impl ScriptOutcome {
    /// True when the script ran to completion with exit code 0.
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Everything one sandboxed run needs; assembled by the owning `Sandbox`.
pub struct RunSpec<'a> {
    pub workspace: &'a Path,
    /// Prepared environment mounted read-only at `/opt/env` (absent for a
    /// dependency-free run, which uses the system interpreter).
    pub env: Option<&'a Path>,
    pub read_paths: &'a [PathBuf],
    /// Absolute path of the system Python (visible via the `/usr` bind).
    pub python: &'a Path,
    /// Share the host network namespace (only after `NetworkAccess` consent).
    pub network: bool,
    pub limits: Limits,
}

/// Run `script` inside bubblewrap and capture bounded output.
pub async fn run(spec: &RunSpec<'_>, script: &str) -> Result<ScriptOutcome> {
    let args = bwrap_args(spec);
    let mut command = Command::new("bwrap");
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A dropped future (aborted turn) must not leave the script running.
        .kill_on_drop(true);
    super::limits::apply(command.as_std_mut(), spec.limits);

    let mut child = command.spawn().map_err(|e| {
        ApiError::new(
            ApiErrorKind::Config,
            format!(
                "cannot start bubblewrap: {e}. Install bubblewrap and enable \
                 unprivileged user namespaces (see docs/arc42-architecture.md, M5)."
            ),
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|e| ApiError::internal(format!("cannot deliver the script to python: {e}")))?;
        drop(stdin); // EOF: python starts executing
    }

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let out_task = tokio::spawn(read_capped(stdout, MAX_STREAM_BYTES));
    let err_task = tokio::spawn(read_capped(stderr, MAX_STREAM_BYTES));

    let started = Instant::now();
    let wall = Duration::from_secs(spec.limits.wall_secs.max(1));
    let (status, timed_out) = match tokio::time::timeout(wall, child.wait()).await {
        Ok(Ok(status)) => (Some(status), false),
        Ok(Err(e)) => {
            return Err(ApiError::internal(format!(
                "cannot wait for the sandboxed process: {e}"
            )));
        }
        Err(_) => {
            let _ = child.start_kill();
            (child.wait().await.ok(), true)
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    // The child is gone (or killed), so the pipe readers finish promptly.
    let (stdout, stdout_truncated) = out_task.await.unwrap_or_default();
    let (stderr, stderr_truncated) = err_task.await.unwrap_or_default();

    let (exit_code, signal) = match &status {
        Some(status) => (status.code(), signal_of(status)),
        None => (None, None),
    };
    Ok(ScriptOutcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        signal,
        truncated: stdout_truncated || stderr_truncated,
        timed_out,
        duration_ms,
    })
}

/// The full bubblewrap command line (public for tests that pin the recipe).
pub fn bwrap_args(spec: &RunSpec<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let flag = |args: &mut Vec<OsString>, value: &str| args.push(OsString::from(value));

    for value in [
        "--unshare-all",
        "--new-session",
        "--die-with-parent",
        "--clearenv",
    ] {
        flag(&mut args, value);
    }
    if spec.network {
        // Only ever reached with an explicit NetworkAccess consent.
        flag(&mut args, "--share-net");
    }
    for path in SYSTEM_BINDS {
        flag(&mut args, "--ro-bind-try");
        flag(&mut args, path);
        flag(&mut args, path);
    }
    // Mount /tmp before binding any read path under it: bwrap creates missing
    // destination parents, and a later `--tmpfs /tmp` would otherwise hide a
    // bind placed in the namespace root.
    for value in ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"] {
        flag(&mut args, value);
    }
    for path in spec.read_paths {
        flag(&mut args, "--ro-bind-try");
        args.push(path.as_os_str().to_owned());
        args.push(path.as_os_str().to_owned());
    }
    if let Some(env) = spec.env {
        flag(&mut args, "--ro-bind");
        args.push(env.as_os_str().to_owned());
        flag(&mut args, ENV_MOUNT);
    }
    flag(&mut args, "--bind");
    args.push(spec.workspace.as_os_str().to_owned());
    flag(&mut args, WORKSPACE_MOUNT);
    // The namespace root is a writable tmpfs by default; make it read-only so
    // the workspace stays the only writable path (the child mounts above are
    // unaffected). Must come after the mounts that create their mount points
    // in the root.
    for value in ["--remount-ro", "/"] {
        flag(&mut args, value);
    }
    flag(&mut args, "--chdir");
    flag(&mut args, WORKSPACE_MOUNT);

    let path = if spec.env.is_some() {
        format!("{ENV_MOUNT}/bin")
    } else {
        spec.python
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_else(|| "/usr/bin".to_owned())
    };
    for (key, value) in [
        ("PATH", format!("{path}:/usr/bin:/bin")),
        ("HOME", "/tmp".to_owned()),
        ("LANG", "C.UTF-8".to_owned()),
        ("LC_ALL", "C.UTF-8".to_owned()),
        ("PYTHONDONTWRITEBYTECODE", "1".to_owned()),
        ("PYTHONUNBUFFERED", "1".to_owned()),
        // A display-less default so plotting libraries write files instead of
        // trying to open a window.
        ("MPLBACKEND", "Agg".to_owned()),
        ("KAERU_WORKSPACE", WORKSPACE_MOUNT.to_owned()),
    ] {
        flag(&mut args, "--setenv");
        flag(&mut args, key);
        flag(&mut args, &value);
    }

    flag(&mut args, "--");
    if spec.env.is_some() {
        flag(&mut args, &format!("{ENV_MOUNT}/bin/python"));
    } else {
        args.push(spec.python.as_os_str().to_owned());
    }
    // `python -` reads the program from stdin.
    flag(&mut args, "-");
    args
}

/// Drain a child pipe, storing at most `cap` bytes and reporting truncation.
async fn read_capped(reader: impl AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    let mut reader = reader;
    let mut chunk = [0u8; 8192];
    let mut collected = Vec::new();
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(collected.len());
                if read <= room {
                    collected.extend_from_slice(&chunk[..read]);
                } else {
                    collected.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    // Keep draining so the child never blocks on a full pipe.
                }
            }
        }
    }
    (collected, truncated)
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(workspace: &'a Path, read_paths: &'a [PathBuf]) -> RunSpec<'a> {
        RunSpec {
            workspace,
            env: None,
            read_paths,
            python: Path::new("/usr/bin/python3"),
            network: false,
            limits: Limits {
                memory_bytes: 512 * 1024 * 1024,
                cpu_secs: 15,
                fsize_bytes: 64 * 1024 * 1024,
                wall_secs: 10,
            },
        }
    }

    #[test]
    fn recipe_pins_the_sandbox_flags_in_order() {
        let workspace = PathBuf::from("/tmp/ws");
        let args = bwrap_args(&spec(&workspace, &[]));
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--unshare-all");
        assert!(args.contains(&"--new-session".to_owned()));
        assert!(args.contains(&"--die-with-parent".to_owned()));
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(&args[bind..bind + 3], ["--bind", "/tmp/ws", "/workspace"]);
        assert!(args.windows(2).any(|w| w == ["--chdir", "/workspace"]));
        assert!(args.windows(2).any(|w| w == ["--remount-ro", "/"]));
        assert_eq!(args.last().unwrap(), "-");
        assert!(args.iter().any(|a| a == "/usr/bin/python3"));
        assert!(!args.iter().any(|a| a == "--share-net"));
        assert!(!args.iter().any(|a| a == "/opt/env"));
    }

    #[test]
    fn network_is_opt_in_and_prepared_envs_mount_read_only() {
        let workspace = PathBuf::from("/tmp/ws");
        let env = PathBuf::from("/tmp/envs/abc");
        let reads = vec![PathBuf::from("/srv/notes")];
        let mut spec = spec(&workspace, &reads);
        spec.env = Some(&env);
        spec.network = true;
        let args = bwrap_args(&spec);
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        // --share-net must follow --unshare-all to undo just the net namespace.
        let unshare = args.iter().position(|a| a == "--unshare-all").unwrap();
        let share = args.iter().position(|a| a == "--share-net").unwrap();
        assert!(share > unshare);
        assert!(args.windows(2).any(|w| w == ["--ro-bind", "/tmp/envs/abc"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--ro-bind-try", "/srv/notes"])
        );
        assert_eq!(args.last().unwrap(), "-");
        assert!(args.iter().any(|a| a == "/opt/env/bin/python"));
    }

    /// RLIMIT_FSIZE is enforced: writing past the cap kills or fails the
    /// script. Needs bubblewrap; skipped where the host cannot sandbox.
    #[tokio::test]
    async fn the_file_size_limit_is_enforced() {
        let root = std::env::temp_dir().join(format!("kaeru-fsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config = crate::config::SandboxConfig {
            workspace: root.join("ws"),
            read_paths: Vec::new(),
            timeout_secs: 20,
            memory_mb: 256,
        };
        let sandbox = crate::sandbox::Sandbox::new(&config, root.join("envs")).unwrap();
        if sandbox.check_host().is_err() {
            eprintln!("skipping: bubblewrap is not usable on this host");
            return;
        }
        let limits = Limits {
            memory_bytes: 256 * 1024 * 1024,
            cpu_secs: 25,
            fsize_bytes: 1024 * 1024,
            wall_secs: 20,
        };
        let spec = RunSpec {
            workspace: sandbox.workspace(),
            env: None,
            read_paths: &[],
            python: sandbox.python(),
            network: false,
            limits,
        };
        let script = r#"
try:
    with open('big.bin', 'wb') as f:
        f.write(b'x' * (4 * 1024 * 1024))
    print('WROTE-ALL')
except Exception as err:
    print('blocked-fsize:', type(err).__name__)
"#;
        let outcome = run(&spec, script).await.unwrap();
        assert!(!outcome.stdout.contains("WROTE-ALL"), "{outcome:?}");
        // Either the write failed with EFBIG or SIGXFSZ killed the process.
        assert!(
            outcome.stdout.contains("blocked-fsize") || outcome.signal == Some(libc::SIGXFSZ),
            "{outcome:?}"
        );
    }
}
