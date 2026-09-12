//! Phase A of the Python tool (M5, ADR-013): prepare an ephemeral uv
//! environment for a requested dependency set. This is the **only** phase
//! that touches the network, and it runs only after explicit
//! `PackageInstall` consent. Prepared environments are keyed by a stable hash
//! of the normalized dependency set and reused, so a cached dep set never
//! asks again; execution mounts them read-only (`exec.rs`).

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{ApiError, ApiErrorKind, Result};

/// How long one consented env-prep may take (venv + package install).
pub const ENV_PREP_TIMEOUT: Duration = Duration::from_secs(300);
/// Marker written only after a fully successful install; its presence is what
/// makes an env reusable without new consent.
const COMPLETE_MARKER: &str = ".complete";

/// Identifies a prepared environment: the stable hash of its dep set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvId(String);

impl EnvId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EnvId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Trim, drop empties, and dedupe a requested dependency list, preserving
/// request order (the hash sorts, so order does not split environments).
pub fn normalize_deps(raw: &[String]) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    for dep in raw {
        let dep = dep.trim();
        if dep.is_empty() || deps.iter().any(|known| known == dep) {
            continue;
        }
        deps.push(dep.to_owned());
    }
    deps
}

/// Stable FNV-1a 64-bit hash of the sorted dependency set: the same deps
/// always name the same environment, independent of order and of the Rust
/// toolchain (unlike `DefaultHasher`).
pub fn deps_hash(deps: &[String]) -> String {
    let mut sorted: Vec<&str> = deps.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hash: u64 = 0xcbf29ce484222325;
    for dep in sorted {
        for byte in dep.as_bytes().iter().chain(std::iter::once(&0u8)) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

/// The id a dep set maps to (without touching the disk).
pub fn env_id(deps: &[String]) -> EnvId {
    EnvId(deps_hash(&normalize_deps(deps)))
}

/// True when the environment for `deps` exists and its install completed.
pub fn prepared(envs: &Path, deps: &[String]) -> bool {
    let id = env_id(deps);
    envs.join(id.as_str()).join(COMPLETE_MARKER).is_file()
        && envs.join(id.as_str()).join("bin/python").exists()
}

/// Prepare (or reuse) the environment for `deps`.
///
/// Reuse is checked first, so a cached dep set performs no work and no
/// network access. A failed install leaves no marker and no env: the next
/// consented attempt starts clean.
pub async fn prepare(envs: &Path, python: &Path, deps: &[String]) -> Result<EnvId> {
    let deps = normalize_deps(deps);
    if deps.is_empty() {
        return Err(ApiError::config("no dependencies to prepare"));
    }
    let id = env_id(&deps);
    if prepared(envs, &deps) {
        return Ok(id);
    }
    tokio::fs::create_dir_all(envs).await.map_err(|e| {
        ApiError::internal(format!("cannot create env dir {}: {e}", envs.display()))
    })?;

    let target = envs.join(id.as_str());
    let tmp = envs.join(format!(".{}.tmp-{}", id.as_str(), std::process::id()));
    let cache = envs.join(".cache");
    let _ = tokio::fs::remove_dir_all(&tmp).await;

    let mut venv_args: Vec<OsString> = vec![
        "venv".into(),
        "--python".into(),
        python.as_os_str().to_owned(),
    ];
    venv_args.push(tmp.as_os_str().to_owned());
    run_uv(&venv_args, "uv venv", &cache).await?;

    let mut install_args: Vec<OsString> = vec![
        "pip".into(),
        "install".into(),
        "--python".into(),
        tmp.join("bin/python").into_os_string(),
        "--".into(),
    ];
    install_args.extend(deps.iter().map(OsString::from));
    run_uv(&install_args, "uv pip install", &cache).await?;

    tokio::fs::write(tmp.join(COMPLETE_MARKER), b"")
        .await
        .map_err(|e| ApiError::internal(format!("cannot mark env complete: {e}")))?;
    match tokio::fs::rename(&tmp, &target).await {
        Ok(()) => Ok(id),
        // Another attempt won the race (checked above under the caller's
        // lock in-process; this also covers a second process).
        Err(_) if prepared(envs, &deps) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            Ok(id)
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            Err(ApiError::internal(format!(
                "cannot finalize env {}: {e}",
                target.display()
            )))
        }
    }
}

/// Run one uv subprocess with a hard timeout and bounded error reporting.
/// The uv cache is kept inside the (disposable) envs dir, so env prep never
/// writes outside the sandbox footprint.
async fn run_uv(args: &[OsString], what: &str, cache: &Path) -> Result<()> {
    let mut command = Command::new("uv");
    command
        .args(args)
        .env("UV_LINK_MODE", "copy")
        .env("UV_CACHE_DIR", cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|e| {
        ApiError::new(
            ApiErrorKind::Config,
            format!(
                "cannot run `uv` ({e}). Install uv and make sure it is on PATH \
                 (see docs/arc42-architecture.md, M5)."
            ),
        )
    })?;
    match tokio::time::timeout(ENV_PREP_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ApiError::config(format!(
                "{what} failed ({}): {}",
                output.status,
                tail(&stderr, 800)
            )))
        }
        Ok(Err(e)) => Err(ApiError::internal(format!("{what} failed: {e}"))),
        Err(_) => Err(ApiError::config(format!(
            "{what} timed out after {}s; try again or request fewer packages",
            ENV_PREP_TIMEOUT.as_secs()
        ))),
    }
}

fn tail(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(max_chars))
        .collect();
    tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-env-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dep_sets_normalize_and_hash_order_independently() {
        let a = normalize_deps(&[
            " pandas ".into(),
            "".into(),
            "numpy".into(),
            "pandas".into(),
        ]);
        assert_eq!(a, vec!["pandas", "numpy"]);
        let b = normalize_deps(&["numpy".into(), "pandas".into()]);
        assert_eq!(deps_hash(&a), deps_hash(&b));
        assert_ne!(deps_hash(&a), deps_hash(&["pandas".into()]));
        assert_eq!(deps_hash(&a).len(), 16);
    }

    #[test]
    fn a_completed_env_is_reused_without_running_uv() {
        let envs = temp_dir("reuse");
        let deps = vec!["pandas".to_owned()];
        let id = env_id(&deps);
        assert!(!prepared(&envs, &deps));
        // Simulate a completed install; the prepare path must return before
        // spawning anything, so a nonexistent python is fine here (and this
        // test never touches the network).
        let dir = envs.join(id.as_str());
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join(COMPLETE_MARKER), b"").unwrap();
        std::fs::write(dir.join("bin/python"), b"stub").unwrap();
        assert!(prepared(&envs, &deps));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let prepared_id = runtime
            .block_on(prepare(&envs, Path::new("/nonexistent/python"), &deps))
            .unwrap();
        assert_eq!(prepared_id, id);
    }
}
