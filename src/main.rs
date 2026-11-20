//! Vision: display color control panel (saturation, hue, temperature,
//! brightness, contrast, gamma, RGB gains) for Wayland.
//!
//! The GUI is unprivileged. It only edits ~/.config/vision/config.toml; the
//! `visiond` helper picks up every write instantly and programs the display
//! hardware, so slider drags are live.

use std::fs;
use std::io::Write;
use std::sync::mpsc;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Layout, Margin, Mesh,
    Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use vision::{Adjust, Config, State, Status};

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
    let hp = (h.rem_euclid(360.0)) / 60.0;
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

// ---------------------------------------------------------------------------
// Sliders
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Field {
    Saturation,
    Hue,
    Temperature,
    Brightness,
    Contrast,
    Gamma,
    Red,
    Green,
    Blue,
}

impl Field {
    fn get(self, a: &mut Adjust) -> &mut f64 {
        match self {
            Field::Saturation => &mut a.saturation,
            Field::Hue => &mut a.hue,
            Field::Temperature => &mut a.temperature,
            Field::Brightness => &mut a.brightness,
            Field::Contrast => &mut a.contrast,
            Field::Gamma => &mut a.gamma,
            Field::Red => &mut a.red,
            Field::Green => &mut a.green,
            Field::Blue => &mut a.blue,
        }
    }
}

struct Spec {
    label: &'static str,
    field: Field,
    min: f64,
    max: f64,
    step: f64,
    fmt: fn(f64) -> String,
    track: fn() -> Vec<Color32>,
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}
fn signed_pct(v: f64) -> String {
    format!("{:+.0}", v * 100.0)
}
fn plain() -> Vec<Color32> {
    vec![]
}

const COLOR_SPECS: &[Spec] = &[
    Spec {
        label: "Saturation",
        field: Field::Saturation,
        min: 0.0,
        max: 3.0,
        step: 0.01,
        fmt: pct,
        track: || vec![hex(0x7a7a7a), hex(0xb05a8a), hex(0xff2d8a), hex(0xff0066)],
    },
    Spec {
        label: "Hue",
        field: Field::Hue,
        min: -180.0,
        max: 180.0,
        step: 1.0,
        fmt: |v| format!("{v:+.0}°"),
        track: || (0..=12).map(|i| hsv(180.0 + i as f32 * 30.0, 0.75, 0.95)).collect(),
    },
    Spec {
        label: "Temperature",
        field: Field::Temperature,
        min: 2700.0,
        max: 10000.0,
        step: 50.0,
        fmt: |v| format!("{v:.0} K"),
        track: || {
            (0..=10)
                .map(|i| {
                    let k = 2700.0 + i as f64 * 730.0;
                    let c = vision::color::temperature_rgb(k);
                    Color32::from_rgb((c[0] * 235.0) as u8, (c[1] * 235.0) as u8, (c[2] * 235.0) as u8)
                })
                .collect()
        },
    },
];

const TONE_SPECS: &[Spec] = &[
    Spec { label: "Brightness", field: Field::Brightness, min: -0.5, max: 0.5, step: 0.01, fmt: signed_pct, track: plain },
    Spec { label: "Contrast", field: Field::Contrast, min: 0.5, max: 1.5, step: 0.01, fmt: pct, track: plain },
    Spec { label: "Gamma", field: Field::Gamma, min: 0.4, max: 2.5, step: 0.01, fmt: |v| format!("{v:.2}"), track: plain },
];

const CHANNEL_SPECS: &[Spec] = &[
    Spec { label: "Red", field: Field::Red, min: 0.5, max: 1.5, step: 0.01, fmt: pct, track: || vec![hex(0x3a1010), hex(0xff4040)] },
    Spec { label: "Green", field: Field::Green, min: 0.5, max: 1.5, step: 0.01, fmt: pct, track: || vec![hex(0x0f3315), hex(0x40e060)] },
    Spec { label: "Blue", field: Field::Blue, min: 0.5, max: 1.5, step: 0.01, fmt: pct, track: || vec![hex(0x101a3a), hex(0x4a7dff)] },
];

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

