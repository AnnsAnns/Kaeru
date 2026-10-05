//! The GTK4 widget (M6, ADR-031): an always-on-top layer-shell surface with a
//! 🐸 avatar that expands into a small chat panel. It is a pure client of the
//! daemon's `/api/*` wire API — all the logic lives in [`crate::client`].
//!
//! Threading: GTK objects are not `Send`, so every widget lives on the main
//! thread. Network work runs on a background tokio runtime and reports back
//! over an `async_channel`, which the GLib main context drains.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell as _};

use crate::client::{ClientError, DaemonClient, Event, Usage};
use crate::config::FrogConfig;
use crate::sprite;
use crate::theme::{self, Theme};

/// Messages from the network task to the main thread.
enum UiMsg {
    Connected(String),
    ConnectError(String),
    Delta(String),
    Reasoning(String),
    Tool { name: String, is_error: bool },
    Artifact(String),
    Approval { id: String, summary: String },
    Done(Option<Usage>),
    Failed(String),
    Aborted,
    Unauthorized,
}

/// The in-flight assistant card.
struct Turn {
    card: gtk::Box,
    title: gtk::Label,
    body: gtk::Label,
    thinking: gtk::Expander,
    thinking_body: gtk::Label,
    text: String,
    reasoning: String,
}

struct ChatUi {
    panel: gtk::Box,
    messages: gtk::Box,
    scroll: gtk::ScrolledWindow,
    entry: gtk::Entry,
    send: gtk::Button,
    stop: gtk::Button,
    status: gtk::Label,
    avatar: gtk::Button,
    collapse: gtk::Button,
    theme_button: gtk::Button,
    retry: gtk::Button,
    css: gtk::CssProvider,
    client: Arc<DaemonClient>,
    runtime: tokio::runtime::Handle,
    tx: async_channel::Sender<UiMsg>,
    config: Rc<RefCell<FrogConfig>>,
    config_path: PathBuf,
    window: gtk::ApplicationWindow,
    /// The two anchored edges and their current margins (drag position).
    edge_h: Edge,
    edge_v: Edge,
    margins: (i32, i32),
    /// Monitor size in logical pixels, once known (clamps the drag).
    monitor: (i32, i32),
    /// Set when a drag moved the frog, so the release does not also expand it.
    suppress_click: bool,
    /// Set while a drag actually moved, to persist the position once on release.
    dragged: bool,
    thread: Option<String>,
    streaming: bool,
    connecting: bool,
    turn: Option<Turn>,
    _sprite: Option<gtk::glib::SourceId>,
    /// Keeps the channel-drain future attached for the process lifetime.
    _channel: Option<gtk::glib::SourceId>,
}

