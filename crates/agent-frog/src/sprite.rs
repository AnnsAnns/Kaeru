//! The avatar (M6, ADR-031): the built-in `frog_idle` spritesheet with overlay
//! animations for sleeping, speaking and thinking, or user-supplied frame files
//! from `[sprite] frames`.
//!
//! The idle sheet is the base frog (frame 0 rests, the rest is a blink). The
//! other three sheets are transparent overlays composited on top: a dream
//! bubble while sleeping, a mouth while speaking, a thought bubble while
//! thinking. `[sprite] fps` drives the thinking/speaking overlays; the sleeping
//! overlay is deliberately slow (one frame every few seconds). Keeping all of
//! this behind one type means the UI only ever calls [`Avatar::set_state`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::glib;
use gtk::prelude::*;

use crate::config::FrogConfig;

/// The embedded spritesheets: horizontal strips of square frames. `idle` is the
/// base; the others are overlays. Shared with the web UI (served from
/// `agent-server/assets/frog/`), like the fonts.
const IDLE_SHEET: &[u8] = include_bytes!("../../agent-server/assets/frog/frog_idle.png");
const SLEEPING_SHEET: &[u8] = include_bytes!("../../agent-server/assets/frog/frog_sleeping.png");
const SPEAKING_SHEET: &[u8] = include_bytes!("../../agent-server/assets/frog/frog_speaking.png");
const THINKING_SHEET: &[u8] = include_bytes!("../../agent-server/assets/frog/frog_thinking.png");

/// The art is drawn on an 80px canvas (4× the 20px source pixels, so the pixel
/// art stays crisp; frames are pre-scaled with nearest-neighbour).
const ART_SIZE: i32 = 80;
/// The avatar button, with room for the 3px themed ring and a small margin.
const AVATAR_SIZE: i32 = ART_SIZE + 12;
/// How long the idle frog rests between blinks.
const BLINK_REST: Duration = Duration::from_secs(5);
/// The sleeping overlay advances one frame every this long.
const SLEEP_FRAME: Duration = Duration::from_secs(5);
/// The frog dozes off after a random stretch of idle time in this range.
const SLEEP_AFTER: (Duration, Duration) = (Duration::from_secs(120), Duration::from_secs(300));

/// What the frog is doing; drives which overlay is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Thinking,
    Speaking,
    Sleeping,
}

/// The always-visible avatar. Owns the button (for the drag/click gestures) and
/// keeps the animation timer alive.
pub struct Avatar {
    button: gtk::Button,
    timing: Timing,
    inner: Inner,
    /// Keeps the animation/cycle source attached for the avatar's lifetime.
    _source: Option<glib::SourceId>,
}

enum Inner {
    /// The built-in sheets with a state machine.
    Animated {
        picture: gtk::Picture,
        frames: Rc<Frames>,
        anim: Rc<RefCell<Anim>>,
    },
    /// A `[sprite] frames` file list, cycled continuously.
    Custom,
    /// The emoji fallback (the sheets could not be decoded).
    Emoji,
}

impl Avatar {
    /// The button to attach the drag and click gestures to.
    pub fn button(&self) -> &gtk::Button {
        &self.button
    }

    /// Switch animations. Idle restarts the sleep countdown; the other states
    /// start their overlay from the first frame. Setting the current state again
    /// is a no-op, so callers can safely call it on every streamed event.
    pub fn set_state(&self, state: State) {
        let Inner::Animated {
            picture,
            frames,
            anim,
        } = &self.inner
        else {
            return;
        };
        let changed = anim.borrow_mut().enter(state, &self.timing);
        if changed {
            render(picture, frames, &anim.borrow());
        }
    }

    /// Reset the idle countdown (a click or drag counts as activity).
    pub fn wake(&self) {
        let Inner::Animated {
            picture,
            frames,
            anim,
        } = &self.inner
        else {
            return;
        };
        let was_sleeping = anim.borrow_mut().wake(&self.timing);
        if was_sleeping {
            render(picture, frames, &anim.borrow());
        }
    }
}

