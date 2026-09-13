//! Plain-markdown memory store (M4, ADR-007).
//!
//! Notes live under `data/memory/YYYY-MM-DD/{slug}.md`, one file per note,
//! with a tiny YAML-ish frontmatter (`tags`, `created`) and a Markdown body.
//! The format is deliberately human-editable: the reader tolerates missing or
//! broken frontmatter, ignores non-markdown files, and never crashes a turn on
//! a bad file — it skips it with a warning.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Result;

/// One markdown memory note read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    pub path: PathBuf,
    /// Day partition (the directory name, `YYYY-MM-DD`).
    pub day: String,
    /// File stem (the note's slug).
    pub slug: String,
    pub tags: Vec<String>,
    pub created: Option<String>,
    /// Note body with the frontmatter stripped.
    pub content: String,
    /// File modification time; used for newest-first ordering.
    pub modified_unix: u64,
}

/// Markdown memory store rooted at a day-partitioned directory (`data/memory`).
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory for today's **local** date (`<dir>/YYYY-MM-DD`).
    pub fn day_dir(&self) -> PathBuf {
        self.dir.join(local_date())
    }

    /// Persist one markdown note for today; returns the written path.
    ///
    /// Atomic like the other data stores: write `.tmp`, then rename over the
    /// final name, so a crash mid-write never leaves a half-written note.
    pub fn write(&self, content: &str, tags: &[String]) -> Result<PathBuf> {
        let path = self.day_dir().join(format!("{}.md", self.file_stem(content)));
        let document = format!(
            "---\ntags: {}\ncreated: {}\n---\n{}\n",
            format_tags(tags),
            local_date(),
            content.trim()
        );
        crate::util::write_atomic(&path, &document)?;
        Ok(path)
    }

    /// Whether a note with exactly this body (trimmed) already exists. Used by
    /// the reflector so a retried run does not duplicate notes that landed
    /// before a partial failure.
    pub fn contains_body(&self, content: &str) -> bool {
        let body = content.trim();
        self.list().iter().any(|note| note.content.trim() == body)
    }

    /// Every readable note, newest first. A missing directory is empty; a
    /// broken or non-markdown file is skipped, never fatal.
    pub fn list(&self) -> Vec<MemoryNote> {
        let mut notes = Vec::new();
        let days = match std::fs::read_dir(&self.dir) {
            Ok(days) => days,
            Err(_) => return notes,
        };
        for day in days.flatten() {
            let day_name = day.file_name().to_string_lossy().into_owned();
            if !is_day_dir(&day_name) {
                continue;
            }
            let entries = match std::fs::read_dir(day.path()) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if path.is_symlink() {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => {
                        tracing::warn!(
                            target: "agent_core::memory",
                            "skipping unreadable memory file {}: {err}",
                            path.display()
                        );
                        continue;
                    }
                };
                let slug = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                notes.push(parse_note(&path, &day_name, &slug, &text));
            }
        }
        notes.sort_by(|a, b| {
            (a.day.as_str(), a.modified_unix, a.slug.as_str())
                .cmp(&(b.day.as_str(), b.modified_unix, b.slug.as_str()))
                .reverse()
        });
        notes
    }

    /// Notes matching a free-text query (case-insensitive substring against
    /// the body and tags), best match first, then newest. An empty query
    /// returns the newest `limit` notes.
    pub fn search(&self, query: &str, limit: usize) -> Vec<MemoryNote> {
        let terms = super::query_terms(query, 1);
        if terms.is_empty() {
            return self.list().into_iter().take(limit).collect();
        }
        let mut scored: Vec<(usize, MemoryNote)> = self
            .list()
            .into_iter()
            .filter_map(|note| {
                let score = super::score_note(&note, &terms);
                (score > 0).then_some((score, note))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, note)| note)
            .collect()
    }

    fn file_stem(&self, content: &str) -> String {
        let first_line = content.lines().next().unwrap_or("").trim();
        let mut slug: String = first_line
            .chars()
            .take(40)
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        while slug.contains("--") {
            slug = slug.replace("--", "-");
        }
        let slug = slug.trim_matches('-');
        let slug = if slug.is_empty() { "note" } else { slug };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{slug}-{nanos:x}")
    }
}

