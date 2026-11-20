//! visiond: keeps the display hardware's color pipeline in sync with
//! ~/.config/vision/config.toml.
//!
//!   visiond          run forever (watches the config, re-applies after
//!                    compositor restarts, hotplug, suspend, VT switches)
//!   visiond --once   apply once and exit
//!   visiond --reset  restore every display to stock and exit
//!
//! Needs CAP_SYS_PTRACE (granted from the Vision window) to borrow the
//! compositor's DRM master fd via pidfd_getfd(2).

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use vision::drm::{self, FindError, MasterCache, PropNames};
use vision::{Config, OutputInfo, State, Status};

struct Daemon {
    cache: MasterCache,
    names: PropNames,
    config: Config,
    last_status: String,
}

impl Daemon {
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

        let mut errors = Vec::new();
        for m in &masters {
            let driver = drm::driver_of(&m.card);
            let outs = match drm::outputs(m, &mut self.names) {
                Ok(o) => o,
                Err(e) => {
                    errors.push(format!("{}: {e}", m.card));
                    continue;
                }
            };
            for o in outs {
                let mut info = OutputInfo {
                    name: o.name.clone(),
                    model: o.model.clone(),
                    card: m.card.clone(),
                    driver: driver.clone(),
                    supported: o.ctm.is_some(),
                    active: false,
                };
                let adj = self.config.effective(&o.name);
                let fd = m.fd.as_raw_fd();

                let want_ctm = (!adj.matrix_is_identity()).then(|| adj.ctm_blob());
                let want_lut = (!adj.tone_is_identity() && o.degamma.is_some())
                    .then(|| adj.degamma_blob(o.degamma_size));

                let mut blobs = Vec::new();
                let mut props = Vec::new();
                let mut plan = |prop: Option<drm::Prop>, want: Option<Vec<u8>>| -> io::Result<()> {
                    let Some(p) = prop else { return Ok(()) };
                    if drm::current_blob(fd, p) == want {
                        return Ok(());
                    }
                    match want {
                        Some(data) => {
                            let b = drm::Blob::new(&m.fd, &data)?;
                            props.push((p.id, b.id as u64));
                            blobs.push(b);
                        }
                        None => props.push((p.id, 0)),
                    }
                    Ok(())
                };
                let planned = plan(o.ctm, want_ctm.clone()).and_then(|_| plan(o.degamma, want_lut.clone()));

                let result = planned.and_then(|_| {
                    if props.is_empty() {
                        Ok(())
                    } else {
                        drm::set_crtc_props(fd, o.crtc, &props)
                    }
                });
                drop(blobs);
                match result {
                    Ok(()) => info.active = want_ctm.is_some() || want_lut.is_some(),
                    Err(e) => errors.push(format!("{}: {e}", o.name)),
                }
                status.outputs.push(info);
            }
        }
        drop(masters); // never hold the compositor's fd longer than needed

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
            if status.state != State::Ok || !self.last_status.contains("\"Ok\"") {
                eprintln!("visiond: {:?}: {}", status.state, status.message);
            }
            let _ = vision::write_atomic(&vision::status_path(), json.as_bytes());
            self.last_status = json;
        }
    }

    fn reload(&mut self) {
        match Config::load() {
            Ok(c) => self.config = c,
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
    let mut d = Daemon {
        cache: MasterCache::default(),
        names: PropNames::default(),
        config: Config::default(),
        last_status: String::new(),
    };

    match arg.as_deref() {
        Some("--reset") | Some("--once") => {
            if arg.as_deref() == Some("--once") {
                d.reload();
            }
            let s = d.sync();
            println!("{:?}: {}", s.state, s.message);
            for o in &s.outputs {
                println!("  {} ({}, {}) active={}", o.name, o.model.as_deref().unwrap_or("?"), o.driver, o.active);
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
    let mut last_full = Instant::now() - Duration::from_secs(60);
    loop {
        let s = d.sync();
        d.publish(&s);
        let idle = if s.state == State::Ok { Duration::from_millis(1500) } else { Duration::from_millis(1000) };
        if wait(ino, idle) {
            // Coalesce bursts of writes from a slider drag.
            std::thread::sleep(Duration::from_millis(4));
            while wait(ino, Duration::ZERO) {}
            d.reload();
        } else if last_full.elapsed() > Duration::from_secs(30) {
            d.reload();
            last_full = Instant::now();
        }
    }
}