/// Build the always-visible avatar button. A `[sprite] frames` list of image
/// files cycles continuously; otherwise the embedded `frog_idle` sheet is the
/// base and the sleeping/thinking/speaking sheets overlay it. Falls back to the
/// configured emoji only if the idle sheet cannot be decoded.
pub fn avatar_button(config: &FrogConfig) -> Avatar {
    let button = gtk::Button::new();
    button.add_css_class("frog-avatar");
    button.set_tooltip_text(Some("Ask Kaeru 🐸"));
    button.set_size_request(AVATAR_SIZE, AVATAR_SIZE);
    let timing = Timing::from_fps(config.sprite.fps);

    if !config.sprite.frames.is_empty() {
        let picture = picture_widget();
        if let Some(first) = config.sprite.frames.first() {
            picture.set_filename(Some(first));
        }
        button.set_child(Some(&picture));
        let source = cycle_files(picture, config.sprite.frames.clone(), timing.frame_ms);
        return Avatar {
            button,
            timing,
            inner: Inner::Custom,
            _source: Some(source),
        };
    }

    if let Some(frames) = Frames::decode() {
        let picture = picture_widget();
        button.set_child(Some(&picture));
        let frames = Rc::new(frames);
        let anim = Rc::new(RefCell::new(Anim::new(&timing)));
        render(&picture, &frames, &anim.borrow());
        let source = spawn_tick(&picture, &frames, &anim, timing);
        return Avatar {
            button,
            timing,
            inner: Inner::Animated {
                picture,
                frames,
                anim,
            },
            _source: Some(source),
        };
    }

    let label = gtk::Label::new(Some(&config.avatar));
    button.set_child(Some(&label));
    Avatar {
        button,
        timing,
        inner: Inner::Emoji,
        _source: None,
    }
}

/// The picture widget used for both the sheet and the frame-file seam.
fn picture_widget() -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    // Never scale the sprite *up* to the panel width: a `Contain` fit reports a
    // huge height-for-width at wide sizes, which inflates the avatar's box and
    // leaves a gap between the panel and the frog. The frames are already
    // pre-scaled to ART_SIZE, so `ScaleDown` shows them 1:1 (and shrinks only if
    // the avatar is ever smaller).
    picture.set_content_fit(gtk::ContentFit::ScaleDown);
    picture.set_size_request(ART_SIZE, ART_SIZE);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);
    picture
}

/// All decoded frames, pre-scaled to [`ART_SIZE`].
struct Frames {
    idle: Vec<Pixbuf>,
    sleeping: Vec<Pixbuf>,
    speaking: Vec<Pixbuf>,
    thinking: Vec<Pixbuf>,
}

impl Frames {
    /// Decode the four embedded sheets. `None` if the idle base is unreadable;
    /// an unreadable overlay is simply treated as absent.
    fn decode() -> Option<Self> {
        Some(Frames {
            idle: decode_sheet(IDLE_SHEET)?,
            sleeping: decode_sheet(SLEEPING_SHEET).unwrap_or_default(),
            speaking: decode_sheet(SPEAKING_SHEET).unwrap_or_default(),
            thinking: decode_sheet(THINKING_SHEET).unwrap_or_default(),
        })
    }
}

/// Decode a horizontal strip into per-frame pixbufs, scaled nearest-neighbour.
fn decode_sheet(bytes: &'static [u8]) -> Option<Vec<Pixbuf>> {
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(bytes));
    let sheet = Pixbuf::from_stream(&stream, gtk::gio::Cancellable::NONE).ok()?;
    let (side, count) = strip_geometry(sheet.width(), sheet.height())?;
    let mut frames = Vec::with_capacity(count as usize);
    for i in 0..count {
        let frame = sheet.new_subpixbuf(i * side, 0, side, side);
        frames.push(frame.scale_simple(ART_SIZE, ART_SIZE, InterpType::Nearest)?);
    }
    Some(frames)
}

/// Frame side and count for a horizontal strip of square frames, or `None` if
/// the image is not a positive, evenly divisible square-frame strip.
fn strip_geometry(width: i32, height: i32) -> Option<(i32, i32)> {
    if height <= 0 || width < height || width % height != 0 {
        return None;
    }
    Some((height, width / height))
}

/// Tick counts derived from `[sprite] fps` and the animation durations.
#[derive(Debug, Clone, Copy)]
struct Timing {
    frame_ms: u64,
    /// Idle ticks to rest between blinks.
    blink_rest: u32,
    /// Idle ticks per sleeping frame.
    sleep_frame: u32,
    sleep_min: u32,
    sleep_max: u32,
}