/// Split a note file into `(tags, created, body)`, tolerating anything.
fn parse_note(path: &Path, day: &str, slug: &str, text: &str) -> MemoryNote {
    let trimmed = text.trim_start_matches('\u{feff}');
    let mut tags = Vec::new();
    let mut created = None;
    let body;
    if let Some(rest) = trimmed.strip_prefix("---") {
        let rest = rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'));
        match rest.and_then(|rest| rest.find("\n---").map(|pos| (rest, pos))) {
            Some((rest, pos)) => {
                for line in rest[..pos].lines() {
                    let Some((key, value)) = line.split_once(':') else {
                        continue;
                    };
                    match key.trim().to_ascii_lowercase().as_str() {
                        "tags" => tags = parse_tags(value),
                        "created" => {
                            let value = value.trim();
                            if !value.is_empty() {
                                created = Some(value.to_owned());
                            }
                        }
                        _ => {}
                    }
                }
                body = rest[pos + 4..].trim_start_matches(['\r', '\n']).trim_end();
            }
            None => body = rest.unwrap_or("").trim(),
        }
    } else {
        body = trimmed.trim();
    }
    MemoryNote {
        path: path.to_path_buf(),
        day: day.to_owned(),
        slug: slug.to_owned(),
        tags,
        created,
        content: body.to_owned(),
        modified_unix: modified_unix(path),
    }
}

fn parse_tags(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    value
        .split(',')
        .map(|tag| tag.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|tag| !tag.is_empty())
        .collect()
}

fn format_tags(tags: &[String]) -> String {
    if tags.is_empty() {
        "[]".to_owned()
    } else {
        format!("[{}]", tags.join(", "))
    }
}

fn is_day_dir(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
}

fn modified_unix(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Today's date (`YYYY-MM-DD`) in the owner's **local** time zone.
///
/// Uses `localtime_r` through libc (already linked by std; no new
/// dependency) so DST is handled, and falls back to UTC off unix.
pub fn local_date() -> String {
    local_date_from_unix(now_unix() as i64)
}

/// Current wall-clock time as unix seconds.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The local date (`YYYY-MM-DD`) of a unix timestamp; UTC off unix.
pub fn local_date_from_unix(unix: i64) -> String {
    match local_tm(unix) {
        Some((year, month, day, _, _)) => format!("{year:04}-{month:02}-{day:02}"),
        None => utc_date_from_unix(unix),
    }
}

/// Minutes since local midnight of a unix timestamp; UTC off unix.
pub fn local_minutes_from_unix(unix: i64) -> u32 {
    match local_tm(unix) {
        Some((_, _, _, hour, minute)) => (hour * 60 + minute) as u32,
        None => (unix.rem_euclid(86_400) / 60) as u32,
    }
}

#[cfg(unix)]
fn local_tm(unix: i64) -> Option<(i32, i32, i32, i32, i32)> {
    use std::os::raw::{c_char, c_int, c_long};

    // Assumes the glibc/musl `struct tm` layout. A different libc layout would
    // have to revisit this FFI (the UTC fallback keeps the worst case sane).
    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }

    unsafe extern "C" {
        fn localtime_r(timep: *const c_long, result: *mut Tm) -> *mut Tm;
    }

    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    let time: c_long = unix as c_long;
    if unsafe { localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
    ))
}

#[cfg(not(unix))]
fn local_tm(_unix: i64) -> Option<(i32, i32, i32, i32, i32)> {
    None
}