/// Build the window and start talking to the daemon. Called from
/// `Application::activate`.
pub fn build(
    app: &gtk::Application,
    client: Arc<DaemonClient>,
    runtime: tokio::runtime::Handle,
    config: FrogConfig,
    config_path: PathBuf,
    use_layer_shell: bool,
    start_expanded: bool,
) {
    let window = gtk::ApplicationWindow::builder().application(app).build();
    window.set_decorated(false);
    window.set_title(Some("Kaeru 🐸"));

    let (edge_h, edge_v) = corner_edges(&config.corner);
    if use_layer_shell && layer_shell_supported() {
        configure_layer_shell(&window, &config, (edge_h, edge_v));
    } else if use_layer_shell {
        tracing::warn!("compositor has no wlr-layer-shell; using a normal window");
    }

    // ---- panel ----
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    panel.add_css_class("frog-panel");
    panel.set_size_request(340, -1);
    panel.set_visible(start_expanded);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    let title = gtk::Label::new(Some("Kaeru"));
    title.add_css_class("panel-title");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let retry = icon_button("↻", "Reconnect");
    let theme_button = icon_button("🎨", "Cycle theme");
    let collapse_button = icon_button("➖", "Collapse");
    header.append(&title);
    header.append(&spacer);
    header.append(&retry);
    header.append(&theme_button);
    header.append(&collapse_button);

    let status = gtk::Label::new(Some("connecting…"));
    status.add_css_class("status");
    status.set_xalign(0.0);
    status.set_wrap(true);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    scroll.add_css_class("frog-scroll");
    scroll.set_has_frame(false);
    scroll.set_min_content_height(220);
    scroll.set_max_content_height(420);
    let messages = gtk::Box::new(gtk::Orientation::Vertical, 10);
    messages.add_css_class("frog-messages");
    messages.set_margin_top(8);
    messages.set_margin_bottom(8);
    messages.set_margin_start(4);
    messages.set_margin_end(4);
    scroll.set_child(Some(&messages));

    let entry = gtk::Entry::new();
    entry.add_css_class("frog-entry");
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("ask the frog…"));
    let send = gtk::Button::with_label("Ponder");
    send.add_css_class("action");
    let stop = gtk::Button::with_label("stop");
    stop.add_css_class("action");
    stop.add_css_class("stop");
    stop.set_visible(false);
    let input_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    input_row.append(&entry);
    input_row.append(&send);
    input_row.append(&stop);

    panel.append(&header);
    panel.append(&status);
    panel.append(&scroll);
    panel.append(&input_row);

    // ---- avatar + root ----
    let (avatar, sprite_source) = sprite::avatar_button(&config);
    avatar.set_size_request(52, 52);
    avatar.set_halign(gtk::Align::End);
    avatar.set_valign(gtk::Align::End);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("frog-root");
    root.append(&panel);
    root.append(&avatar);
    window.set_child(Some(&root));

    // ---- theme ----
    theme::register_fonts();
    let css = gtk::CssProvider::new();
    let theme = Theme::by_id(&config.theme);
    css.load_from_string(&theme::stylesheet(theme));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let (tx, rx) = async_channel::unbounded::<UiMsg>();
    let margins = (config.position.h, config.position.v);
    let ui = Rc::new(RefCell::new(ChatUi {
        panel,
        messages,
        scroll,
        entry,
        send,
        stop,
        status,
        avatar,
        collapse: collapse_button,
        theme_button,
        retry,
        css,
        client,
        runtime,
        tx,
        config: Rc::new(RefCell::new(config)),
        config_path,
        window: window.clone(),
        edge_h,
        edge_v,
        margins,
        monitor: (0, 0),
        suppress_click: false,
        dragged: false,
        thread: None,
        streaming: false,
        connecting: false,
        turn: None,
        _sprite: sprite_source,
        _channel: None,
    }));

    connect(&ui);

    let channel_ui = Rc::clone(&ui);
    let channel = gtk::glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            handle_msg(&channel_ui, msg);
        }
    })
    .into_source_id()
    .ok();
    ui.borrow_mut()._channel = channel;

    // Escape collapses the panel back to the bare frog.
    let key = gtk::EventControllerKey::new();
    let key_ui = Rc::clone(&ui);
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk::gdk::Key::Escape {
            collapse(&key_ui);
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    window.present();
    install_drag(&ui);
    if start_expanded {
        ui.borrow().entry.grab_focus();
    }

    // Learn the monitor size (for drag clamping) once the surface is mapped,
    // and pull the persisted position back on screen if the layout changed.
    let monitor_ui = Rc::clone(&ui);
    gtk::glib::idle_add_local_once(move || {
        let geometry = {
            let u = monitor_ui.borrow();
            u.window
                .surface()
                .and_then(|surface| surface.display().monitor_at_surface(&surface))
                .map(|monitor| monitor.geometry())
        };
        let mut u = monitor_ui.borrow_mut();
        if let Some(geometry) = geometry {
            u.monitor = (geometry.width(), geometry.height());
        }
        let (width, height) = u.monitor;
        let (mut margin_h, mut margin_v) = u.margins;
        if width > 0 {
            margin_h = margin_h.clamp(0, (width - u.window.width()).max(0));
        }
        if height > 0 {
            margin_v = margin_v.clamp(0, (height - u.window.height()).max(0));
        }
        if (margin_h, margin_v) != u.margins {
            u.margins = (margin_h, margin_v);
            u.window.set_margin(u.edge_h, margin_h);
            u.window.set_margin(u.edge_v, margin_v);
        }
    });

    // No thread yet: keep input disabled so an early Enter cannot drop a
    // message. The first `Connected` re-enables it.
    set_streaming(&ui, false);
    reconnect(&ui);
}

fn icon_button(label: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("icon-btn");
    button.set_tooltip_text(Some(tooltip));
    button
}

