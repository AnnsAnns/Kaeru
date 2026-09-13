//! The Python sandbox (M5, ADR-012/013): [`envprep`] prepares a uv
//! environment (the only networked phase, consent-gated), [`exec`] runs one
//! script under `bwrap` with no network, a writable workspace only, and
//! wall-clock/CPU/memory/process limits. The host check fails closed at
//! startup (C11) when `bwrap`/`uv` or userns support are missing.

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
mod tests;
