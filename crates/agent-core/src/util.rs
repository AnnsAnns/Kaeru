//! Small shared text helpers. One home for the token estimate, the truncation
//! rule and the `tags:` line parser that used to be copied per module.

use std::path::Path;

use crate::error::{ApiError, Result};

/// Deterministic token estimate: ~4 characters per token (ADR-018).
/// Deliberately crude and stable — the budget governs shape, not billing.
pub(crate) const CHARS_PER_TOKEN: u64 = 4;

/// Truncate `text` to at most `max_chars` characters, marking the cut with a
/// single ellipsis. Never splits a character; `max_chars == 0` yields empty.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Parse a worker's `tags: a, b` line (or a bare comma list) into clean tags:
/// optional `tags:`/`Tags:` prefix, brackets and quotes stripped, blanks
/// dropped.
pub(crate) fn parse_tag_line(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line
        .strip_prefix("tags:")
        .or_else(|| line.strip_prefix("Tags:"))
        .unwrap_or(line);
    line.split(',')
        .map(|tag| tag.trim().trim_matches(['[', ']', '"', '\'']).to_owned())
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// Write `contents` to `path` atomically: a `<ext>.tmp` sibling, then rename
/// over the target, so a crash never leaves a half-written file. The parent
/// directory is created when missing.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal(format!("cannot create dir {}: {e}", parent.display()))
        })?;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
    let tmp = path.with_extension(format!("{ext}.tmp"));
    std::fs::write(&tmp, contents)
        .map_err(|e| ApiError::internal(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ApiError::internal(format!("cannot finalize {}: {e}", path.display())))
}

/// A fresh per-test directory under the system temp dir, unique per process
/// and `name`.
#[cfg(test)]
pub(crate) fn temp_dir(suite: &str, name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kaeru-{suite}-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_never_exceeds_the_character_cap() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abc", 0), "");
        let cut = truncate_chars("hello world", 5);
        assert_eq!(cut, "hell…");
        assert_eq!(cut.chars().count(), 5);
    }

    #[test]
    fn parse_tag_line_tolerates_worker_shapes() {
        assert_eq!(parse_tag_line("tags: rust, axum"), vec!["rust", "axum"]);
        assert_eq!(parse_tag_line("Tags: [frogs]"), vec!["frogs"]);
        assert_eq!(parse_tag_line("plain, \"quoted\""), vec!["plain", "quoted"]);
        assert!(parse_tag_line("   ").is_empty());
    }

    #[test]
    fn write_atomic_creates_the_parent_and_writes_the_file() {
        let dir = std::env::temp_dir().join(format!("kaeru-util-{}", std::process::id()));
        let path = dir.join("nested").join("note.md");
        write_atomic(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!dir.join("nested").join("md.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