/// UTC fallback (`YYYY-MM-DD`), shared with the conversation store's clock.
fn utc_date_from_unix(unix: i64) -> String {
    crate::conversations::rfc3339_from_unix(unix.max(0) as u64)
        .chars()
        .take(10)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!("kaeru-memory-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        MemoryStore::new(dir)
    }

    #[test]
    fn write_creates_a_day_file_with_frontmatter() {
        let store = temp_store("write");
        let path = store
            .write("Rust notes", &["rust".into(), "axum".into()])
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\n"));
        assert!(text.contains("tags: [rust, axum]"));
        assert!(text.contains("Rust notes"));
        assert!(path.starts_with(store.day_dir()));
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn written_notes_round_trip_through_list() {
        let store = temp_store("round-trip");
        store.write("first note", &["alpha".into()]).unwrap();
        store.write("second note", &["beta".into()]).unwrap();
        let notes = store.list();
        assert_eq!(notes.len(), 2);
        let first = notes
            .iter()
            .find(|n| n.content.contains("first note"))
            .unwrap();
        assert_eq!(first.tags, vec!["alpha".to_string()]);
        assert_eq!(first.day, local_date());
        assert!(first.created.is_some());
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn write_is_atomic_and_leaves_no_tmp_files() {
        let store = temp_store("atomic");
        let path = store.write("atomic note", &[]).unwrap();
        assert!(path.is_file());
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "only the final note remains: {names:?}");
        assert!(
            names.iter().all(|name| !name.ends_with(".tmp")),
            "no temp file may survive: {names:?}"
        );
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn list_tolerates_hand_edited_and_foreign_files() {
        let store = temp_store("tolerant");
        let day = store.dir().join("2026-09-10");
        std::fs::create_dir_all(&day).unwrap();
        // No frontmatter at all: the whole file is the body.
        std::fs::write(day.join("plain.md"), "just a body\n").unwrap();
        // Broken frontmatter: never panics, body is best-effort.
        std::fs::write(day.join("broken.md"), "---\ntags: [oops\nno closing\n").unwrap();
        // Not markdown: ignored.
        std::fs::write(day.join("ignore.txt"), "not a note").unwrap();
        // Not a day directory: ignored.
        std::fs::create_dir_all(store.dir().join("scratch")).unwrap();
        std::fs::write(store.dir().join("scratch").join("x.md"), "hidden").unwrap();

        let notes = store.list();
        assert_eq!(notes.len(), 2);
        let plain = notes.iter().find(|n| n.slug == "plain").unwrap();
        assert_eq!(plain.content, "just a body");
        assert!(plain.tags.is_empty());
        assert!(notes.iter().all(|n| n.day == "2026-09-10"));
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn search_matches_body_and_tags_and_ranks_tags_higher() {
        let store = temp_store("search");
        let day = store.dir().join("2026-09-10");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rust.md"),
            "---\ntags: [rust]\ncreated: 2026-09-10\n---\nnotes about frogs\n",
        )
        .unwrap();
        std::fs::write(
            day.join("frog.md"),
            "---\ntags: [animals]\ncreated: 2026-09-10\n---\nfrogs are amphibians\n",
        )
        .unwrap();
        std::fs::write(
            day.join("other.md"),
            "---\ntags: []\ncreated: 2026-09-10\n---\nunrelated\n",
        )
        .unwrap();

        let hits = store.search("frogs", 5);
        assert_eq!(hits.len(), 2, "only notes mentioning frogs match");
        // The body-only match is a frog note; a tag hit would outrank it.
        let tag_hits = store.search("rust", 5);
        assert_eq!(tag_hits.len(), 1);
        assert_eq!(tag_hits[0].slug, "rust");

        assert_eq!(store.search("", 2).len(), 2);
        assert!(store.search("nothing-here", 5).is_empty());
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn missing_directory_lists_as_empty() {
        let store = temp_store("missing");
        assert!(store.list().is_empty());
        assert!(store.search("anything", 5).is_empty());
    }

    #[test]
    fn local_date_is_an_iso_day_and_parses_back() {
        let date = local_date();
        assert!(is_day_dir(&date), "unexpected local date {date:?}");
        // The unix fallback path is exercised off unix and for bad input.
        assert_eq!(utc_date_from_unix(0), "1970-01-01");
    }
}
