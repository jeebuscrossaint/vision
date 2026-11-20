//! visiond: keeps the display hardware's color pipeline in sync with
//! ~/.config/vision/config.json.
//!
//!   visiond          run forever (watches the config, re-applies after
//!                    compositor restarts, hotplug, suspend, VT switches)
//!   visiond --once   apply once and exit
//!   visiond --reset  restore every display to stock and exit
//!
//! Needs CAP_SYS_PTRACE (granted from the Vision window) to borrow the
//! compositor's DRM master fd via pidfd_getfd(2).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use vision::drm::{self, FindError, MasterCache, PropNames};
use vision::{Config, OutputInfo, State, Status};

/// (CTM blob, DEGAMMA blob); None = property cleared.
type Pipeline = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Foreign changes within this window count towards a conflict.
const CONFLICT_WINDOW: Duration = Duration::from_secs(60);
const CONFLICT_STRIKES: usize = 3;
/// How long to leave an output alone once a conflict is detected.
const BACKOFF: Duration = Duration::from_secs(60);

/// Compositors known to drive the CTM themselves (night light, color management).
const CTM_COMPOSITORS: &[(&str, &str)] = &[
    ("kwin_wayland", "KDE's Night Light"),
    ("gnome-shell", "GNOME's Night Light"),
    ("Hyprland", "hyprsunset"),
    ("cosmic-comp", "COSMIC's night light"),
];

struct Daemon {
    cache: MasterCache,
    names: PropNames,
    config: Config,
    last_status: String,
    /// What we last left on each output.
    owned: HashMap<String, Pipeline>,
    /// Recent foreign changes per output.
    strikes: HashMap<String, Vec<Instant>>,
    backoff: HashMap<String, Instant>,
    /// --reset: clear everything regardless of who set it.
    force: bool,
}

