//! Bort design tokens ported to GTK4 CSS (M6, ADR-031; the same eight themes as
//! `agent-server/assets/app.css`, C17: copied by hand, never imported).
//!
//! GTK4 CSS has no custom properties, so the theme is a plain Rust table and
//! the stylesheet is regenerated (and reloaded) whenever the theme changes.
//! The box recipe mirrors the web UI: base background, a 3px ring in the box
//! colour, `rounded-sm`, and a hard offset shadow.

use std::path::PathBuf;

/// One Bort theme. Colours are sRGB triples; the derived colours (accent,
/// error, translucent rings) are computed exactly like `color-mix(in srgb, …)`
/// in `app.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub id: &'static str,
    pub text: (u8, u8, u8),
    pub box_color: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub bd_bg: (u8, u8, u8),
}

/// Bort's theme cycle order (`themes.ts` via `app.js`), which deliberately
/// skips `trans`; the frog mirrors the web UI exactly.
pub const THEMES: [Theme; 7] = [
    Theme {
        id: "latenightbath",
        text: (0x46, 0x42, 0x5e),
        box_color: (0xf6, 0x91, 0x97),
        bg: (0xff, 0xee, 0xcc),
        bd_bg: (0xff, 0xc4, 0xb8),
    },
    Theme {
        id: "ayy4",
        text: (0x00, 0x30, 0x3b),
        box_color: (0xff, 0xce, 0x96),
        bg: (0xf1, 0xf2, 0xda),
        bd_bg: (0xff, 0x77, 0x77),
    },
    Theme {
        id: "curiosities",
        text: (0x46, 0x42, 0x5e),
        box_color: (0xff, 0xb0, 0xa3),
        bg: (0xff, 0xee, 0xcc),
        bd_bg: (0x46, 0x42, 0x5e),
    },
    Theme {
        id: "sunnyswamp",
        text: (0x46, 0x42, 0x5e),
        box_color: (0xd1, 0xad, 0x82),
        bg: (0xdb, 0xd1, 0xb4),
        bd_bg: (0x98, 0xa6, 0x81),
    },
    Theme {
        id: "standard_og",
        text: (0x5e, 0x2b, 0xff),
        box_color: (0xb1, 0xed, 0xe8),
        bg: (0xff, 0xfc, 0xf9),
        bd_bg: (0xff, 0x69, 0x78),
    },
    Theme {
        id: "werwolvdark",
        text: (0xf3, 0xf4, 0xf4),
        box_color: (0x68, 0x62, 0x99),
        bg: (0x1e, 0x21, 0x27),
        bd_bg: (0x24, 0x24, 0x24),
    },
    Theme {
        id: "nostalgia",
        text: (0xb8, 0xb5, 0xb9),
        box_color: (0x53, 0xa7, 0x88),
        bg: (0x13, 0x16, 0x20),
        bd_bg: (0x18, 0x1c, 0x28),
    },
];

impl Theme {
    /// The theme with `id`, or the first theme when the id is unknown (a stale
    /// config must never leave the widget unstyled).
    pub fn by_id(id: &str) -> &'static Theme {
        THEMES
            .iter()
            .find(|theme| theme.id == id)
            .unwrap_or(&THEMES[0])
    }

    /// The next theme in Bort's cycle order.
    pub fn next_id(id: &str) -> &'static str {
        let index = THEMES.iter().position(|theme| theme.id == id).unwrap_or(0);
        THEMES[(index + 1) % THEMES.len()].id
    }

    fn accent(&self) -> (u8, u8, u8) {
        // color-mix(in srgb, box 75%, color-mix(in srgb, bg 50%, text))
        let inner = mix(self.bg, self.text, 0.5);
        mix(self.box_color, inner, 0.75)
    }

    fn error(&self) -> (u8, u8, u8) {
        mix(self.text, self.bg, 0.75)
    }
}

/// Linear sRGB mix, matching CSS `color-mix(in srgb, a p%, b)`.
fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let channel = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    (channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}

fn rgb(color: (u8, u8, u8)) -> String {
    format!("rgb({}, {}, {})", color.0, color.1, color.2)
}

