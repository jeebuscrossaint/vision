//! Vision: display color control panel (saturation, hue, temperature,
//! brightness, contrast, gamma, RGB gains) for Wayland.
//!
//! The GUI is unprivileged. It only edits ~/.config/vision/config.json; the
//! `visiond` helper picks up every write instantly and programs the display
//! hardware, so slider drags are live.

use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Key, Layout, Margin, Mesh, Rect, Sense, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};
mod cli;

use vision::color::Field;
use vision::{Adjust, Config, Preset, State, Status};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x14);
const CARD: Color32 = Color32::from_rgb(0x17, 0x1a, 0x21);
const RAISED: Color32 = Color32::from_rgb(0x20, 0x24, 0x2d);
const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3a);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xeb, 0xf1);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x93, 0xa6);
const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x8d, 0xff);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x1d, 0x2e, 0x4f);
const OK: Color32 = Color32::from_rgb(0x3e, 0xcf, 0x8e);
const WARN: Color32 = Color32::from_rgb(0xf5, 0xa5, 0x24);
const ERR: Color32 = Color32::from_rgb(0xff, 0x5c, 0x5c);

fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let c = v * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    Color32::from_rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

fn hex(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

fn muted(t: &str) -> egui::RichText {
    egui::RichText::new(t).size(12.5).color(MUTED)
}

// ---------------------------------------------------------------------------
// Slider definitions
// ---------------------------------------------------------------------------

struct Spec {
    field: Field,
    track: fn() -> Vec<Color32>,
}

fn plain() -> Vec<Color32> {
    vec![]
}

const COLOR_SPECS: &[Spec] = &[
    Spec { field: Field::Saturation, track: || vec![hex(0x7a7a7a), hex(0xb05a8a), hex(0xff2d8a), hex(0xff0066)] },
    Spec { field: Field::Hue, track: || (0..=12).map(|i| hsv(180.0 + i as f32 * 30.0, 0.75, 0.95)).collect() },
    Spec {
        field: Field::Temperature,
        track: || {
            (0..=10)
                .map(|i| {
                    let c = vision::color::temperature_rgb(2700.0 + i as f64 * 730.0);
                    Color32::from_rgb((c[0] * 235.0) as u8, (c[1] * 235.0) as u8, (c[2] * 235.0) as u8)
                })
                .collect()
        },
    },
];

const TONE_SPECS: &[Spec] = &[
    Spec { field: Field::Brightness, track: plain },
    Spec { field: Field::Contrast, track: plain },
    Spec { field: Field::Gamma, track: plain },
];

const CHANNEL_SPECS: &[Spec] = &[
    Spec { field: Field::Red, track: || vec![hex(0x3a1010), hex(0xff4040)] },
    Spec { field: Field::Green, track: || vec![hex(0x0f3315), hex(0x40e060)] },
    Spec { field: Field::Blue, track: || vec![hex(0x101a3a), hex(0x4a7dff)] },
];

fn all_specs() -> impl Iterator<Item = &'static Spec> {
    COLOR_SPECS.iter().chain(TONE_SPECS).chain(CHANNEL_SPECS)
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn gradient(ui: &Ui, rect: Rect, colors: &[Color32]) {
    let mut mesh = Mesh::default();
    let n = colors.len();
    for (i, c) in colors.iter().enumerate() {
        let x = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
        mesh.colored_vertex(pos2(x, rect.top()), *c);
        mesh.colored_vertex(pos2(x, rect.bottom()), *c);
        if i > 0 {
            let b = (2 * i) as u32;
            mesh.add_triangle(b - 2, b - 1, b);
            mesh.add_triangle(b - 1, b, b + 1);
        }
    }
    ui.painter().add(mesh);
}

/// A value being typed into a slider's number field.
struct ValueEdit {
    label: &'static str,
    text: String,
    fresh: bool,
}

/// Drag, click, double-click (reset), scroll wheel, arrow keys (Shift = ×10)
/// and click-the-number-to-type. Returns true when the value changed.
fn slider(ui: &mut Ui, spec: &Spec, adj: &mut Adjust, edit: &mut Option<ValueEdit>) -> bool {
    let default = spec.field.default_value();
    let mut value = spec.field.get(adj);
    let mut changed = false;
    let modified = (value - default).abs() > 1e-6;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(spec.field.label()).size(14.0).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if modified {
                let r = ui
                    .add(egui::Label::new(egui::RichText::new("⟲").size(15.0).color(MUTED)).sense(Sense::click()))
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text("Reset");
                if r.clicked() {
                    value = default;
                    changed = true;
                }
            }
            match edit {
                Some(e) if e.label == spec.field.label() => {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut e.text)
                            .font(FontId::monospace(14.0))
                            .desired_width(76.0),
                    );
                    if e.fresh {
                        r.request_focus();
                        e.fresh = false;
                    }
                    if r.lost_focus() {
                        if !ui.input(|i| i.key_pressed(Key::Escape)) {
                            if let Some(v) = spec.field.parse(&e.text) {
                                value = v;
                                changed = true;
                            }
                        }
                        *edit = None;
                    }
                }
                _ => {
                    let r = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(spec.field.format(value))
                                    .size(14.0)
                                    .monospace()
                                    .color(if modified { ACCENT } else { MUTED }),
                            )
                            .sense(Sense::click()),
                        )
                        .on_hover_cursor(CursorIcon::Text)
                        .on_hover_text("Click to type a value");
                    if r.clicked() {
                        *edit = Some(ValueEdit { label: spec.field.label(), text: spec.field.format(value), fresh: true });
                    }
                }
            }
        });
    });

    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let id = egui::Id::unique(("vision-slider", spec.field.label()));
    let resp = ui.interact(rect, id, Sense::click_and_drag()).on_hover_cursor(CursorIcon::PointingHand);
    let inner = rect.shrink2(vec2(9.0, 0.0));
    let to_x = |v: f64| inner.left() + inner.width() * ((v - spec.field.min()) / (spec.field.max() - spec.field.min())) as f32;

    if resp.double_clicked() {
        value = default;
        changed = true;
    } else if resp.is_pointer_button_down_on() || resp.dragged() {
        resp.request_focus();
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - inner.left()) / inner.width()).clamp(0.0, 1.0) as f64;
            let mut v = spec.field.snap(spec.field.min() + t * (spec.field.max() - spec.field.min()));
            // Gentle snap to the default so "back to normal" is easy.
            if (to_x(v) - to_x(default)).abs() < 5.0 {
                v = default;
            }
            if (v - value).abs() > 1e-9 {
                value = v;
                changed = true;
            }
        }
    }

    // Arrow keys while focused.
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
            )
        });
        let delta = ui.input(|i| {
            let n = if i.modifiers.shift { 10.0 } else { 1.0 };
            let up = i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::ArrowUp);
            let down = i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::ArrowDown);
            (up as i32 - down as i32) as f64 * n
        });
        if delta != 0.0 {
            value = spec.field.snap(value + delta * spec.field.step());
            changed = true;
        }
    }

    // Scroll wheel while hovering the track (consumed so the page doesn't scroll).
    if resp.hovered() {
        let d = ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
        if d != 0.0 {
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
            let steps = ui.ctx().data_mut(|data| {
                let acc = data.get_temp_mut_or_default::<f32>(id);
                *acc += d;
                let s = (*acc / 24.0).trunc();
                *acc -= s * 24.0;
                s
            });
            if steps != 0.0 {
                value = spec.field.snap(value + steps as f64 * spec.field.step());
                changed = true;
            }
        }
    }

    let track = Rect::from_center_size(rect.center(), vec2(inner.width() + 6.0, 6.0));
    let colors = (spec.track)();
    if colors.is_empty() {
        ui.painter().rect_filled(track, 3.0, BORDER);
        let (a, b) = (to_x(default), to_x(value));
        ui.painter().rect_filled(Rect::from_x_y_ranges(a.min(b)..=a.max(b), track.y_range()), 3.0, ACCENT);
    } else {
        gradient(ui, track, &colors);
    }
    let dx = to_x(default);
    ui.painter().line_segment([pos2(dx, track.top() - 4.0), pos2(dx, track.bottom() + 4.0)], Stroke::new(1.5, MUTED));
    let knob = pos2(to_x(value), rect.center().y);
    let hot = resp.hovered() || resp.dragged();
    if resp.has_focus() {
        ui.painter().circle_stroke(knob, 13.0, Stroke::new(2.0, ACCENT.gamma_multiply(0.5)));
    }
    ui.painter().circle(knob, if hot { 9.0 } else { 8.0 }, TEXT, Stroke::new(3.0, ACCENT));
    ui.add_space(6.0);

    if changed {
        spec.field.set(adj, value);
    }
    changed
}

