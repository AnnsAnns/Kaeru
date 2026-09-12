//! The Python sandbox (M5): a two-phase supervisor around `uv` and
//! `bubblewrap` (ADR-012/013).
//!
//! - **Phase A** ([`envprep`]) prepares an ephemeral uv environment for a
//!   requested dependency set. It is the only phase with network access and
//!   runs only after `PackageInstall` consent.
//! - **Phase B** ([`exec`]) runs one script inside a bubblewrap user
//!   namespace: no network by default, the workspace is the only writable
//!   path, the prepared env is read-only, and the supervisor enforces
//!   wall-clock, CPU, memory, process and file-size limits.
//!
//! The host check fails closed at startup (C11): Linux with unprivileged
//! user namespaces and `bwrap` + `uv` on PATH are required before the
//! frontend serves anything.

pub mod envprep;
pub mod exec;
pub mod limits;
pub mod workspace;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::SandboxConfig;
use crate::error::{ApiError, ApiErrorKind, Result};

pub use envprep::{EnvId, env_id, normalize_deps, prepared};
pub use exec::{MAX_STREAM_BYTES, ScriptOutcome, WORKSPACE_MOUNT};
pub use limits::Limits;
pub use workspace::{
    inline_mime, is_internal_name, mime_hint, resolve_workspace_file, safe_file_name,
};

/// Most files the artifact scan tracks; a workspace beyond this still runs,
/// it just stops surfacing new files.
pub const MAX_SCAN_ENTRIES: usize = 2048;

/// Host-requirement help shown when the startup check fails (C11).
pub const HOST_HELP: &str = "\
Kaeru's Python sandbox needs Linux with unprivileged user namespaces and the
`bwrap` (bubblewrap) and `uv` tools on PATH. Install them, e.g.
  Arch:            pacman -S bubblewrap uv
  Debian/Ubuntu:   apt install bubblewrap uv
  Fedora:          dnf install bubblewrap uv
If your kernel gates user namespaces, enable them
(sysctl kernel.unprivileged_userns_clone=1).";

/// One file in a workspace snapshot (M5 artifact detection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    /// Workspace-relative path with `/` separators.
    pub path: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// The sandbox supervisor: paths, the system interpreter, and the limits.
pub struct Sandbox {
    workspace: PathBuf,
    envs: PathBuf,
    read_paths: Vec<PathBuf>,
    python: PathBuf,
    limits: Limits,
    /// Serializes env-prep so two turns cannot install into the same id.
    preparer: tokio::sync::Mutex<()>,
}

