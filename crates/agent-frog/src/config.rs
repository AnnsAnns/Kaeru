//! Frog client configuration (M6): where the daemon lives, the shared
//! `X-Auth-Token`, and the cosmetic bits (theme, corner, avatar).
//!
//! Deliberately separate from the daemon's own `data/config.toml`: the frog may
//! run on a *different* tailnet device, where the daemon's data dir does not
//! exist. This file holds only what a client needs. Like the daemon config it
//! is written `0600` (it carries the token) and unknown keys are hard errors
//! (`deny_unknown_fields`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default daemon URL: the localhost bind (ADR-030). Point it at a tailnet
/// address (`http://100.x.y.z:8080`) to reach a daemon on another device.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8080";
/// Default starting theme; must match a [`crate::theme::THEMES`] id.
pub const DEFAULT_THEME: &str = "latenightbath";
/// Default screen corner for the layer-shell surface.
pub const DEFAULT_CORNER: &str = "bottom-right";
/// Default avatar until the owner drops in sprite frames.
pub const DEFAULT_AVATAR: &str = "🐸";

fn default_url() -> String {
    DEFAULT_URL.to_owned()
}
fn default_theme() -> String {
    DEFAULT_THEME.to_owned()
}
fn default_corner() -> String {
    DEFAULT_CORNER.to_owned()
}
fn default_avatar() -> String {
    DEFAULT_AVATAR.to_owned()
}
fn default_fps() -> u32 {
    4
}
fn default_margin() -> i32 {
    18
}

/// Where the frog sits, as the two margins against its anchored corner (so a
/// drag persists across restarts without hard-coding screen coordinates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionConfig {
    /// Margin against the horizontal edge (left or right, per `corner`).
    #[serde(default = "default_margin")]
    pub h: i32,
    /// Margin against the vertical edge (top or bottom, per `corner`).
    #[serde(default = "default_margin")]
    pub v: i32,
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            h: default_margin(),
            v: default_margin(),
        }
    }
}

/// Sprite frames (M6 follow-up seam): leave `frames` empty to draw the emoji
/// `avatar`; drop real images in and they animate at `fps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpriteConfig {
    /// Image paths (PNG/SVG/…), cycled in order. Empty = use the emoji.
    #[serde(default)]
    pub frames: Vec<PathBuf>,
    /// Animation speed for a multi-frame sprite.
    #[serde(default = "default_fps")]
    pub fps: u32,
}

impl Default for SpriteConfig {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            fps: default_fps(),
        }
    }
}

/// The frog's on-disk config (`~/.config/kaeru/frog.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrogConfig {
    /// Daemon base URL. Localhost by default; a tailnet address for M6 remote
    /// use (`[daemon] bind` + `auth_token` on the server side, ADR-030).
    #[serde(default = "default_url")]
    pub url: String,
    /// Shared secret sent as `X-Auth-Token` on every `/api/*` request. Required
    /// whenever the daemon binds past localhost.
    #[serde(default)]
    pub token: String,
    /// Theme id (see [`crate::theme::THEMES`]).
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Where the widget sits: `bottom-right`, `bottom-left`, `top-right` or
    /// `top-left`.
    #[serde(default = "default_corner")]
    pub corner: String,
    /// Pin a specific thread id. Empty = the newest thread, created on demand.
    #[serde(default)]
    pub thread: String,
    /// Emoji avatar, used when `[sprite] frames` is empty.
    #[serde(default = "default_avatar")]
    pub avatar: String,
    #[serde(default)]
    pub sprite: SpriteConfig,
    /// Persisted drag position (margins against the anchored corner).
    #[serde(default)]
    pub position: PositionConfig,
}

impl Default for FrogConfig {
    fn default() -> Self {
        Self {
            url: default_url(),
            token: String::new(),
            theme: default_theme(),
            corner: default_corner(),
            thread: String::new(),
            avatar: default_avatar(),
            sprite: SpriteConfig::default(),
            position: PositionConfig::default(),
        }
    }
}

impl FrogConfig {
    /// The token, normalized: an empty string means "none configured".
    pub fn token(&self) -> Option<&str> {
        let trimmed = self.token.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    /// Base URL with any trailing slash trimmed, so `format!("{base}/api/...")`
    /// never doubles up.
    pub fn base_url(&self) -> &str {
        self.url.trim().trim_end_matches('/')
    }

    /// Load from `path`, or return defaults when the file does not exist yet.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|err| format!("cannot parse {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("cannot read {}: {err}", path.display())),
        }
    }

    /// Persist atomically at `0600` (the file carries the auth token).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self)
            .map_err(|err| format!("cannot serialize config: {err}"))?;
        write_private(path, &text)
    }
}

/// `$XDG_CONFIG_HOME/kaeru/frog.toml`, else `~/.config/kaeru/frog.toml`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("kaeru").join("frog.toml")
}

/// Write `contents` to `path` via a temp file + rename, mode `0600`.
fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)
        .map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("cannot chmod {}: {err}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|err| format!("cannot replace {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_defaults() {
        let config: FrogConfig = toml::from_str("").unwrap();
        assert_eq!(config, FrogConfig::default());
        assert_eq!(config.base_url(), DEFAULT_URL);
        assert_eq!(config.token(), None);
    }

    #[test]
    fn unknown_keys_are_hard_errors() {
        let err = toml::from_str::<FrogConfig>("nope = 1").unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn round_trips_through_toml() {
        let config = FrogConfig {
            url: "http://100.64.0.2:8080/".into(),
            token: " secret ".into(),
            thread: "frog".into(),
            sprite: SpriteConfig {
                frames: vec![PathBuf::from("a.png"), PathBuf::from("b.png")],
                fps: 8,
            },
            ..FrogConfig::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let back: FrogConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, config);
        assert_eq!(back.base_url(), "http://100.64.0.2:8080");
        assert_eq!(back.token(), Some("secret"));
    }

    #[test]
    fn save_then_load_round_trips_and_is_private() {
        let dir = std::env::temp_dir().join(format!("kaeru-frog-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("frog.toml");
        let config = FrogConfig {
            token: "t".into(),
            ..FrogConfig::default()
        };
        config.save(&path).unwrap();
        assert_eq!(FrogConfig::load(&path).unwrap(), config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
