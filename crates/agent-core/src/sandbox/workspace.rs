//! Workspace path hygiene (M5): the rules every frontend's file flow shares
//! (ADR-017) — uploads are ordinary single-component names, serving cannot
//! escape the workspace (no traversal, no symlink escape), and artifact MIME
//! hints are guessed from the extension.

use std::path::{Path, PathBuf};

use crate::error::{ApiError, ApiErrorKind, Result};

/// Longest accepted file name in bytes (the common Linux filesystem limit).
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// Validate one uploaded file name: a single, ordinary path component. No
/// separators, no parent links, no hidden/internal names, no control
/// characters.
pub fn safe_file_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(ApiError::config("a file name is required"));
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err(ApiError::config(format!(
            "file name is too long (max {MAX_FILE_NAME_BYTES} bytes)"
        )));
    }
    if name.starts_with('.') {
        return Err(ApiError::config(
            "file names may not start with a dot (internal names are reserved)",
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(ApiError::config(
            "file names may not contain path separators",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ApiError::config(
            "file names may not contain control characters",
        ));
    }
    Ok(name.to_owned())
}

/// Resolve a workspace-relative path for serving (M5, ADR-017). Every
/// component must be ordinary and the canonical result must stay inside the
/// workspace, so neither `..` nor a symlink can escape. Errors: `Config` for
/// malformed input, `Forbidden` for escapes, `NotFound` for missing files.
pub fn resolve_workspace_file(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(ApiError::config("a file path is required"));
    }
    if Path::new(rel).is_absolute() {
        return Err(ApiError::new(
            ApiErrorKind::Forbidden,
            "absolute paths are not allowed",
        ));
    }
    let root = std::fs::canonicalize(root).map_err(|e| {
        ApiError::internal(format!("workspace {} is not usable: {e}", root.display()))
    })?;
    let mut candidate = root.clone();
    for component in rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(ApiError::new(
                ApiErrorKind::Forbidden,
                format!("unsafe path component in {rel:?}"),
            ));
        }
        if component.contains('\\') || component.chars().any(char::is_control) {
            return Err(ApiError::new(
                ApiErrorKind::Forbidden,
                format!("unsafe path component in {rel:?}"),
            ));
        }
        candidate.push(component);
    }
    let resolved = std::fs::canonicalize(&candidate).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            ApiError::new(ApiErrorKind::NotFound, format!("no such file: {rel}"))
        }
        _ => ApiError::new(ApiErrorKind::NotFound, format!("cannot read {rel}: {e}")),
    })?;
    if !resolved.starts_with(&root) {
        return Err(ApiError::new(
            ApiErrorKind::Forbidden,
            format!("path escapes the workspace: {rel}"),
        ));
    }
    Ok(resolved)
}

/// Artifact MIME hint from the file extension; `None` for unknown kinds.
pub fn mime_hint(path: &Path) -> Option<&'static str> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "json" => "application/json",
        "txt" | "md" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        _ => return None,
    })
}

/// MIME type a frontend may render inline as an image. SVG is deliberately
/// not inline (it can carry script; serve it as a download instead).
pub fn inline_mime(path: &Path) -> Option<&'static str> {
    match mime_hint(path) {
        Some(mime @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp")) => Some(mime),
        _ => None,
    }
}

/// Names the artifact scan ignores: hidden files (internal bookkeeping) and
/// Python bytecode caches.
pub fn is_internal_name(name: &str) -> bool {
    name.starts_with('.') || name == "__pycache__" || name.ends_with(".pyc")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::temp_dir;

    #[test]
    fn upload_names_must_be_single_ordinary_components() {
        assert_eq!(safe_file_name("notes.csv").unwrap(), "notes.csv");
        for bad in [
            "",
            ".",
            "..",
            ".env",
            "a/b",
            "a\\b",
            "a\nb",
            "x".repeat(256).as_str(),
        ] {
            assert!(safe_file_name(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(safe_file_name("wéird name.txt").is_ok());
    }

    #[test]
    fn resolution_reads_inside_the_workspace_and_refuses_escapes() {
        let root = temp_dir("ws", "resolve");
        std::fs::write(root.join("plot.png"), b"png").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/data.csv"), b"a,b").unwrap();

        assert_eq!(
            resolve_workspace_file(&root, "plot.png").unwrap(),
            std::fs::canonicalize(root.join("plot.png")).unwrap()
        );
        assert_eq!(
            resolve_workspace_file(&root, "sub/data.csv").unwrap(),
            std::fs::canonicalize(root.join("sub/data.csv")).unwrap()
        );

        let outside = temp_dir("ws", "resolve-outside");
        std::fs::write(outside.join("secret.txt"), b"no").unwrap();
        for bad in ["../secret.txt", "sub/../../secret.txt", "/etc/passwd"] {
            let err = resolve_workspace_file(&root, bad).unwrap_err();
            assert!(
                err.kind == ApiErrorKind::Forbidden || err.kind == ApiErrorKind::Config,
                "{bad:?} must be refused, got {err:?}"
            );
        }
        assert_eq!(
            resolve_workspace_file(&root, "missing.txt")
                .unwrap_err()
                .kind,
            ApiErrorKind::NotFound
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
            assert_eq!(
                resolve_workspace_file(&root, "link.txt").unwrap_err().kind,
                ApiErrorKind::Forbidden
            );
        }
    }

    #[test]
    fn mime_hints_cover_images_and_common_data_files() {
        assert_eq!(mime_hint(Path::new("plot.PNG")), Some("image/png"));
        assert_eq!(mime_hint(Path::new("data.csv")), Some("text/csv"));
        assert_eq!(mime_hint(Path::new("archive.bin")), None);
        assert_eq!(inline_mime(Path::new("plot.png")), Some("image/png"));
        assert_eq!(inline_mime(Path::new("evil.svg")), None);
        assert_eq!(inline_mime(Path::new("notes.txt")), None);
    }

    #[test]
    fn internal_names_are_ignored_by_the_artifact_scan() {
        for name in [".complete", ".cache", "__pycache__", "mod.pyc"] {
            assert!(is_internal_name(name), "{name}");
        }
        for name in ["plot.png", "data.csv", "pyc-notes.txt"] {
            assert!(!is_internal_name(name), "{name}");
        }
    }
}