fn chip(ui: &mut Ui, text: &str, sub: Option<&str>, selected: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.5), if selected { TEXT } else { MUTED });
    let gs = sub.map(|s| ui.painter().layout_no_wrap(s.to_string(), FontId::proportional(11.0), MUTED));
    let w = g.size().x.max(gs.as_ref().map_or(0.0, |g| g.size().x)) + 26.0;
    let h = if gs.is_some() { 44.0 } else { 30.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let fill = if selected {
        ACCENT_DIM
    } else if resp.hovered() {
        RAISED
    } else {
        CARD
    };
    ui.painter().rect(rect, 8.0, fill, Stroke::new(1.0, if selected { ACCENT } else { BORDER }), StrokeKind::Inside);
    let mut y = rect.top() + if gs.is_some() { 6.0 } else { (h - g.size().y) / 2.0 };
    ui.painter().galley(pos2(rect.left() + 13.0, y), g.clone(), TEXT);
    if let Some(gs) = gs {
        y += g.size().y + 1.0;
        ui.painter().galley(pos2(rect.left() + 13.0, y), gs, MUTED);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    ui.painter().rect_filled(rect, 11.0, lerp_color(BORDER, ACCENT, t));
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    ui.painter().circle_filled(pos2(x, rect.center().y), 8.0, TEXT);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn card_with<R>(ui: &mut Ui, title: &str, stroke: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(egui::RichText::new(title.to_uppercase()).size(11.5).strong().color(MUTED));
                ui.add_space(8.0);
            }
            add(ui)
        })
        .inner
}