/// Returns true when the value changed.
fn slider(ui: &mut Ui, spec: &Spec, adj: &mut Adjust) -> bool {
    let default = *spec.field.get(&mut Adjust::default());
    let value = spec.field.get(adj);
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(spec.label).size(14.0).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let modified = (*value - default).abs() > 1e-6;
            if modified {
                let r = ui
                    .add(egui::Label::new(egui::RichText::new("⟲").size(15.0).color(MUTED)).sense(Sense::click()))
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text("Reset");
                if r.clicked() {
                    *value = default;
                    changed = true;
                }
            }
            ui.label(
                egui::RichText::new((spec.fmt)(*value))
                    .size(14.0)
                    .monospace()
                    .color(if modified { ACCENT } else { MUTED }),
            );
        });
    });

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click_and_drag());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let inner = rect.shrink2(vec2(9.0, 0.0));
    let to_x = |v: f64| inner.left() + inner.width() * ((v - spec.min) / (spec.max - spec.min)) as f32;

    if resp.double_clicked() {
        *value = default;
        changed = true;
    } else if resp.is_pointer_button_down_on() || resp.dragged() {
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - inner.left()) / inner.width()).clamp(0.0, 1.0) as f64;
            let mut v = spec.min + t * (spec.max - spec.min);
            v = (v / spec.step).round() * spec.step;
            // Gentle snap to the default so "back to normal" is easy.
            if (to_x(v) - to_x(default)).abs() < 5.0 {
                v = default;
            }
            if (v - *value).abs() > 1e-9 {
                *value = v.clamp(spec.min, spec.max);
                changed = true;
            }
        }
    }

    let track = Rect::from_center_size(rect.center(), vec2(inner.width() + 6.0, 6.0));
    let colors = (spec.track)();
    let hot = resp.hovered() || resp.dragged();
    if colors.is_empty() {
        ui.painter().rect_filled(track, 3.0, BORDER);
        let (a, b) = (to_x(default), to_x(*value));
        let fill = Rect::from_x_y_ranges(a.min(b)..=a.max(b), track.y_range());
        ui.painter().rect_filled(fill, 3.0, ACCENT);
    } else {
        gradient(ui, track, &colors);
    }
    // Default tick.
    let dx = to_x(default);
    ui.painter()
        .line_segment([pos2(dx, track.top() - 4.0), pos2(dx, track.bottom() + 4.0)], Stroke::new(1.5, MUTED));
    // Knob.
    let kx = to_x(*value);
    let r = if hot { 9.0 } else { 8.0 };
    ui.painter().circle(pos2(kx, rect.center().y), r, TEXT, Stroke::new(3.0, ACCENT));
    ui.add_space(6.0);
    changed
}

// ---------------------------------------------------------------------------
// Small widgets
// ---------------------------------------------------------------------------