/// Is the layer-shell protocol available? `is_supported()` asserts a Wayland
/// display, so only ask it on one (X11/testing falls back to a normal window).
fn layer_shell_supported() -> bool {
    let wayland = gtk::gdk::Display::default()
        .map(|display| display.type_().name().contains("Wayland"))
        .unwrap_or(false);
    wayland && gtk4_layer_shell::is_supported()
}

/// Anchor the surface to the configured corner. `OnDemand` keyboard mode lets
/// the entry take focus on click without stealing it while collapsed.
fn configure_layer_shell(
    window: &gtk::ApplicationWindow,
    config: &FrogConfig,
    (edge_h, edge_v): (Edge, Edge),
) {
    window.init_layer_shell();
    window.set_namespace(Some("kaeru-frog"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window.set_exclusive_zone(0);
    window.set_anchor(edge_h, true);
    window.set_anchor(edge_v, true);
    window.set_margin(edge_h, config.position.h);
    window.set_margin(edge_v, config.position.v);
}

/// Signed delta for a drag: dragging right/down must move the window with the
/// pointer, whichever edges it is anchored to.
fn edge_sign(edge: Edge) -> i32 {
    match edge {
        Edge::Left | Edge::Top => 1,
        _ => -1,
    }
}

/// New margin for one axis: `start` plus the signed drag offset, clamped so the
/// window stays on the monitor (when its size is known) and never goes past the
/// anchored edge.
fn drag_margin(start: i32, offset: f64, sign: i32, monitor: i32, window: i32) -> i32 {
    let raw = start + sign * offset.round() as i32;
    if monitor > 0 {
        raw.clamp(0, (monitor - window).max(0))
    } else {
        raw.max(0)
    }
}

/// Hold-and-drag the frog around its corner. Layer surfaces cannot be moved by
/// the client, so a drag just updates the two anchored margins (and the choice
/// is persisted to the config on release).
fn install_drag(ui: &Rc<RefCell<ChatUi>>) {
    let drag = gtk::GestureDrag::new();
    let start = Rc::new(std::cell::Cell::new((0i32, 0i32)));

    {
        let ui = Rc::clone(ui);
        let start = Rc::clone(&start);
        drag.connect_drag_begin(move |_, _, _| {
            let mut u = ui.borrow_mut();
            start.set(u.margins);
            u.suppress_click = false;
            u.dragged = false;
        });
    }
    {
        let ui = Rc::clone(ui);
        let start = Rc::clone(&start);
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            let (sign_h, sign_v, monitor) = {
                let u = ui.borrow();
                (edge_sign(u.edge_h), edge_sign(u.edge_v), u.monitor)
            };
            let (start_h, start_v) = start.get();
            let mut u = ui.borrow_mut();
            let margin_h = drag_margin(start_h, offset_x, sign_h, monitor.0, u.window.width());
            let margin_v = drag_margin(start_v, offset_y, sign_v, monitor.1, u.window.height());
            if (margin_h, margin_v) != u.margins {
                u.suppress_click = true;
                u.dragged = true;
                u.margins = (margin_h, margin_v);
                u.window.set_margin(u.edge_h, margin_h);
                u.window.set_margin(u.edge_v, margin_v);
            }
        });
    }
    {
        let ui = Rc::clone(ui);
        drag.connect_drag_end(move |_, _, _| {
            let moved = ui.borrow().dragged;
            if moved {
                persist_position(&ui);
                // Clear the click guard on the next loop turn: if GTK denied
                // the release click it would otherwise swallow the next one.
                let ui = Rc::clone(&ui);
                gtk::glib::idle_add_local_once(move || {
                    ui.borrow_mut().suppress_click = false;
                });
            }
        });
    }

    let avatar = ui.borrow().avatar.clone();
    avatar.add_controller(drag);
}

fn persist_position(ui: &Rc<RefCell<ChatUi>>) {
    let u = ui.borrow();
    let mut config = u.config.borrow_mut();
    config.position.h = u.margins.0;
    config.position.v = u.margins.1;
    if let Err(err) = config.save(&u.config_path) {
        tracing::warn!("cannot save frog config: {err}");
    }
}

fn corner_edges(corner: &str) -> (Edge, Edge) {
    match corner {
        "bottom-left" => (Edge::Left, Edge::Bottom),
        "top-right" => (Edge::Right, Edge::Top),
        "top-left" => (Edge::Left, Edge::Top),
        _ => (Edge::Right, Edge::Bottom),
    }
}