fn card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_with(ui, title, BORDER, add)
}

fn btn(text: &str, primary: bool) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(13.0).color(TEXT))
        .fill(if primary { ACCENT } else { RAISED })
        .corner_radius(8.0)
        .min_size(vec2(0.0, 28.0))
}

fn button(ui: &mut Ui, text: &str, primary: bool) -> egui::Response {
    ui.add(btn(text, primary)).on_hover_cursor(CursorIcon::PointingHand)
}

// ---------------------------------------------------------------------------
// Helper process, launcher entry, login service
// ---------------------------------------------------------------------------

fn visiond_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("visiond")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("visiond"))
}

fn spawn_daemon() {
    let log = fs::File::create(vision::runtime_dir().join("visiond.log")).ok();
    let mut cmd = Command::new(visiond_path());
    cmd.stdin(Stdio::null()).process_group(0);
    if let Some(l) = log {
        cmd.stdout(l.try_clone().map(Stdio::from).unwrap_or(Stdio::null())).stderr(Stdio::from(l));
    }
    if let Ok(mut child) = cmd.spawn() {
        // Reap in the background so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Put Vision in the app launcher (fuzzel, rofi, …) on first run, pointing at
/// wherever `cargo install` placed the binary. Skipped for dev builds.
fn install_desktop_entry() {
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.components().any(|c| c.as_os_str() == "target") {
        return;
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    let entry = include_str!("../assets/vision.desktop").replace("Exec=vision", &format!("Exec={}", exe.display()));
    let files = [
        (data.join("applications/vision.desktop"), entry.into_bytes()),
        (data.join("icons/hicolor/scalable/apps/vision.svg"), include_bytes!("../assets/vision.svg").to_vec()),
    ];
    for (path, bytes) in files {
        if fs::read(&path).ok().as_deref() != Some(&bytes[..]) {
            let _ = vision::write_atomic(&path, &bytes);
        }
    }
}

fn unit_dir() -> PathBuf {
    vision::config_dir().parent().unwrap_or(&vision::config_dir()).join("systemd/user")
}

fn login_enabled() -> bool {
    unit_dir().join("default.target.wants/vision.service").exists()
}

fn set_login(on: bool) {
    if on {
        let unit = format!(
            "[Unit]\nDescription=Vision display color daemon\n\n[Service]\nExecStart={}\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
            visiond_path().display()
        );
        let _ = vision::write_atomic(&unit_dir().join("vision.service"), unit.as_bytes());
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let _ = Command::new("systemctl").args(["--user", "enable", "vision.service"]).status();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "vision.service"]).status();
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    config: Config,
    selected: String,
    link_all: bool,
    login: bool,
    status: Option<Status>,
    daemon_up: bool,

    dirty: bool,
    last_write: Instant,
    last_poll: Instant,
    last_spawn: Instant,
    /// mtime of the config as we last read or wrote it.
    config_mtime: Option<SystemTime>,

    /// Undo history: `committed` is the last settled state.
    committed: Config,
    undo: Vec<Config>,
    redo: Vec<Config>,

    edit: Option<ValueEdit>,
    /// Name being typed for a new preset (and whether to grab focus).
    new_preset: Option<(String, bool)>,

    password: String,
    grant_rx: Option<mpsc::Receiver<Result<(), String>>>,
    grant_err: Option<String>,
    toast: Option<(String, Instant)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let config = Config::load().unwrap_or_default();
        let mut app = App {
            committed: config.clone(),
            config,
            selected: String::new(),
            link_all: false,
            login: login_enabled(),
            status: None,
            daemon_up: false,
            dirty: false,
            last_write: Instant::now(),
            last_poll: Instant::now() - Duration::from_secs(10),
            last_spawn: Instant::now() - Duration::from_secs(60),
            config_mtime: vision::config_mtime(),
            undo: Vec::new(),
            redo: Vec::new(),
            edit: None,
            new_preset: None,
            password: String::new(),
            grant_rx: None,
            grant_err: None,
            toast: None,
        };
        install_desktop_entry();
        app.poll();
        app
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn outputs(&self) -> Vec<(String, Option<String>)> {
        let mut v: Vec<(String, Option<String>)> = self
            .status
            .as_ref()
            .map(|s| s.outputs.iter().map(|o| (o.name.clone(), o.model.clone())).collect())
            .unwrap_or_default();
        if v.is_empty() {
            v = self.config.outputs.keys().map(|k| (k.clone(), None)).collect();
        }
        v
    }

    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.last_poll = Instant::now();
        self.daemon_up = vision::daemon_running();
        if !self.daemon_up && self.last_spawn.elapsed() > Duration::from_secs(5) {
            self.last_spawn = Instant::now();
            spawn_daemon();
        }
        self.status = if self.daemon_up { Status::load() } else { None };
        // The file is the single source of truth: pick up outside edits
        // (another Vision window, a hand edit) unless we have unsaved changes.
        let m = vision::config_mtime();
        if m != self.config_mtime && !self.dirty {
            if let Ok(c) = Config::load() {
                self.committed = c.clone();
                self.config = c;
            }
            self.config_mtime = m;
        }
        let outs = self.outputs();
        if !outs.iter().any(|(n, _)| *n == self.selected) {
            self.selected = outs.first().map(|o| o.0.clone()).unwrap_or_default();
        }
    }

    fn save_if_needed(&mut self, ctx: &egui::Context) {
        if !self.dirty {
            return;
        }
        if self.last_write.elapsed() >= Duration::from_millis(16) {
            if let Err(e) = self.config.save() {
                self.toast(format!("Couldn't save settings: {e}"));
            }
            self.dirty = false;
            self.last_write = Instant::now();
            self.config_mtime = vision::config_mtime();
        } else {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    // --- history ----------------------------------------------------------

    /// Record a history step once a change settles (a whole slider drag is
    /// one step, not hundreds).
    fn commit_history(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_down()) || self.config == self.committed {
            return;
        }
        self.undo.push(std::mem::replace(&mut self.committed, self.config.clone()));
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.config, prev.clone()));
            self.committed = prev;
            self.dirty = true;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.config, next.clone()));
            self.committed = next;
            self.dirty = true;
        }
    }

    // --- settings ---------------------------------------------------------

    fn current(&self) -> Adjust {
        self.config.outputs.get(&self.selected).copied().unwrap_or_default()
    }

    fn set_current(&mut self, adj: Adjust) {
        let targets: Vec<String> = if self.link_all {
            self.outputs().into_iter().map(|o| o.0).collect()
        } else {
            vec![self.selected.clone()]
        };
        for t in targets.into_iter().filter(|t| !t.is_empty()) {
            if adj.is_identity() {
                self.config.outputs.remove(&t);
            } else {
                self.config.outputs.insert(t, adj);
            }
        }
        self.dirty = true;
    }

    // --- sections ---------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            paint_logo(ui, r);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Vision").size(22.0).strong().color(TEXT));
                ui.label(muted("Display color"));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = self.config.enabled;
                if toggle(ui, &mut on).on_hover_text("Turn all adjustments on or off").changed() {
                    self.config.enabled = on;
                    self.dirty = true;
                }
                ui.label(egui::RichText::new(if on { "On" } else { "Off" }).size(13.0).color(MUTED));
                ui.add_space(14.0);
                let reset = ui
                    .add_enabled(!self.current().is_identity(), btn("Reset", false))
                    .on_hover_text(if self.link_all {
                        "Restore stock colors on all displays"
                    } else {
                        "Restore stock colors on this display"
                    });
                if reset.clicked() {
                    self.set_current(Adjust::default());
                    self.toast("Reset to stock colors");
                }
                if ui.add_enabled(!self.redo.is_empty(), btn("Redo", false)).on_hover_text("Ctrl+Shift+Z").clicked() {
                    self.redo();
                }
                if ui.add_enabled(!self.undo.is_empty(), btn("Undo", false)).on_hover_text("Ctrl+Z").clicked() {
                    self.undo();
                }
            });
        });
    }

    /// Everything that differs from stock on the selected display, each
    /// clickable to reset, so nothing is ever hidden further down the page.
    fn adjusted_row(&mut self, ui: &mut Ui) {
        let mut adj = self.current();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            if !self.config.enabled {
                ui.label(muted("Off: every display shows stock colors"));
                return;
            }
            let modified: Vec<&Spec> =
                all_specs().filter(|s| (s.field.get(&adj) - s.field.default_value()).abs() > 1e-6).collect();
            if modified.is_empty() {
                ui.label(muted("No adjustments: stock colors"));
                return;
            }
            ui.label(muted("Adjusted:"));
            for s in modified {
                let text = format!("{} {}  ×", s.field.label(), s.field.format(s.field.get(&adj)));
                if chip(ui, &text, None, true).on_hover_text("Reset").clicked() {
                    s.field.set(&mut adj, s.field.default_value());
                    changed = true;
                }
            }
        });
        if changed {
            self.set_current(adj);
        }
    }

    fn warning(&self, ui: &mut Ui) -> bool {
        let Some(w) = self.status.as_ref().and_then(|s| s.warning.clone()) else { return false };
        card_with(ui, "Conflict", WARN, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(w).size(13.0).color(TEXT)).wrap());
        });
        true
    }

    fn displays(&mut self, ui: &mut Ui) {
        let outs = self.outputs();
        if outs.is_empty() {
            ui.label(muted("No displays detected yet"));
            return;
        }
        let conflicts: Vec<String> = self
            .status
            .as_ref()
            .map(|s| s.outputs.iter().filter(|o| o.conflict).map(|o| o.name.clone()).collect())
            .unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            for (name, model) in &outs {
                let mut sub = model.clone().or_else(|| name.starts_with("eDP").then(|| "Built-in".to_string()));
                if conflicts.contains(name) {
                    sub = Some("⚠ conflict".into());
                }
                if chip(ui, name, sub.as_deref(), *name == self.selected).clicked() {
                    self.selected = name.clone();
                }
            }
        });
        if outs.len() > 1 {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                toggle(ui, &mut self.link_all);
                ui.label(egui::RichText::new("Apply changes to all displays").size(13.0).color(MUTED));
            });
        }
    }

    fn preview(&self, ui: &mut Ui) {
        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
        let hues: Vec<Color32> = (0..=24).map(|i| hsv(i as f32 * 15.0, 1.0, 1.0)).collect();
        gradient(ui, r, &hues);
        ui.add_space(4.0);
        let (r, _) = ui.allocate_exact_size(vec2(w, 14.0), Sense::hover());
        gradient(ui, r, &[Color32::BLACK, Color32::WHITE]);
        ui.add_space(6.0);
        let sw = [
            0xf1c27d, 0xc68642, 0x8d5524, 0x6fa8dc, 0x3d85c6, 0x6aa84f, 0x38761d, 0xe06666, 0xffd966, 0x8e7cc3,
            0xff0000, 0x00ff00, 0x0000ff, 0x00ffff, 0xff00ff, 0xffff00,
        ];
        let n = sw.len() as f32;
        let gap = 4.0;
        let size = ((w - gap * (n - 1.0)) / n).min(64.0);
        let (row, _) = ui.allocate_exact_size(vec2(w, size), Sense::hover());
        for (i, c) in sw.iter().enumerate() {
            let x = row.left() + i as f32 * (size + gap);
            ui.painter().rect_filled(Rect::from_min_size(pos2(x, row.top()), Vec2::splat(size)), 5.0, hex(*c));
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Changes hit the whole screen live. Use this pattern to judge them.").size(11.5).color(MUTED));
    }

    fn presets(&mut self, ui: &mut Ui) {
        let cur = self.current();
        let mut apply: Option<Adjust> = None;
        let mut delete: Option<usize> = None;
        let mut update: Option<usize> = None;
        let mut save: Option<String> = None;
        card(ui, "Presets", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (name, p) in vision::builtin_presets() {
                    if chip(ui, name, None, cur == p).clicked() {
                        apply = Some(p);
                    }
                }
                for (i, p) in self.config.presets.iter().enumerate() {
                    let r = chip(ui, &p.name, None, cur == p.adjust).on_hover_text("Right-click for options");
                    if r.clicked() {
                        apply = Some(p.adjust);
                    }
                    r.context_menu(|ui| {
                        if ui.button("Update to current settings").clicked() {
                            update = Some(i);
                            ui.close();
                        }
                        if ui.button("Delete").clicked() {
                            delete = Some(i);
                            ui.close();
                        }
                    });
                }
                match &mut self.new_preset {
                    None => {
                        if chip(ui, "+ Save current", None, false).clicked() {
                            self.new_preset = Some((String::new(), true));
                        }
                    }
                    Some((name, fresh)) => {
                        let r = ui.add(
                            egui::TextEdit::singleline(name)
                                .hint_text("Preset name")
                                .desired_width(150.0)
                                .char_limit(32),
                        );
                        if *fresh {
                            r.request_focus();
                            *fresh = false;
                        }
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if button(ui, "Save", true).clicked() || enter {
                            save = Some(name.trim().to_string());
                        }
                        if button(ui, "Cancel", false).clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            self.new_preset = None;
                        }
                    }
                }
            });
        });
        if let Some(a) = apply {
            self.set_current(a);
        }
        if let Some(i) = update {
            self.config.presets[i].adjust = cur;
            self.dirty = true;
            self.toast(format!("Updated \"{}\"", self.config.presets[i].name));
        }
        if let Some(i) = delete {
            let p = self.config.presets.remove(i);
            self.dirty = true;
            self.toast(format!("Deleted \"{}\" (Ctrl+Z to undo)", p.name));
        }
        if let Some(name) = save {
            if !name.is_empty() {
                match self.config.presets.iter_mut().find(|p| p.name == name) {
                    Some(p) => p.adjust = cur,
                    None => self.config.presets.push(Preset { name: name.clone(), adjust: cur }),
                }
                self.dirty = true;
                self.new_preset = None;
                self.toast(format!("Saved \"{name}\""));
            }
        }
    }

    fn slider_card(&mut self, ui: &mut Ui, title: &str, specs: &[Spec]) {
        let mut adj = self.current();
        let mut changed = false;
        card(ui, title, |ui| {
            for s in specs {
                changed |= slider(ui, s, &mut adj, &mut self.edit);
            }
        });
        if changed && !self.selected.is_empty() {
            self.set_current(adj);
        }
    }

    fn left_column(&mut self, ui: &mut Ui) {
        self.displays(ui);
        ui.add_space(12.0);
        card(ui, "Preview", |ui| self.preview(ui));
        ui.add_space(12.0);
        self.presets(ui);
        ui.add_space(12.0);
    }

    fn right_column(&mut self, ui: &mut Ui) {
        for (title, specs) in [("Color", COLOR_SPECS), ("Tone", TONE_SPECS), ("Channels", CHANNEL_SPECS)] {
            self.slider_card(ui, title, specs);
            ui.add_space(12.0);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let (color, text) = match (&self.status, self.daemon_up) {
            (_, false) | (None, true) => (WARN, "Starting the helper…".to_string()),
            (Some(s), true) => match s.state {
                State::Ok => {
                    let drivers: std::collections::BTreeSet<&str> = s.outputs.iter().map(|o| o.driver.as_str()).collect();
                    let active = s.outputs.iter().filter(|o| o.active).count();
                    let n = s.outputs.len();
                    let mut parts = vec![];
                    if !s.compositor.is_empty() {
                        parts.push(s.compositor.clone());
                    }
                    parts.push(drivers.into_iter().collect::<Vec<_>>().join(" + "));
                    parts.push(format!("{active} of {n} display{} adjusted in hardware", if n == 1 { "" } else { "s" }));
                    (if s.warning.is_some() { WARN } else { OK }, parts.join(" · "))
                }
                State::Waiting | State::Starting => (WARN, s.message.clone()),
                State::NeedsPrivilege => (ERR, "Waiting for one-time setup".into()),
                State::Error => (ERR, s.message.clone()),
            },
        };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, color);
            ui.add(egui::Label::new(muted(&text)).wrap());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut login = self.login;
                if toggle(ui, &mut login).changed() {
                    set_login(login);
                    self.login = login_enabled();
                }
                ui.label(egui::RichText::new("Apply at login").size(13.0).color(MUTED));
            });
        });
    }

    // --- one-time setup ----------------------------------------------------

    fn needs_setup(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.state == State::NeedsPrivilege)
    }

    /// One-time permission prompt, like a Windows admin prompt.
    fn setup_card(&mut self, ui: &mut Ui) {
        let busy = self.grant_rx.is_some();
        card_with(ui, "One-time setup", ACCENT, |ui| {
            ui.label(egui::RichText::new("Vision needs permission to program your display hardware.").size(14.0).color(TEXT));
            ui.label(muted("Only Vision's helper gets this access. You'll be asked again after Vision updates."));
            ui.add_space(8.0);
            let mut submit = false;
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .hint_text("Your password")
                        .desired_width(220.0)
                        .interactive(!busy),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                if button(ui, if busy { "Granting…" } else { "Grant access" }, true).clicked() {
                    submit = true;
                }
            });
            if let Some(e) = &self.grant_err {
                ui.label(egui::RichText::new(e).size(12.0).color(ERR));
            }
            if submit && !busy {
                self.start_grant();
            }
        });
    }

    /// Runs `sudo setcap cap_sys_ptrace=ep <visiond>` on a background thread,
    /// feeding the password over stdin. The password is wiped afterwards.
    fn start_grant(&mut self) {
        let daemon = visiond_path();
        if !daemon.is_absolute() {
            self.grant_err = Some("Couldn't find visiond next to the vision binary".into());
            return;
        }
        let Some(setcap) = cli::setcap_path() else {
            self.grant_err = Some("setcap not found (install libcap)".into());
            return;
        };
        let mut pw = std::mem::take(&mut self.password);
        let (tx, rx) = mpsc::channel();
        self.grant_rx = Some(rx);
        self.grant_err = None;
        std::thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let mut child = Command::new("sudo")
                    .args(["-S", "-p", ""])
                    .arg(&setcap)
                    .arg("cap_sys_ptrace=ep")
                    .arg(&daemon)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("Couldn't run sudo: {e}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(pw.as_bytes());
                    let _ = stdin.write_all(b"\n");
                }
                let out = child.wait_with_output().map_err(|e| e.to_string())?;
                if out.status.success() {
                    return Ok(());
                }
                let msg = String::from_utf8_lossy(&out.stderr);
                if msg.contains("incorrect password") || msg.contains("try again") || msg.contains("no password") {
                    Err("Wrong password".into())
                } else {
                    Err(msg.lines().last().unwrap_or("sudo failed").trim().to_string())
                }
            })();
            // SAFETY: overwriting with zeros keeps the String valid UTF-8.
            unsafe { pw.as_bytes_mut().fill(0) };
            let _ = tx.send(result);
        });
    }

    fn poll_grant(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.grant_rx else { return };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.grant_rx = None;
                self.restart_daemon();
                if !self.login {
                    set_login(true);
                    self.login = login_enabled();
                }
                self.toast("Access granted");
            }
            Ok(Err(e)) => {
                self.grant_rx = None;
                self.grant_err = Some(e);
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            Err(mpsc::TryRecvError::Disconnected) => self.grant_rx = None,
        }
    }

    fn restart_daemon(&mut self) {
        if let Some(s) = self.status.take() {
            unsafe { libc::kill(s.pid as i32, libc::SIGTERM) };
        }
        std::thread::sleep(Duration::from_millis(150));
        spawn_daemon();
        self.last_spawn = Instant::now();
        self.last_poll = Instant::now() - Duration::from_secs(10);
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = self.edit.is_some() || self.new_preset.is_some() || self.needs_setup();
        if typing {
            return;
        }
        let (undo, redo) = ctx.input(|i| {
            let c = i.modifiers.command;
            let z = i.key_pressed(Key::Z);
            (c && z && !i.modifiers.shift, c && ((z && i.modifiers.shift) || i.key_pressed(Key::Y)))
        });
        if undo {
            self.undo();
        } else if redo {
            self.redo();
        }
    }
}