fn comm(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

impl Daemon {
    fn new() -> Self {
        let owned = fs::read(vision::applied_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Daemon {
            cache: MasterCache::default(),
            names: PropNames::default(),
            config: Config::default(),
            last_status: String::new(),
            owned,
            strikes: HashMap::new(),
            backoff: HashMap::new(),
            force: false,
        }
    }

    fn save_owned(&self) {
        if let Ok(j) = serde_json::to_vec(&self.owned) {
            let _ = vision::write_atomic(&vision::applied_path(), &j);
        }
    }

    /// Bring every connected display in line with the config. Cheap when
    /// nothing changed: it only reads the current blobs and compares.
    fn sync(&mut self) -> Status {
        let mut status = Status { pid: std::process::id(), ..Default::default() };

        let masters = match drm::find_masters(&mut self.cache) {
            Ok(m) => m,
            Err(FindError::NoPrivilege) => {
                status.state = State::NeedsPrivilege;
                status.message = "visiond lacks CAP_SYS_PTRACE — open Vision to grant it".into();
                return status;
            }
            Err(FindError::NoCompositor) => {
                status.state = State::Waiting;
                status.message = "Waiting for the compositor".into();
                return status;
            }
        };
        status.compositor = masters.first().map(|m| comm(m.pid)).unwrap_or_default();

        let now = Instant::now();
        let mut errors = Vec::new();
        let mut conflicted = Vec::new();
        let mut owned_changed = false;

        for m in &masters {
            let driver = drm::driver_of(&m.card);
            let outs = match drm::outputs(m, &mut self.names) {
                Ok(o) => o,
                Err(e) => {
                    errors.push(format!("{}: {e}", m.card));
                    continue;
                }
            };
            let fd = m.fd.as_raw_fd();
            for o in outs {
                let mut info = OutputInfo {
                    name: o.name.clone(),
                    model: o.model.clone(),
                    card: m.card.clone(),
                    driver: driver.clone(),
                    supported: o.ctm.is_some(),
                    ..Default::default()
                };

                let adj = if self.force { vision::Adjust::default() } else { self.config.effective(&o.name) };
                let want: Pipeline = (
                    (!adj.matrix_is_identity() && o.ctm.is_some()).then(|| adj.ctm_blob()),
                    (!adj.tone_is_identity() && o.degamma.is_some()).then(|| adj.degamma_blob(o.degamma_size)),
                );
                let cur: Pipeline = (
                    o.ctm.and_then(|p| drm::current_blob(fd, p)),
                    o.degamma.and_then(|p| drm::current_blob(fd, p)),
                );
                let prev = self.owned.get(&o.name);

                // A non-empty value we didn't put there belongs to someone else.
                let mine = |c: &Option<Vec<u8>>, p: Option<&Option<Vec<u8>>>| c.is_none() || p == Some(c);
                let foreign = !self.force
                    && !(mine(&cur.0, prev.map(|p| &p.0)) && mine(&cur.1, prev.map(|p| &p.1)));

                if cur != want && foreign {
                    if want == (None, None) && prev.is_none() {
                        // Nothing of ours to remove; leave the other program be.
                        status.outputs.push(info);
                        continue;
                    }
                    let s = self.strikes.entry(o.name.clone()).or_default();
                    s.retain(|t| now.duration_since(*t) < CONFLICT_WINDOW);
                    s.push(now);
                    if s.len() >= CONFLICT_STRIKES {
                        self.backoff.insert(o.name.clone(), now + BACKOFF);
                        s.clear();
                    }
                }
                if self.backoff.get(&o.name).is_some_and(|until| now < *until) {
                    info.conflict = true;
                    conflicted.push(o.name.clone());
                    status.outputs.push(info);
                    continue;
                }

                if cur != want {
                    let r = (|| -> io::Result<()> {
                        let mut blobs = Vec::new();
                        let mut props = Vec::new();
                        for (prop, w, c) in [(o.ctm, &want.0, &cur.0), (o.degamma, &want.1, &cur.1)] {
                            let Some(p) = prop else { continue };
                            if w == c {
                                continue;
                            }
                            match w {
                                Some(data) => {
                                    let b = drm::Blob::new(&m.fd, data)?;
                                    props.push((p.id, b.id as u64));
                                    blobs.push(b);
                                }
                                None => props.push((p.id, 0)),
                            }
                        }
                        drm::set_crtc_props(fd, o.crtc, &props)
                    })();
                    if let Err(e) = r {
                        errors.push(format!("{}: {e}", o.name));
                        status.outputs.push(info);
                        continue;
                    }
                }
                if prev != Some(&want) {
                    self.owned.insert(o.name.clone(), want.clone());
                    owned_changed = true;
                }
                info.active = want != (None, None);
                status.outputs.push(info);
            }
        }
        drop(masters); // never hold the compositor's fd longer than needed
        if owned_changed {
            self.save_owned();
        }

        if !conflicted.is_empty() {
            let culprit = CTM_COMPOSITORS
                .iter()
                .find(|(c, _)| *c == status.compositor)
                .map(|(_, what)| format!("likely {what}"))
                .unwrap_or_else(|| "a night-light or color tool?".into());
            status.warning = Some(format!(
                "Another program keeps changing the colors on {} ({culprit}). Vision is backing off \
                 there to avoid flicker; turn the other tool off and Vision takes over again.",
                conflicted.join(", ")
            ));
        }

        if errors.is_empty() {
            status.state = State::Ok;
            status.message = "Applied in display hardware".into();
        } else {
            status.state = State::Error;
            status.message = errors.join("; ");
        }
        status
    }

    fn publish(&mut self, status: &Status) {
        let json = serde_json::to_string_pretty(status).unwrap_or_default();
        if json != self.last_status {
            let _ = vision::write_atomic(&vision::status_path(), json.as_bytes());
            self.last_status = json;
        }
    }

    fn reload(&mut self) {
        match Config::load() {
            Ok(c) => {
                if c != self.config {
                    // The user is actively changing things: give it another go.
                    self.backoff.clear();
                }
                self.config = c;
            }
            Err(e) => eprintln!("visiond: keeping previous config, failed to parse: {e}"),
        }
    }
}

fn take_lock() -> Option<fs::File> {
    let _ = fs::create_dir_all(vision::runtime_dir());
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(vision::lock_path())
        .ok()?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

/// inotify on the config directory (the GUI writes via rename).
fn watch_config() -> Option<i32> {
    let dir = vision::config_dir();
    let _ = fs::create_dir_all(&dir);
    let cdir = std::ffi::CString::new(dir.to_str()?).ok()?;
    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK);
        if fd < 0 {
            return None;
        }
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
        if libc::inotify_add_watch(fd, cdir.as_ptr(), mask) < 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Block until the config changes or the timeout passes. Returns true on change.
fn wait(ino: Option<i32>, timeout: Duration) -> bool {
    let Some(fd) = ino else {
        std::thread::sleep(timeout);
        return false;
    };
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
    if r <= 0 {
        return false;
    }
    let mut buf = [0u8; 4096];
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    true
}

fn main() {
    let arg = std::env::args().nth(1);
    let mut d = Daemon::new();

    match arg.as_deref() {
        Some("--reset") | Some("--once") => {
            if arg.as_deref() == Some("--reset") {
                d.force = true;
            } else {
                d.reload();
            }
            let s = d.sync();
            println!("{:?}: {}", s.state, s.message);
            for o in &s.outputs {
                println!(
                    "  {} ({}, {}) active={} conflict={}",
                    o.name,
                    o.model.as_deref().unwrap_or("?"),
                    o.driver,
                    o.active,
                    o.conflict
                );
            }
            if let Some(w) = &s.warning {
                println!("warning: {w}");
            }
            std::process::exit(if s.state == State::Ok { 0 } else { 1 });
        }
        Some("-h") | Some("--help") => {
            println!("usage: visiond [--once | --reset]");
            return;
        }
        _ => {}
    }

    let Some(_lock) = take_lock() else {
        eprintln!("visiond: already running");
        return;
    };

    let ino = watch_config();
    d.reload();
    let mut last_reload = Instant::now();
    loop {
        let s = d.sync();
        d.publish(&s);
        if wait(ino, Duration::from_millis(1000)) {
            // Coalesce bursts of writes from a slider drag.
            std::thread::sleep(Duration::from_millis(4));
            while wait(ino, Duration::ZERO) {}
            d.reload();
            last_reload = Instant::now();
        } else if last_reload.elapsed() > Duration::from_secs(30) {
            d.reload();
            last_reload = Instant::now();
        }
    }
}
