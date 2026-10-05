//! The avatar seam (M6): today a 🐸 emoji; drop real sprite frames into
//! `[sprite] frames = [...]` and they animate instead. Keeping this behind one
//! function means swapping in proper art later touches nothing else.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

use crate::config::FrogConfig;

/// Build the always-visible avatar button. When `[sprite] frames` is set the
/// frames cycle at `fps`; otherwise the configured emoji is shown.
pub fn avatar_button(config: &FrogConfig) -> (gtk::Button, Option<glib::SourceId>) {
    let button = gtk::Button::new();
    button.add_css_class("frog-avatar");
    button.set_tooltip_text(Some("Ask Kaeru 🐸"));

    if config.sprite.frames.is_empty() {
        let label = gtk::Label::new(Some(&config.avatar));
        button.set_child(Some(&label));
        return (button, None);
    }

    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    if let Some(first) = config.sprite.frames.first() {
        picture.set_filename(Some(first));
    }
    button.set_child(Some(&picture));

    if config.sprite.frames.len() < 2 {
        return (button, None);
    }

    let frames = config.sprite.frames.clone();
    let fps = config.sprite.fps.max(1);
    let index = Rc::new(RefCell::new(0usize));
    let picture = picture.clone();
    let source = glib::timeout_add_local(Duration::from_millis(1000 / u64::from(fps)), move || {
        let mut index = index.borrow_mut();
        *index = (*index + 1) % frames.len();
        picture.set_filename(Some(&frames[*index]));
        glib::ControlFlow::Continue
    });
    (button, Some(source))
}