impl Timing {
    fn from_fps(fps: u32) -> Self {
        let frame_ms = (1000 / u64::from(fps.max(1))).max(1);
        let ticks = |duration: Duration| (duration.as_millis() as u64 / frame_ms).max(1) as u32;
        Timing {
            frame_ms,
            blink_rest: ticks(BLINK_REST),
            sleep_frame: ticks(SLEEP_FRAME),
            sleep_min: ticks(SLEEP_AFTER.0),
            sleep_max: ticks(SLEEP_AFTER.1),
        }
    }
}

/// The animation state machine (no GTK), so it can be unit-tested.
struct Anim {
    state: State,
    base_index: usize,
    base_rest: u32,
    overlay_index: usize,
    overlay_wait: u32,
    idle_ticks: u32,
    sleep_after: u32,
}

impl Anim {
    fn new(timing: &Timing) -> Self {
        Anim {
            state: State::Idle,
            base_index: 0,
            base_rest: timing.blink_rest,
            overlay_index: 0,
            overlay_wait: 0,
            idle_ticks: 0,
            sleep_after: random_sleep_ticks(timing),
        }
    }

    /// Switch to `state`; returns whether the visible frame changed.
    fn enter(&mut self, state: State, timing: &Timing) -> bool {
        let changed = self.state != state;
        if changed {
            self.state = state;
            self.overlay_index = 0;
            self.overlay_wait = match state {
                State::Sleeping => timing.sleep_frame.saturating_sub(1),
                _ => 0,
            };
        }
        if state == State::Idle {
            self.idle_ticks = 0;
            self.sleep_after = random_sleep_ticks(timing);
        }
        changed
    }

    /// Reset the idle countdown; returns whether the frog was asleep.
    fn wake(&mut self, timing: &Timing) -> bool {
        let was_sleeping = self.state == State::Sleeping;
        if was_sleeping {
            self.state = State::Idle;
            self.overlay_index = 0;
            self.overlay_wait = 0;
        }
        self.idle_ticks = 0;
        self.sleep_after = random_sleep_ticks(timing);
        was_sleeping
    }

    /// Advance one tick; returns whether the visible frame changed.
    fn advance(&mut self, timing: &Timing, idle_len: usize, overlay_len: usize) -> bool {
        let mut dirty = false;

        // Idle base: rest on frame 0, then play the rest of the strip as a blink.
        if idle_len > 1 {
            if self.base_rest > 0 {
                self.base_rest -= 1;
            } else {
                self.base_index = (self.base_index + 1) % idle_len;
                if self.base_index == 0 {
                    self.base_rest = timing.blink_rest;
                }
                dirty = true;
            }
        }

        // Overlay: thinking/speaking at `fps`, sleeping one frame every 5s.
        match self.state {
            State::Thinking | State::Speaking => {
                if overlay_len > 1 {
                    self.overlay_index = (self.overlay_index + 1) % overlay_len;
                    dirty = true;
                }
            }
            State::Sleeping => {
                if overlay_len > 1 {
                    if self.overlay_wait > 0 {
                        self.overlay_wait -= 1;
                    } else {
                        self.overlay_index = (self.overlay_index + 1) % overlay_len;
                        self.overlay_wait = timing.sleep_frame.saturating_sub(1);
                        dirty = true;
                    }
                }
            }
            State::Idle => {}
        }

        // Doze off after a while with nothing to do.
        if self.state == State::Idle {
            self.idle_ticks += 1;
            if self.idle_ticks >= self.sleep_after {
                self.enter(State::Sleeping, timing);
                dirty = true;
            }
        }

        dirty
    }
}

/// A random idle delay, in ticks, in the configured range.
fn random_sleep_ticks(timing: &Timing) -> u32 {
    let (min, max) = (timing.sleep_min, timing.sleep_max);
    if max <= min {
        return min.max(1);
    }
    glib::random_int_range(min as i32, max as i32).max(1) as u32
}

/// The overlay sheet for a state (`idle` has none).
fn overlay_sheet(frames: &Frames, state: State) -> &[Pixbuf] {
    match state {
        State::Idle => &[],
        State::Sleeping => &frames.sleeping,
        State::Speaking => &frames.speaking,
        State::Thinking => &frames.thinking,
    }
}