impl Sandbox {
    /// Create the workspace/env directories and resolve host tools. This does
    /// not check the host; call [`Sandbox::check_host`] at startup.
    pub fn new(config: &SandboxConfig, envs: impl Into<PathBuf>) -> Result<Self> {
        let envs = envs.into();
        let workspace = config.workspace.clone();
        for dir in [&workspace, &envs] {
            std::fs::create_dir_all(dir)
                .map_err(|e| ApiError::config(format!("cannot create {}: {e}", dir.display())))?;
        }
        let read_paths = config
            .read_paths
            .iter()
            .filter_map(|path| match std::fs::canonicalize(path) {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!(
                        target: "agent_core::sandbox",
                        path = %path.display(),
                        "read_paths entry is not usable and is skipped: {e}"
                    );
                    None
                }
            })
            .collect();
        Ok(Self {
            workspace,
            envs,
            read_paths,
            python: find_python()?,
            limits: Limits::from_config(config),
            preparer: tokio::sync::Mutex::new(()),
        })
    }

    /// The one writable folder inside the sandbox.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn envs(&self) -> &Path {
        &self.envs
    }

    /// The system interpreter used for dependency-free runs and venv seeds.
    pub fn python(&self) -> &Path {
        &self.python
    }

    pub fn read_paths(&self) -> &[PathBuf] {
        &self.read_paths
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Verify the host can actually run the sandbox: `bwrap` and `uv` exist,
    /// and a probe run proves user namespaces, the minimal mounts, and the
    /// system interpreter all work. Fail closed with instructions (C11).
    pub fn check_host(&self) -> Result<()> {
        for (tool, args) in [("bwrap", ["--version"]), ("uv", ["--version"])] {
            let output = std::process::Command::new(tool).args(args).output();
            match output {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    return Err(ApiError::config(format!(
                        "`{tool} {}` failed: {}\n{HOST_HELP}",
                        args.join(" "),
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
                Err(e) => {
                    return Err(ApiError::config(format!(
                        "cannot run `{tool}`: {e}\n{HOST_HELP}"
                    )));
                }
            }
        }
        if !self.python.is_file() {
            return Err(ApiError::config(format!(
                "system Python {} does not exist\n{HOST_HELP}",
                self.python.display()
            )));
        }
        // The probe is the real thing: unshare + minimal ro binds + exec.
        let mut probe = std::process::Command::new("bwrap");
        for value in [
            "--unshare-all",
            "--new-session",
            "--die-with-parent",
            "--clearenv",
            "--ro-bind",
            "/usr",
            "/usr",
        ] {
            probe.arg(value);
        }
        for path in ["/bin", "/lib", "/lib64", "/sbin", "/etc"] {
            probe.args(["--ro-bind-try", path, path]);
        }
        probe.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        probe.args(["--setenv", "PATH", "/usr/bin:/bin"]);
        probe.args(["--", &self.python.to_string_lossy(), "-c", "pass"]);
        match probe.output() {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(ApiError::config(format!(
                "bubblewrap cannot create the sandbox: {}\n{HOST_HELP}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
            Err(e) => Err(ApiError::config(format!(
                "bubblewrap probe failed to start: {e}\n{HOST_HELP}"
            ))),
        }
    }

    /// True when the dep set's environment is already prepared (sync: used by
    /// a tool's `risk()` to skip the consent card for a cached set).
    pub fn env_prepared(&self, deps: &[String]) -> bool {
        prepared(&self.envs, deps)
    }

    /// Prepare (or reuse) the env for `deps`; serialized in-process.
    pub async fn prepare_env(&self, deps: &[String]) -> Result<EnvId> {
        let _guard = self.preparer.lock().await;
        envprep::prepare(&self.envs, &self.python, deps).await
    }

    /// Snapshot workspace files for artifact detection, bounded and sorted.
    pub fn snapshot(&self) -> Vec<FileStamp> {
        let mut files = Vec::new();
        collect(&self.workspace, &self.workspace, &mut files);
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// Run one script (Phase B). Dependency-free scripts use the system
    /// interpreter; otherwise the prepared env is mounted read-only.
    pub async fn run_script(
        &self,
        script: &str,
        deps: &[String],
        network: bool,
    ) -> Result<ScriptOutcome> {
        let env = if normalize_deps(deps).is_empty() {
            None
        } else {
            let id = self.prepare_env(deps).await?;
            Some(self.envs.join(id.as_str()))
        };
        let spec = exec::RunSpec {
            workspace: &self.workspace,
            env: env.as_deref(),
            read_paths: &self.read_paths,
            python: &self.python,
            network,
            limits: self.limits,
        };
        exec::run(&spec, script).await
    }
}

/// Locate the system Python used to seed venvs and to run dependency-free
/// scripts. Prefers `python3` on PATH, then `/usr/bin/python3`.
fn find_python() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("python3");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let fallback = PathBuf::from("/usr/bin/python3");
    if fallback.is_file() {
        return Ok(fallback);
    }
    Err(ApiError::new(
        ApiErrorKind::Config,
        "python3 not found on PATH; the Python tool needs a system interpreter",
    ))
}

/// Recursive, bounded workspace walk (symlinks are reported, not followed).
fn collect(root: &Path, dir: &Path, files: &mut Vec<FileStamp>) {
    if files.len() >= MAX_SCAN_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_SCAN_ENTRIES {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_internal_name(name) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect(root, &path, files);
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        files.push(FileStamp {
            path: relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-sb-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sandbox(name: &str) -> Sandbox {
        let root = temp_dir(name);
        let config = SandboxConfig {
            workspace: root.join("ws"),
            read_paths: Vec::new(),
            timeout_secs: 10,
            memory_mb: 128,
        };
        Sandbox::new(&config, root.join("envs")).unwrap()
    }

    #[test]
    fn new_creates_the_workspace_and_envs_dirs() {
        let sandbox = sandbox("dirs");
        assert!(sandbox.workspace().is_dir());
        assert!(sandbox.envs().is_dir());
        assert_eq!(sandbox.limits().wall_secs, 10);
        assert_eq!(sandbox.limits().memory_bytes, 128 * 1024 * 1024);
    }

    fn sandbox_with(name: &str, timeout_secs: u64, memory_mb: u64) -> Sandbox {
        let root = temp_dir(name);
        let config = SandboxConfig {
            workspace: root.join("ws"),
            read_paths: Vec::new(),
            timeout_secs,
            memory_mb,
        };
        Sandbox::new(&config, root.join("envs")).unwrap()
    }

    /// True when the host can actually sandbox; integration tests skip
    /// (with a note) rather than fail where bwrap cannot run.
    fn usable(sandbox: &Sandbox) -> bool {
        match sandbox.check_host() {
            Ok(()) => true,
            Err(err) => {
                eprintln!("skipping sandbox integration test: {err}");
                false
            }
        }
    }

    #[tokio::test]
    async fn a_script_runs_offline_and_the_workspace_is_the_only_writable_path() {
        let sandbox = sandbox_with("run", 20, 512);
        if !usable(&sandbox) {
            return;
        }
        let script = r#"
results = []
try:
    open('/etc/kaeru-escape.txt', 'w').write('x')
    results.append('ESCAPED-ETC')
except Exception:
    results.append('blocked-etc')
try:
    open('../kaeru-escape.txt', 'w').write('x')
    results.append('ESCAPED-PARENT')
except Exception:
    results.append('blocked-parent')
open('ok.txt', 'w').write('x')
results.append('wrote-workspace')
print('|'.join(results))
"#;
        let outcome = sandbox.run_script(script, &[], false).await.unwrap();
        assert!(outcome.success(), "{outcome:?}");
        assert!(outcome.stdout.contains("blocked-etc"), "{outcome:?}");
        assert!(outcome.stdout.contains("blocked-parent"), "{outcome:?}");
        assert!(outcome.stdout.contains("wrote-workspace"), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(sandbox.workspace().join("ok.txt")).unwrap(),
            "x"
        );
        assert!(!std::path::Path::new("/etc/kaeru-escape.txt").exists());
    }

    #[tokio::test]
    async fn a_script_cannot_open_a_network_connection() {
        let sandbox = sandbox_with("net", 20, 512);
        if !usable(&sandbox) {
            return;
        }
        let script = r#"
import socket
try:
    socket.create_connection(('1.1.1.1', 80), timeout=3)
    print('NETWORK-OPEN')
except Exception as err:
    print('no-network:', type(err).__name__)
"#;
        let outcome = sandbox.run_script(script, &[], false).await.unwrap();
        assert!(outcome.stdout.contains("no-network"), "{outcome:?}");
        assert!(!outcome.stdout.contains("NETWORK-OPEN"), "{outcome:?}");
    }

    #[tokio::test]
    async fn the_wall_clock_timeout_kills_a_hung_script() {
        let sandbox = sandbox_with("timeout", 1, 256);
        if !usable(&sandbox) {
            return;
        }
        let outcome = sandbox
            .run_script(
                "import time\nprint('started', flush=True)\ntime.sleep(60)\n",
                &[],
                false,
            )
            .await
            .unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(!outcome.success());
        assert!(
            outcome.stdout.contains("started"),
            "output before the kill is kept: {outcome:?}"
        );
        assert!(
            outcome.duration_ms < 15_000,
            "killed promptly, took {} ms",
            outcome.duration_ms
        );
    }

    #[tokio::test]
    async fn extra_read_paths_are_visible_and_read_only() {
        let root = temp_dir("reads");
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("notes.txt"), b"hello-read-path").unwrap();
        let config = SandboxConfig {
            workspace: root.join("ws"),
            read_paths: vec![shared.clone()],
            timeout_secs: 20,
            memory_mb: 512,
        };
        let sandbox = Sandbox::new(&config, root.join("envs")).unwrap();
        if !usable(&sandbox) {
            return;
        }
        let script = format!(
            r#"
print(open('{path}/notes.txt').read())
try:
    open('{path}/notes.txt', 'a').write('x')
    print('WROTE-READ-PATH')
except Exception:
    print('read-only')
"#,
            path = shared.display()
        );
        let outcome = sandbox.run_script(&script, &[], false).await.unwrap();
        assert!(outcome.stdout.contains("hello-read-path"), "{outcome:?}");
        assert!(!outcome.stdout.contains("WROTE-READ-PATH"), "{outcome:?}");
        assert!(outcome.stdout.contains("read-only"), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(shared.join("notes.txt")).unwrap(),
            "hello-read-path",
            "the host copy is untouched"
        );
    }

    #[tokio::test]
    async fn the_memory_limit_stops_an_allocation() {
        let sandbox = sandbox_with("memory", 20, 128);
        if !usable(&sandbox) {
            return;
        }
        let outcome = sandbox
            .run_script(
                "x = bytearray(400 * 1024 * 1024)\nprint('ALLOCATED')\n",
                &[],
                false,
            )
            .await
            .unwrap();
        assert!(!outcome.success(), "{outcome:?}");
        assert!(!outcome.stdout.contains("ALLOCATED"), "{outcome:?}");
    }

    #[test]
    fn snapshot_lists_files_relative_and_skips_internal_names() {
        let sandbox = sandbox("snapshot");
        std::fs::write(sandbox.workspace().join("plot.png"), b"png").unwrap();
        std::fs::create_dir_all(sandbox.workspace().join("sub")).unwrap();
        std::fs::write(sandbox.workspace().join("sub/data.csv"), b"a,b").unwrap();
        std::fs::write(sandbox.workspace().join(".hidden"), b"x").unwrap();
        std::fs::create_dir_all(sandbox.workspace().join("__pycache__")).unwrap();
        std::fs::write(sandbox.workspace().join("__pycache__/m.pyc"), b"x").unwrap();

        let paths: Vec<String> = sandbox
            .snapshot()
            .into_iter()
            .map(|stamp| stamp.path)
            .collect();
        assert_eq!(paths, vec!["plot.png", "sub/data.csv"]);
    }
}