/* ---------- widget helpers ---------- */

struct BoxView {
    card: gtk::Box,
    title: gtk::Label,
    body: gtk::Label,
}

fn add_box(ui: &Rc<RefCell<ChatUi>>, role: &str, title: &str) -> BoxView {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("card");
    card.add_css_class(role);
    card.set_halign(gtk::Align::Start);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("card-title");
    title_label.set_xalign(0.0);
    title_label.set_hexpand(true);
    let body = gtk::Label::new(None);
    body.add_css_class("card-body");
    body.set_wrap(true);
    body.set_xalign(0.0);
    body.set_selectable(true);
    card.append(&title_label);
    card.append(&body);
    ui.borrow().messages.append(&card);
    BoxView {
        card,
        title: title_label,
        body,
    }
}

fn add_line(ui: &Rc<RefCell<ChatUi>>, css_class: &str, text: &str) {
    let label = gtk::Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    ui.borrow().messages.append(&label);
    scroll_to_bottom(ui);
}

fn scroll_to_bottom(ui: &Rc<RefCell<ChatUi>>) {
    let adjustment = ui.borrow().scroll.vadjustment();
    gtk::glib::idle_add_local_once(move || adjustment.set_value(adjustment.upper()));
}

/* ---------- signals ---------- */

fn connect(ui: &Rc<RefCell<ChatUi>>) {
    let avatar = ui.borrow().avatar.clone();
    let ui_avatar = Rc::clone(ui);
    avatar.connect_clicked(move |_| {
        let suppress = {
            let mut u = ui_avatar.borrow_mut();
            let suppress = u.suppress_click;
            u.suppress_click = false;
            suppress
        };
        if !suppress {
            toggle_panel(&ui_avatar);
        }
    });

    let collapse_btn = ui.borrow().collapse.clone();
    let ui_collapse = Rc::clone(ui);
    collapse_btn.connect_clicked(move |_| collapse(&ui_collapse));

    let theme_button = ui.borrow().theme_button.clone();
    let ui_theme = Rc::clone(ui);
    theme_button.connect_clicked(move |_| cycle_theme(&ui_theme));

    let retry = ui.borrow().retry.clone();
    let ui_retry = Rc::clone(ui);
    retry.connect_clicked(move |_| reconnect(&ui_retry));

    let send = ui.borrow().send.clone();
    let ui_send = Rc::clone(ui);
    send.connect_clicked(move |_| start_turn(&ui_send));

    let entry = ui.borrow().entry.clone();
    let ui_entry = Rc::clone(ui);
    entry.connect_activate(move |_| start_turn(&ui_entry));

    let stop = ui.borrow().stop.clone();
    let ui_stop = Rc::clone(ui);
    stop.connect_clicked(move |_| stop_turn(&ui_stop));
}

fn toggle_panel(ui: &Rc<RefCell<ChatUi>>) {
    if ui.borrow().panel.is_visible() {
        collapse(ui);
    } else {
        let panel = ui.borrow().panel.clone();
        let entry = ui.borrow().entry.clone();
        panel.set_visible(true);
        entry.grab_focus();
    }
}

fn collapse(ui: &Rc<RefCell<ChatUi>>) {
    ui.borrow().panel.set_visible(false);
}

/* ---------- connection ---------- */

fn reconnect(ui: &Rc<RefCell<ChatUi>>) {
    let (client, runtime, tx, configured) = {
        let u = ui.borrow();
        if u.connecting {
            return;
        }
        (
            Arc::clone(&u.client),
            u.runtime.clone(),
            u.tx.clone(),
            u.config.borrow().thread.clone(),
        )
    };
    ui.borrow_mut().connecting = true;
    set_status(ui, &format!("connecting to {}…", client.base()));
    runtime.spawn(async move {
        let configured = configured.trim();
        let configured = (!configured.is_empty()).then_some(configured);
        let result = match client.resolve_thread(configured).await {
            Ok(thread) => UiMsg::Connected(thread),
            Err(ClientError::Unauthorized) => UiMsg::Unauthorized,
            Err(err) => UiMsg::ConnectError(err.to_string()),
        };
        let _ = tx.send(result).await;
    });
}

/* ---------- turns ---------- */