fn chip(ui: &mut Ui, text: &str, sub: Option<&str>, selected: bool) -> egui::Response {
    let font = FontId::proportional(13.5);
    let small = FontId::proportional(11.0);
    let g = ui.painter().layout_no_wrap(text.to_string(), font, if selected { TEXT } else { MUTED });
    let gs = sub.map(|s| ui.painter().layout_no_wrap(s.to_string(), small, MUTED));
    let w = g.size().x.max(gs.as_ref().map_or(0.0, |g| g.size().x)) + 26.0;
    let h = if gs.is_some() { 44.0 } else { 30.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let hot = resp.hovered();
    let fill = if selected { ACCENT_DIM } else if hot { RAISED } else { CARD };
    let stroke = Stroke::new(1.0, if selected { ACCENT } else { BORDER });
    ui.painter().rect(rect, 8.0, fill, stroke, StrokeKind::Inside);
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
    let bg = lerp_color(BORDER, ACCENT, t);
    ui.painter().rect_filled(rect, 11.0, bg);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    ui.painter().circle_filled(pos2(x, rect.center().y), 8.0, TEXT);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
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

fn button(ui: &mut Ui, text: &str, primary: bool) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(text).size(13.0).color(TEXT))
        .fill(if primary { ACCENT } else { RAISED })
        .corner_radius(8.0)
        .min_size(vec2(0.0, 28.0));
    ui.add(b).on_hover_cursor(CursorIcon::PointingHand)
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

fn presets() -> Vec<(&'static str, Adjust)> {
    let d = Adjust::default();
    vec![
        ("Standard", d),
        ("Vivid", Adjust { saturation: 1.35, contrast: 1.04, ..d }),
        ("Punchy", Adjust { saturation: 1.7, contrast: 1.08, ..d }),
        ("Cinema", Adjust { saturation: 0.92, contrast: 1.08, gamma: 0.92, temperature: 6100.0, ..d }),
        ("Warm", Adjust { temperature: 5200.0, ..d }),
        ("Cool", Adjust { temperature: 8000.0, ..d }),
        ("Night", Adjust { temperature: 3400.0, saturation: 0.9, ..d }),
        ("Grayscale", Adjust { saturation: 0.0, ..d }),
    ]
}

// ---------------------------------------------------------------------------
// Helper process + login service
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
    if let Ok(child) = cmd.spawn() {
        // Reap in the background so it never lingers as a zombie.
        std::thread::spawn(move || {
            let mut c = child;
            let _ = c.wait();
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
    toast: Option<(String, Instant)>,
    password: String,
    grant_rx: Option<mpsc::Receiver<Result<(), String>>>,
    grant_err: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let config = Config::load().unwrap_or_default();
        let mut app = App {
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
            toast: None,
            password: String::new(),
            grant_rx: None,
            grant_err: None,
        };
        install_desktop_entry();
        app.poll();
        app
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
                self.toast = Some((format!("Couldn't save settings: {e}"), Instant::now()));
            }
            self.dirty = false;
            self.last_write = Instant::now();
        } else {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn current(&self) -> Adjust {
        self.config.outputs.get(&self.selected).copied().unwrap_or_default()
    }

    fn set_current(&mut self, adj: Adjust) {
        let targets: Vec<String> = if self.link_all {
            self.outputs().into_iter().map(|o| o.0).collect()
        } else {
            vec![self.selected.clone()]
        };
        for t in targets {
            if adj.is_identity() {
                self.config.outputs.remove(&t);
            } else {
                self.config.outputs.insert(t, adj);
            }
        }
        self.dirty = true;
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            paint_logo(ui, r);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Vision").size(22.0).strong().color(TEXT));
                ui.label(egui::RichText::new("Display color").size(12.5).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = self.config.enabled;
                if toggle(ui, &mut on).changed() {
                    self.config.enabled = on;
                    self.dirty = true;
                }
                ui.label(egui::RichText::new(if on { "On" } else { "Off" }).size(13.0).color(MUTED));
            });
        });
    }

    fn displays(&mut self, ui: &mut Ui) {
        let outs = self.outputs();
        if outs.is_empty() {
            ui.label(egui::RichText::new("No displays detected yet").color(MUTED));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            for (name, model) in &outs {
                let friendly = model.clone().or_else(|| name.starts_with("eDP").then(|| "Built-in".to_string()));
                if chip(ui, name, friendly.as_deref(), *name == self.selected).clicked() {
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
            0xf1c27d, 0xc68642, 0x8d5524, 0x6fa8dc, 0x3d85c6, 0x6aa84f, 0x38761d, 0xe06666, 0xffd966,
            0x8e7cc3, 0xff0000, 0x00ff00, 0x0000ff, 0x00ffff, 0xff00ff, 0xffff00,
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
        ui.label(
            egui::RichText::new("Changes hit the whole screen live. Use this pattern to judge them.")
                .size(11.5)
                .color(MUTED),
        );
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let (color, text) = match (&self.status, self.daemon_up) {
            (_, false) => (WARN, "Starting the helper…".to_string()),
            (None, true) => (WARN, "Helper starting…".to_string()),
            (Some(s), true) => match s.state {
                State::Ok => {
                    let active = s.outputs.iter().filter(|o| o.active).count();
                    let drv: Vec<String> = s.outputs.iter().map(|o| o.driver.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                    (
                        OK,
                        format!(
                            "Hardware color pipeline · {} · {} of {} display{} adjusted",
                            drv.join(" + "),
                            active,
                            s.outputs.len(),
                            if s.outputs.len() == 1 { "" } else { "s" }
                        ),
                    )
                }
                State::Waiting | State::Starting => (WARN, s.message.clone()),
                State::NeedsPrivilege => (ERR, "The helper needs a one-time permission grant".into()),
                State::Error => (ERR, s.message.clone()),
            },
        };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, color);
            ui.label(egui::RichText::new(text).size(12.5).color(MUTED));
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let mut login = self.login;
            if toggle(ui, &mut login).changed() {
                set_login(login);
                self.login = login_enabled();
            }
            ui.label(egui::RichText::new("Apply at login").size(13.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Reset display", false).clicked() {
                    self.set_current(Adjust::default());
                }
            });
        });
    }

    fn needs_setup(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.state == State::NeedsPrivilege)
    }

    /// One-time permission prompt, like a Windows admin prompt.
    fn setup_card(&mut self, ui: &mut Ui) {
        let busy = self.grant_rx.is_some();
        card(ui, "One-time setup", |ui| {
            ui.label(
                egui::RichText::new("Vision needs permission to program your display hardware.")
                    .size(14.0)
                    .color(TEXT),
            );
            ui.label(
                egui::RichText::new(
                    "Only Vision's helper gets this access. You'll be asked again after Vision updates.",
                )
                .size(12.0)
                .color(MUTED),
            );
            ui.add_space(8.0);
            let mut submit = false;
            ui.horizontal(|ui| {
                let field = egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .hint_text("Your password")
                    .desired_width(220.0)
                    .interactive(!busy);
                let r = ui.add(field);
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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
        let Some(setcap) = ["/usr/bin/setcap", "/usr/sbin/setcap", "/sbin/setcap"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
        else {
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
                self.toast = Some(("Access granted".into(), Instant::now()));
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
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = BG;
        v.override_text_color = None;
        v.selection.bg_fill = ACCENT;
        v.widgets.inactive.weak_bg_fill = RAISED;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.hovered.weak_bg_fill = BORDER;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.weak_bg_fill = ACCENT_DIM;
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        self.poll_grant(&ctx);
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Frame::new().fill(BG).inner_margin(Margin::same(18)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.header(ui);
                ui.add_space(14.0);
                if self.needs_setup() || self.grant_rx.is_some() {
                    self.setup_card(ui);
                    ui.add_space(12.0);
                }
                self.displays(ui);
                ui.add_space(12.0);
                card(ui, "Preview", |ui| self.preview(ui));
                ui.add_space(12.0);

                let mut adj = self.current();
                let mut changed = false;
                card(ui, "Presets", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                        for (name, p) in presets() {
                            if chip(ui, name, None, adj == p).clicked() {
                                adj = p;
                                changed = true;
                            }
                        }
                    });
                });
                ui.add_space(12.0);
                for (title, specs) in [("Color", COLOR_SPECS), ("Tone", TONE_SPECS), ("Channels", CHANNEL_SPECS)] {
                    card(ui, title, |ui| {
                        for s in specs {
                            changed |= slider(ui, s, &mut adj);
                        }
                    });
                    ui.add_space(12.0);
                }
                if changed && !self.selected.is_empty() {
                    self.set_current(adj);
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

        self.save_if_needed(&ctx);
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Vision")
            .with_app_id("vision")
            .with_inner_size([520.0, 860.0])
            .with_min_inner_size([420.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("Vision", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