/// Compose the current base + overlay frame onto the picture.
fn render(picture: &gtk::Picture, frames: &Frames, anim: &Anim) {
    let Some(base) = frames.idle.get(anim.base_index) else {
        return;
    };
    let Some(image) = base.copy() else {
        return;
    };
    let sheet = overlay_sheet(frames, anim.state);
    if !sheet.is_empty() {
        let overlay = &sheet[anim.overlay_index % sheet.len()];
        overlay.composite(
            &image,
            0,
            0,
            ART_SIZE,
            ART_SIZE,
            0.0,
            0.0,
            1.0,
            1.0,
            InterpType::Nearest,
            255,
        );
    }
    picture.set_paintable(Some(&gdk::Texture::for_pixbuf(&image)));
}

/// The periodic animation timer.
fn spawn_tick(
    picture: &gtk::Picture,
    frames: &Rc<Frames>,
    anim: &Rc<RefCell<Anim>>,
    timing: Timing,
) -> glib::SourceId {
    let picture = picture.clone();
    let frames = Rc::clone(frames);
    let anim = Rc::clone(anim);
    glib::timeout_add_local(Duration::from_millis(timing.frame_ms), move || {
        let dirty = {
            let mut anim = anim.borrow_mut();
            let overlay_len = overlay_sheet(&frames, anim.state).len();
            anim.advance(&timing, frames.idle.len(), overlay_len)
        };
        if dirty {
            render(&picture, &frames, &anim.borrow());
        }
        glib::ControlFlow::Continue
    })
}

/// Cycle a user-provided list of frame files (the `[sprite] frames` seam).
fn cycle_files(picture: gtk::Picture, frames: Vec<PathBuf>, frame_ms: u64) -> glib::SourceId {
    let index = Rc::new(RefCell::new(0usize));
    glib::timeout_add_local(Duration::from_millis(frame_ms), move || {
        let mut index = index.borrow_mut();
        *index = (*index + 1) % frames.len();
        picture.set_filename(Some(&frames[*index]));
        glib::ControlFlow::Continue
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing {
            frame_ms: 250,
            blink_rest: 20,
            sleep_frame: 20,
            sleep_min: 4,
            sleep_max: 8,
        }
    }

    fn anim() -> Anim {
        Anim {
            state: State::Idle,
            base_index: 0,
            base_rest: 20,
            overlay_index: 0,
            overlay_wait: 0,
            idle_ticks: 0,
            sleep_after: 3,
        }
    }

    #[test]
    fn strip_geometry_slices_a_square_frame_sheet() {
        // The bundled sheets: 7 frames of 20px.
        assert_eq!(strip_geometry(140, 20), Some((20, 7)));
        assert_eq!(strip_geometry(20, 20), Some((20, 1)));
        // Reject non-square or uneven strips.
        assert_eq!(strip_geometry(141, 20), None);
        assert_eq!(strip_geometry(20, 21), None);
        assert_eq!(strip_geometry(0, 20), None);
    }

    #[test]
    fn idle_dozes_off_after_the_delay_and_wakes_on_activity() {
        let t = timing();
        let mut a = anim();
        // Two idle ticks: still awake (sleep_after = 3).
        assert!(!a.advance(&t, 7, 7));
        assert!(!a.advance(&t, 7, 7));
        assert_eq!(a.state, State::Idle);
        // Third tick: asleep.
        assert!(a.advance(&t, 7, 7));
        assert_eq!(a.state, State::Sleeping);
        // Activity wakes it back to idle and restarts the countdown.
        assert!(a.wake(&t));
        assert_eq!(a.state, State::Idle);
        assert_eq!(a.idle_ticks, 0);
        // Entering the same state is a no-op (idempotent stream calls).
        assert!(!a.enter(State::Idle, &t));
    }

    #[test]
    fn thinking_overlay_advances_every_tick_but_sleeping_is_slow() {
        let t = timing();
        let mut a = anim();
        a.enter(State::Thinking, &t);
        assert!(a.advance(&t, 7, 7));
        assert_eq!(a.overlay_index, 1);

        a.enter(State::Sleeping, &t);
        // The first frames hold for sleep_frame ticks before advancing.
        for _ in 0..t.sleep_frame - 1 {
            assert!(!a.advance(&t, 7, 7));
        }
        assert_eq!(a.overlay_index, 0);
        assert!(a.advance(&t, 7, 7));
        assert_eq!(a.overlay_index, 1);
    }
}