fn start_turn(ui: &Rc<RefCell<ChatUi>>) {
    let (thread, streaming, client, runtime, tx, message) = {
        let u = ui.borrow();
        (
            u.thread.clone(),
            u.streaming,
            Arc::clone(&u.client),
            u.runtime.clone(),
            u.tx.clone(),
            u.entry.text().trim().to_string(),
        )
    };
    let Some(thread) = thread else {
        return;
    };
    if streaming || message.is_empty() {
        return;
    }
    ui.borrow().entry.set_text("");
    begin_turn(ui, &message);
    runtime.spawn(async move {
        let mut stream = match client.send(&thread, &message).await {
            Ok(stream) => stream,
            Err(ClientError::Unauthorized) => {
                let _ = tx.send(UiMsg::Unauthorized).await;
                return;
            }
            Err(err) => {
                let _ = tx.send(UiMsg::Failed(err.to_string())).await;
                return;
            }
        };
        while let Some(event) = stream.next().await {
            let message = match event {
                Ok(Event::Delta { text }) => UiMsg::Delta(text),
                Ok(Event::Reasoning { text }) => UiMsg::Reasoning(text),
                Ok(Event::ToolCall { name, .. }) => UiMsg::Tool {
                    name,
                    is_error: false,
                },
                Ok(Event::ToolResult { name, is_error, .. }) => UiMsg::Tool { name, is_error },
                Ok(Event::Artifact { path, .. }) => UiMsg::Artifact(path),
                Ok(Event::ApprovalRequest { id, summary, .. }) => UiMsg::Approval { id, summary },
                Ok(Event::TurnDone { usage }) => {
                    let _ = tx.send(UiMsg::Done(usage)).await;
                    return;
                }
                Ok(Event::Error { kind, message }) => {
                    let message = if kind == "aborted" {
                        UiMsg::Aborted
                    } else {
                        UiMsg::Failed(message)
                    };
                    let _ = tx.send(message).await;
                    return;
                }
                Ok(Event::Unknown) => continue,
                Err(err) => {
                    let _ = tx.send(UiMsg::Failed(err.to_string())).await;
                    return;
                }
            };
            if tx.send(message).await.is_err() {
                return;
            }
        }
        let _ = tx
            .send(UiMsg::Failed("the stream ended unexpectedly".into()))
            .await;
    });
}

fn begin_turn(ui: &Rc<RefCell<ChatUi>>, message: &str) {
    let you = add_box(ui, "you", "you");
    you.card.set_halign(gtk::Align::End);
    you.body.set_text(message);

    let assistant = add_box(ui, "kaeru", "kaeru");
    let thinking = gtk::Expander::new(Some("thinking"));
    thinking.add_css_class("reasoning");
    thinking.set_visible(false);
    let thinking_body = gtk::Label::new(None);
    thinking_body.add_css_class("reasoning");
    thinking_body.set_wrap(true);
    thinking_body.set_xalign(0.0);
    thinking_body.set_selectable(true);
    thinking.set_child(Some(&thinking_body));
    assistant
        .card
        .insert_child_after(&thinking, Some(&assistant.title));

    ui.borrow_mut().turn = Some(Turn {
        card: assistant.card,
        title: assistant.title,
        body: assistant.body,
        thinking,
        thinking_body,
        text: String::new(),
        reasoning: String::new(),
    });
    set_streaming(ui, true);
    set_status(ui, "thinking…");
    scroll_to_bottom(ui);
}

fn stop_turn(ui: &Rc<RefCell<ChatUi>>) {
    let (client, runtime, thread) = {
        let u = ui.borrow();
        (Arc::clone(&u.client), u.runtime.clone(), u.thread.clone())
    };
    let Some(thread) = thread else {
        return;
    };
    runtime.spawn(async move {
        let _ = client.abort(&thread).await;
    });
    set_status(ui, "stopping…");
}

/* ---------- message handling (main thread) ---------- */