fn paint_logo(ui: &Ui, r: Rect) {
    let c = r.center();
    let rad = r.width() / 2.0;
    let mut mesh = Mesh::default();
    mesh.colored_vertex(c, Color32::WHITE);
    let n = 48;
    for i in 0..=n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(c + vec2(a.cos(), a.sin()) * rad, hsv(a.to_degrees(), 1.0, 1.0));
        if i > 0 {
            mesh.add_triangle(0, i as u32, i as u32 + 1);
        }
    }
    ui.painter().add(mesh);
    ui.painter().circle_filled(c, rad * 0.28, BG);
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 5.0);
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = BG;
        v.selection.bg_fill = ACCENT;
        v.widgets.inactive.weak_bg_fill = RAISED;
        v.widgets.inactive.bg_fill = BORDER;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.hovered.weak_bg_fill = BORDER;
        v.widgets.hovered.bg_fill = MUTED;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.weak_bg_fill = ACCENT_DIM;
        v.widgets.active.bg_fill = ACCENT;
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        self.poll_grant(&ctx);
        self.shortcuts(&ctx);
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Frame::new().fill(BG).inner_margin(Margin::same(18)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.header(ui);
                ui.add_space(10.0);
                self.adjusted_row(ui);
                ui.add_space(12.0);
                if self.needs_setup() || self.grant_rx.is_some() {
                    self.setup_card(ui);
                    ui.add_space(12.0);
                }
                if self.warning(ui) {
                    ui.add_space(12.0);
                }

                if ui.available_width() >= 880.0 {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    ui.columns(2, |cols| {
                        let (l, r) = cols.split_at_mut(1);
                        l[0].spacing_mut().item_spacing.x = 8.0;
                        r[0].spacing_mut().item_spacing.x = 8.0;
                        self.left_column(&mut l[0]);
                        self.right_column(&mut r[0]);
                    });
                    ui.spacing_mut().item_spacing.x = 8.0;
                } else {
                    self.left_column(ui);
                    self.right_column(ui);
                }
                card(ui, "", |ui| self.status_bar(ui));
            });
        });

        if let Some((msg, t)) = &self.toast {
            if t.elapsed() < Duration::from_secs(2) {
                let rect = ctx.content_rect();
                let p = ui.painter();
                let g = p.layout_no_wrap(msg.clone(), FontId::proportional(13.0), TEXT);
                let bx = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 40.0), g.size() + vec2(28.0, 16.0));
                p.rect(bx, 10.0, RAISED, Stroke::new(1.0, BORDER), StrokeKind::Inside);
                p.galley(bx.center() - g.size() / 2.0, g, TEXT);
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                self.toast = None;
            }
        }

        self.commit_history(&ctx);
        self.save_if_needed(&ctx);
    }
}

fn main() -> eframe::Result {
    cli::install_fish_completions();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(args));
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Vision")
            .with_app_id("vision")
            .with_inner_size([1000.0, 900.0])
            .with_min_inner_size([420.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("Vision", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