fn rgba(color: (u8, u8, u8), alpha: f32) -> String {
    format!("rgba({}, {}, {}, {alpha})", color.0, color.1, color.2)
}

/// Build the full stylesheet for `theme`.
pub fn stylesheet(theme: &Theme) -> String {
    let accent = theme.accent();
    let error = theme.error();
    let template = include_str!("stylesheet.css.tmpl");
    template
        .replace("%TEXT%", &rgb(theme.text))
        .replace("%BOX%", &rgb(theme.box_color))
        .replace("%BG%", &rgb(theme.bg))
        .replace("%BDBG%", &rgb(theme.bd_bg))
        .replace("%ACCENT%", &rgb(accent))
        .replace("%ERROR%", &rgb(error))
        .replace("%BOX50%", &rgba(theme.box_color, 0.5))
        .replace("%BOX60%", &rgba(theme.box_color, 0.6))
        .replace("%ACCENT50%", &rgba(accent, 0.5))
        .replace("%ERROR50%", &rgba(error, 0.5))
}

/// The two Bort fonts, embedded at compile time from the web UI assets (C17:
/// copied, never imported at runtime). GTK4 CSS has no `@font-face`, so they
/// are handed to fontconfig for the process instead; the stylesheet then
/// references them by family name.
pub fn register_fonts() {
    const FONTS: [(&str, &[u8]); 2] = [
        (
            "NationalPark-Regular.otf",
            include_bytes!("../../agent-server/assets/fonts/NationalPark-Regular.otf"),
        ),
        (
            "Reglisse_Fill.otf",
            include_bytes!("../../agent-server/assets/fonts/Reglisse_Fill.otf"),
        ),
    ];
    let Some(dir) = cache_dir() else {
        return;
    };
    for (name, bytes) in FONTS {
        let path = dir.join(name);
        if std::fs::read(&path).ok().as_deref() != Some(bytes)
            && (std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, bytes).is_err())
        {
            continue;
        }
        let Ok(path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else {
            continue;
        };
        // SAFETY: fontconfig owns the file; the path is a valid NUL-terminated
        // string and a NULL config means "the current configuration".
        let added = unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), path.as_ptr()) };
        if added == 0 {
            tracing::warn!("fontconfig refused the embedded font {name}");
        }
    }
}

#[link(name = "fontconfig")]
unsafe extern "C" {
    /// `FcBool FcConfigAppFontAddFile(FcConfig *, const FcChar8 *)`.
    fn FcConfigAppFontAddFile(config: *mut std::ffi::c_void, file: *const std::ffi::c_char) -> i32;
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("kaeru-frog").join("fonts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_id_falls_back_to_the_first_theme() {
        assert_eq!(Theme::by_id("nostalgia").id, "nostalgia");
        assert_eq!(Theme::by_id("nope").id, THEMES[0].id);
    }

    #[test]
    fn theme_cycle_skips_nothing_and_wraps() {
        assert_eq!(Theme::next_id("latenightbath"), "ayy4");
        assert_eq!(Theme::next_id("nostalgia"), "latenightbath");
    }

    #[test]
    fn mix_matches_color_mix_srgb() {
        // color-mix(in srgb, #000000 50%, #ffffff) = #808080.
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0.5), (128, 128, 128));
        assert_eq!(mix((10, 20, 30), (10, 20, 30), 0.3), (10, 20, 30));
    }

    #[test]
    fn stylesheet_has_no_unreplaced_placeholders() {
        const PLACEHOLDERS: [&str; 10] = [
            "%TEXT%",
            "%BOX%",
            "%BG%",
            "%BDBG%",
            "%ACCENT%",
            "%ERROR%",
            "%BOX50%",
            "%BOX60%",
            "%ACCENT50%",
            "%ERROR50%",
        ];
        for theme in &THEMES {
            let css = stylesheet(theme);
            for placeholder in PLACEHOLDERS {
                assert!(
                    !css.contains(placeholder),
                    "{} left {placeholder} unreplaced",
                    theme.id
                );
            }
            assert!(css.contains(&rgb(theme.bg)), "{} missing bg", theme.id);
        }
    }
}