fn handle_msg(ui: &Rc<RefCell<ChatUi>>, msg: UiMsg) {
    match msg {
        UiMsg::Connected(thread) => {
            ui.borrow_mut().thread = Some(thread);
            ui.borrow_mut().connecting = false;
            set_streaming(ui, false);
            let base = ui.borrow().client.base().to_owned();
            set_status(ui, &format!("ready · {base}"));
        }
        UiMsg::ConnectError(err) => {
            ui.borrow_mut().connecting = false;
            set_streaming(ui, false);
            let base = ui.borrow().client.base().to_owned();
            set_status(ui, &format!("cannot reach {base}"));
            error_card(ui, &err, true);
        }
        UiMsg::Unauthorized => {
            ui.borrow_mut().connecting = false;
            if let Some(turn) = ui.borrow_mut().turn.take() {
                turn.card.unparent();
            }
            set_streaming(ui, false);
            set_status(ui, "auth required");
            token_prompt(ui);
        }
        UiMsg::Delta(text) => {
            if let Some(turn) = ui.borrow_mut().turn.as_mut() {
                turn.text.push_str(&text);
                turn.body.set_text(&turn.text);
            }
            scroll_to_bottom(ui);
        }
        UiMsg::Reasoning(text) => {
            if let Some(turn) = ui.borrow_mut().turn.as_mut() {
                turn.reasoning.push_str(&text);
                turn.thinking_body.set_text(&turn.reasoning);
                turn.thinking.set_visible(true);
            }
            scroll_to_bottom(ui);
        }
        UiMsg::Tool { name, is_error } => {
            let suffix = if is_error { " (failed)" } else { "" };
            add_line(ui, "tool-line", &format!("🔧 {name}{suffix}"));
        }
        UiMsg::Artifact(path) => add_line(ui, "tool-line", &format!("📎 {path}")),
        UiMsg::Approval { id, summary } => approval_card(ui, id, summary),
        UiMsg::Done(usage) => {
            ui.borrow_mut().turn = None;
            set_streaming(ui, false);
            let base = ui.borrow().client.base().to_owned();
            set_status(ui, &format!("ready · {base}{}", format_usage(usage)));
        }
        UiMsg::Aborted => {
            if let Some(turn) = ui.borrow_mut().turn.take() {
                turn.title.set_text("kaeru · stopped");
            }
            set_streaming(ui, false);
            set_status(ui, "stopped");
        }
        UiMsg::Failed(message) => {
            {
                let mut u = ui.borrow_mut();
                match u.turn.take() {
                    // Nothing streamed: drop the empty card.
                    Some(turn) if turn.text.is_empty() && turn.reasoning.is_empty() => {
                        turn.card.unparent();
                    }
                    // Keep the partial answer, but mark it failed.
                    Some(turn) => turn.title.set_text("kaeru · error"),
                    None => {}
                }
            }
            set_streaming(ui, false);
            set_status(ui, "error");
            error_card(ui, &message, false);
        }
    }
}

fn set_streaming(ui: &Rc<RefCell<ChatUi>>, streaming: bool) {
    let mut u = ui.borrow_mut();
    let ready = u.thread.is_some();
    u.streaming = streaming;
    u.send.set_visible(!streaming);
    u.stop.set_visible(streaming);
    u.entry.set_sensitive(!streaming && ready);
    u.send.set_sensitive(!streaming && ready);
}

fn set_status(ui: &Rc<RefCell<ChatUi>>, text: &str) {
    ui.borrow().status.set_text(text);
}

fn format_usage(usage: Option<Usage>) -> String {
    let Some(usage) = usage else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(input) = usage.input_tokens {
        parts.push(format!("{input} in"));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("{output} out"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · tokens {}", parts.join(" · "))
    }
}

fn error_card(ui: &Rc<RefCell<ChatUi>>, message: &str, retry: bool) {
    let view = add_box(ui, "error", "error");
    view.body.set_text(message);
    if retry {
        let button = gtk::Button::with_label("retry");
        button.add_css_class("action");
        let ui_retry = Rc::clone(ui);
        button.connect_clicked(move |_| reconnect(&ui_retry));
        view.card.append(&button);
    }
    scroll_to_bottom(ui);
}

fn token_prompt(ui: &Rc<RefCell<ChatUi>>) {
    let view = add_box(ui, "error", "access");
    view.body.set_text(
        "This daemon needs its shared secret (the auth_token from its config). \
         Paste it once — it is saved to your frog config (0600).",
    );
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let input = gtk::Entry::new();
    input.set_visibility(false);
    input.set_hexpand(true);
    input.add_css_class("frog-entry");
    input.set_placeholder_text(Some("auth token"));
    let save = gtk::Button::with_label("save");
    save.add_css_class("action");
    row.append(&input);
    row.append(&save);
    view.card.append(&row);

    let ui_save = Rc::clone(ui);
    let card = view.card.clone();
    save.connect_clicked(move |_| {
        let token = input.text().trim().to_string();
        if token.is_empty() {
            return;
        }
        {
            let u = ui_save.borrow();
            u.client.set_token(Some(token.clone()));
            let mut config = u.config.borrow_mut();
            config.token = token;
            if let Err(err) = config.save(&u.config_path) {
                tracing::warn!("cannot save frog config: {err}");
            }
        }
        card.unparent();
        reconnect(&ui_save);
    });
    scroll_to_bottom(ui);
}

fn approval_card(ui: &Rc<RefCell<ChatUi>>, id: String, summary: String) {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("approval");
    card.set_halign(gtk::Align::Start);
    let text = gtk::Label::new(Some(&summary));
    text.add_css_class("approval-text");
    text.set_wrap(true);
    text.set_xalign(0.0);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let allow = gtk::Button::with_label("allow");
    allow.add_css_class("action");
    let deny = gtk::Button::with_label("deny");
    deny.add_css_class("action");
    deny.add_css_class("stop");
    row.append(&allow);
    row.append(&deny);
    card.append(&text);
    card.append(&row);
    ui.borrow().messages.append(&card);

    for (button, decision) in [(&allow, true), (&deny, false)] {
        let ui_decision = Rc::clone(ui);
        let id = id.clone();
        let summary = summary.clone();
        let text = text.clone();
        let row = row.clone();
        button.connect_clicked(move |_| {
            submit_decision(&ui_decision, &id, decision, &summary, &text, &row);
        });
    }
    scroll_to_bottom(ui);
}

fn submit_decision(
    ui: &Rc<RefCell<ChatUi>>,
    id: &str,
    allow: bool,
    summary: &str,
    text: &gtk::Label,
    row: &gtk::Box,
) {
    let (client, runtime, tx, thread) = {
        let u = ui.borrow();
        (
            Arc::clone(&u.client),
            u.runtime.clone(),
            u.tx.clone(),
            u.thread.clone().unwrap_or_default(),
        )
    };
    let id = id.to_owned();
    runtime.spawn(async move {
        if let Err(err) = client.approve(&thread, &id, allow).await {
            let _ = tx.send(UiMsg::Failed(err.to_string())).await;
        }
    });
    text.set_text(&format!(
        "{}: {summary}",
        if allow { "allowed" } else { "denied" }
    ));
    row.set_visible(false);
}

fn cycle_theme(ui: &Rc<RefCell<ChatUi>>) {
    let (next, config_path, config) = {
        let u = ui.borrow();
        (
            Theme::next_id(&u.config.borrow().theme).to_owned(),
            u.config_path.clone(),
            Rc::clone(&u.config),
        )
    };
    let u = ui.borrow();
    config.borrow_mut().theme = next.clone();
    u.css
        .load_from_string(&theme::stylesheet(Theme::by_id(&next)));
    if let Err(err) = config.borrow().save(&config_path) {
        tracing::warn!("cannot save frog config: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_signs_point_with_the_pointer() {
        assert_eq!(edge_sign(Edge::Left), 1);
        assert_eq!(edge_sign(Edge::Top), 1);
        assert_eq!(edge_sign(Edge::Right), -1);
        assert_eq!(edge_sign(Edge::Bottom), -1);
    }

    #[test]
    fn drag_margin_follows_the_pointer_and_clamps() {
        // Anchored left: dragging right grows the margin.
        assert_eq!(drag_margin(18, 100.0, 1, 1000, 100), 118);
        // Anchored right: dragging right shrinks it.
        assert_eq!(drag_margin(118, 100.0, -1, 1000, 100), 18);
        // Never negative, never past the far edge.
        assert_eq!(drag_margin(0, -50.0, 1, 1000, 100), 0);
        assert_eq!(drag_margin(18, 5000.0, 1, 1000, 100), 900);
        // No monitor size known: only the non-negative clamp.
        assert_eq!(drag_margin(18, -100.0, 1, 0, 100), 0);
        assert_eq!(drag_margin(18, 5000.0, 1, 0, 100), 5018);
    }
}
