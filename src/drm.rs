//! Minimal raw DRM/KMS ioctl layer.
//!
//! Vision never opens `/dev/dri/card*` itself: opening a primary node while no
//! DRM master exists would make *us* the master and lock the compositor out.
//! Instead every ioctl here runs on a file descriptor borrowed from the running
//! compositor with `pidfd_getfd(2)`, which is the very same open file (and thus
//! the same DRM master) the compositor uses. We only ever touch the CRTC
//! `CTM` and `DEGAMMA_LUT` properties, which wlroots leaves alone.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

// ---------------------------------------------------------------------------
// ioctl plumbing
// ---------------------------------------------------------------------------

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr
}
const fn iowr<T>(nr: u64) -> u64 {
    ioc(3, nr, std::mem::size_of::<T>())
}
const fn iow<T>(nr: u64) -> u64 {
    ioc(1, nr, std::mem::size_of::<T>())
}

#[repr(C)]
#[derive(Default)]
struct DrmAuth {
    magic: u32,
}

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_modeinfo` is 68 bytes; we only need room for one so the
/// kernel never triggers a (slow, possibly flickery) connector probe.
#[repr(C)]
struct ModeInfo([u8; 68]);

#[repr(C)]
#[derive(Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

#[repr(C)]
struct GetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetBlob {
    blob_id: u32,
    length: u32,
    data: u64,
}

#[repr(C)]
#[derive(Default)]
struct CreateBlob {
    data: u64,
    length: u32,
    blob_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct DestroyBlob {
    blob_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct Atomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

const IOCTL_AUTH_MAGIC: u64 = iow::<DrmAuth>(0x11);
const IOCTL_GETRESOURCES: u64 = iowr::<CardRes>(0xA0);
const IOCTL_GETCONNECTOR: u64 = iowr::<GetConnector>(0xA7);
const IOCTL_GETPROPERTY: u64 = iowr::<GetProperty>(0xAA);
const IOCTL_GETPROPBLOB: u64 = iowr::<GetBlob>(0xAC);
const IOCTL_OBJ_GETPROPERTIES: u64 = iowr::<ObjGetProperties>(0xB9);
const IOCTL_ATOMIC: u64 = iowr::<Atomic>(0xBC);
const IOCTL_CREATEPROPBLOB: u64 = iowr::<CreateBlob>(0xBD);
const IOCTL_DESTROYPROPBLOB: u64 = iowr::<DestroyBlob>(0xBE);

const OBJ_CRTC: u32 = 0xcccc_cccc;
const OBJ_CONNECTOR: u32 = 0xc0c0_c0c0;
const ATOMIC_TEST_ONLY: u32 = 0x0100;

fn ioctl<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    loop {
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Borrowing the compositor's master fd
// ---------------------------------------------------------------------------

/// A DRM master file descriptor borrowed from the compositor.
pub struct MasterFd {
    pub card: String,
    pub fd: OwnedFd,
    pub pid: i32,
    pub target_fd: i32,
}

#[derive(Debug)]
pub enum FindError {
    /// Candidates existed but `pidfd_getfd` was refused (missing CAP_SYS_PTRACE).
    NoPrivilege,
    /// No process of ours currently holds DRM master.
    NoCompositor,
}

fn pidfd_getfd(pid: i32, target: i32) -> io::Result<OwnedFd> {
    unsafe {
        let pidfd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32;
        if pidfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = OwnedFd::from_raw_fd(pidfd);
        let fd = libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), target, 0) as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Same trick libdrm's `drmIsMaster` uses: AUTH_MAGIC needs master, and magic 0
/// is never valid, so a master gets EINVAL while everyone else gets EACCES.
fn is_master(fd: RawFd) -> bool {
    let mut a = DrmAuth { magic: 0 };
    matches!(
        ioctl(fd, IOCTL_AUTH_MAGIC, &mut a).err().and_then(|e| e.raw_os_error()),
        Some(libc::EINVAL)
    )
}

fn card_name_of(link: &Path) -> Option<String> {
    let s = link.to_str()?;
    let name = s.strip_prefix("/dev/dri/")?;
    if name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit()) {
        Some(name.to_string())
    } else {
        None
    }
}

fn try_candidate(pid: i32, fd: i32, card: &str) -> Result<Option<MasterFd>, io::Error> {
    let owned = pidfd_getfd(pid, fd)?;
    // Make sure the fd still points at the card we think it does.
    let link = fs::read_link(format!("/proc/self/fd/{}", owned.as_raw_fd()))?;
    if card_name_of(&link).as_deref() != Some(card) || !is_master(owned.as_raw_fd()) {
        return Ok(None);
    }
    Ok(Some(MasterFd { card: card.to_string(), fd: owned, pid, target_fd: fd }))
}

/// Cache of (pid, fd, card) that were master last time, to avoid rescanning /proc.
#[derive(Default)]
pub struct MasterCache(Vec<(i32, i32, String)>);

pub fn find_masters(cache: &mut MasterCache) -> Result<Vec<MasterFd>, FindError> {
    // Fast path: everything cached still valid.
    if !cache.0.is_empty() {
        let mut out = Vec::new();
        for (pid, fd, card) in &cache.0 {
            if let Ok(Some(m)) = try_candidate(*pid, *fd, card) {
                out.push(m);
            }
        }
        if out.len() == cache.0.len() {
            return Ok(out);
        }
    }

    let uid = unsafe { libc::getuid() };
    let mut found: HashMap<String, MasterFd> = HashMap::new();
    let mut denied = false;

    let Ok(procs) = fs::read_dir("/proc") else { return Err(FindError::NoCompositor) };
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        // Only ever look at our own processes.
        match p.metadata() {
            Ok(m) if m.uid() == uid => {}
            _ => continue,
        }
        let Ok(fds) = fs::read_dir(p.path().join("fd")) else { continue };
        for f in fds.flatten() {
            let Ok(link) = fs::read_link(f.path()) else { continue };
            let Some(card) = card_name_of(&link) else { continue };
            if found.contains_key(&card) {
                continue;
            }
            let Some(fd) = f.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            match try_candidate(pid, fd, &card) {
                Ok(Some(m)) => {
                    found.insert(card, m);
                }
                Ok(None) => {}
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => denied = true,
                Err(_) => {}
            }
        }
    }

    if found.is_empty() {
        return Err(if denied { FindError::NoPrivilege } else { FindError::NoCompositor });
    }
    let mut out: Vec<MasterFd> = found.into_values().collect();
    out.sort_by(|a, b| a.card.cmp(&b.card));
    cache.0 = out.iter().map(|m| (m.pid, m.target_fd, m.card.clone())).collect();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

fn get_resources(fd: RawFd) -> io::Result<(Vec<u32>, Vec<u32>)> {
    loop {
        let mut r = CardRes::default();
        ioctl(fd, IOCTL_GETRESOURCES, &mut r)?;
        let mut crtcs = vec![0u32; r.count_crtcs as usize];
        let mut conns = vec![0u32; r.count_connectors as usize];
        let (nc, nn) = (r.count_crtcs, r.count_connectors);
        let mut r2 = CardRes {
            crtc_id_ptr: crtcs.as_mut_ptr() as u64,
            connector_id_ptr: conns.as_mut_ptr() as u64,
            count_crtcs: nc,
            count_connectors: nn,
            ..Default::default()
        };
        ioctl(fd, IOCTL_GETRESOURCES, &mut r2)?;
        if r2.count_crtcs == nc && r2.count_connectors == nn {
            return Ok((crtcs, conns));
        }
    }
}

fn get_connector(fd: RawFd, id: u32) -> io::Result<GetConnector> {
    let mut mode = ModeInfo([0; 68]);
    let mut c = GetConnector {
        connector_id: id,
        modes_ptr: &mut mode as *mut _ as u64,
        count_modes: 1,
        ..Default::default()
    };
    ioctl(fd, IOCTL_GETCONNECTOR, &mut c)?;
    Ok(c)
}

fn obj_props(fd: RawFd, id: u32, ty: u32) -> io::Result<Vec<(u32, u64)>> {
    loop {
        let mut q = ObjGetProperties { obj_id: id, obj_type: ty, ..Default::default() };
        ioctl(fd, IOCTL_OBJ_GETPROPERTIES, &mut q)?;
        let n = q.count_props;
        let mut ids = vec![0u32; n as usize];
        let mut vals = vec![0u64; n as usize];
        let mut q2 = ObjGetProperties {
            props_ptr: ids.as_mut_ptr() as u64,
            prop_values_ptr: vals.as_mut_ptr() as u64,
            count_props: n,
            obj_id: id,
            obj_type: ty,
        };
        ioctl(fd, IOCTL_OBJ_GETPROPERTIES, &mut q2)?;
        if q2.count_props == n {
            return Ok(ids.into_iter().zip(vals).collect());
        }
    }
}

fn prop_name(fd: RawFd, prop: u32) -> io::Result<String> {
    let mut p = GetProperty {
        values_ptr: 0,
        enum_blob_ptr: 0,
        prop_id: prop,
        flags: 0,
        name: [0; 32],
        count_values: 0,
        count_enum_blobs: 0,
    };
    ioctl(fd, IOCTL_GETPROPERTY, &mut p)?;
    Ok(CStr::from_bytes_until_nul(&p.name)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default())
}

pub fn get_blob(fd: RawFd, id: u32) -> io::Result<Vec<u8>> {
    let mut b = GetBlob { blob_id: id, ..Default::default() };
    ioctl(fd, IOCTL_GETPROPBLOB, &mut b)?;
    let mut data = vec![0u8; b.length as usize];
    let mut b2 = GetBlob { blob_id: id, length: b.length, data: data.as_mut_ptr() as u64 };
    ioctl(fd, IOCTL_GETPROPBLOB, &mut b2)?;
    data.truncate(b2.length as usize);
    Ok(data)
}

/// A property blob that is destroyed (its id released) when dropped. Any CRTC
/// state referencing it keeps its own reference, so this never pulls the rug.
pub struct Blob<'a> {
    fd: &'a OwnedFd,
    pub id: u32,
}

impl<'a> Blob<'a> {
    pub fn new(fd: &'a OwnedFd, data: &[u8]) -> io::Result<Self> {
        let mut c = CreateBlob { data: data.as_ptr() as u64, length: data.len() as u32, blob_id: 0 };
        ioctl(fd.as_raw_fd(), IOCTL_CREATEPROPBLOB, &mut c)?;
        Ok(Blob { fd, id: c.blob_id })
    }
}

impl Drop for Blob<'_> {
    fn drop(&mut self) {
        let mut d = DestroyBlob { blob_id: self.id };
        let _ = ioctl(self.fd.as_raw_fd(), IOCTL_DESTROYPROPBLOB, &mut d);
    }
}

fn atomic(fd: RawFd, obj: u32, props: &[(u32, u64)], flags: u32) -> io::Result<()> {
    let objs = [obj];
    let counts = [props.len() as u32];
    let ids: Vec<u32> = props.iter().map(|p| p.0).collect();
    let vals: Vec<u64> = props.iter().map(|p| p.1).collect();
    let mut a = Atomic {
        flags,
        count_objs: 1,
        objs_ptr: objs.as_ptr() as u64,
        count_props_ptr: counts.as_ptr() as u64,
        props_ptr: ids.as_ptr() as u64,
        prop_values_ptr: vals.as_ptr() as u64,
        ..Default::default()
    };
    ioctl(fd, IOCTL_ATOMIC, &mut a)
}

/// Commit a set of CRTC properties: test first, then a blocking commit,
/// retrying if the compositor has a flip in flight.
pub fn set_crtc_props(fd: RawFd, crtc: u32, props: &[(u32, u64)]) -> io::Result<()> {
    atomic(fd, crtc, props, ATOMIC_TEST_ONLY)?;
    let mut last = None;
    for _ in 0..50 {
        match atomic(fd, crtc, props, 0) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                last = Some(e);
                sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EBUSY)))
}

// ---------------------------------------------------------------------------
// Output enumeration
// ---------------------------------------------------------------------------

const CONNECTOR_NAMES: &[&str] = &[
    "Unknown", "VGA", "DVI-I", "DVI-D", "DVI-A", "Composite", "SVIDEO", "LVDS", "Component",
    "DIN", "DP", "HDMI-A", "HDMI-B", "TV", "eDP", "Virtual", "DSI", "DPI", "Writeback", "SPI",
    "USB",
];

#[derive(Debug, Clone, Copy)]
pub struct Prop {
    pub id: u32,
    pub value: u64,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub name: String,
    pub model: Option<String>,
    pub crtc: u32,
    pub ctm: Option<Prop>,
    pub degamma: Option<Prop>,
    pub degamma_size: u32,
}

/// Property-id → name cache (ids are stable for the lifetime of a device).
#[derive(Default)]
pub struct PropNames(HashMap<(String, u32), String>);

impl PropNames {
    fn lookup(&mut self, card: &str, fd: RawFd, id: u32) -> String {
        self.0
            .entry((card.to_string(), id))
            .or_insert_with(|| prop_name(fd, id).unwrap_or_default())
            .clone()
    }
}

pub fn outputs(m: &MasterFd, names: &mut PropNames) -> io::Result<Vec<Output>> {
    let fd = m.fd.as_raw_fd();
    let (_, conns) = get_resources(fd)?;
    let mut out = Vec::new();
    for cid in conns {
        let Ok(c) = get_connector(fd, cid) else { continue };
        if c.connection != 1 {
            continue;
        }
        let ty = CONNECTOR_NAMES.get(c.connector_type as usize).copied().unwrap_or("Unknown");
        let name = format!("{}-{}", ty, c.connector_type_id);

        let mut crtc = 0u32;
        let mut edid = 0u64;
        for (pid, val) in obj_props(fd, cid, OBJ_CONNECTOR)? {
            match names.lookup(&m.card, fd, pid).as_str() {
                "CRTC_ID" => crtc = val as u32,
                "EDID" => edid = val,
                _ => {}
            }
        }
        if crtc == 0 {
            continue; // connected but disabled by the compositor
        }
        let model = if edid != 0 {
            get_blob(fd, edid as u32).ok().and_then(|b| edid_name(&b))
        } else {
            None
        };

        let mut o = Output { name, model, crtc, ctm: None, degamma: None, degamma_size: 0 };
        for (pid, val) in obj_props(fd, crtc, OBJ_CRTC)? {
            match names.lookup(&m.card, fd, pid).as_str() {
                "CTM" => o.ctm = Some(Prop { id: pid, value: val }),
                "DEGAMMA_LUT" => o.degamma = Some(Prop { id: pid, value: val }),
                "DEGAMMA_LUT_SIZE" => o.degamma_size = val as u32,
                _ => {}
            }
        }
        out.push(o);
    }
    Ok(out)
}

/// Pull a human name ("BOE 0x0B66" / "DELL U2723QE") out of an EDID blob.
fn edid_name(e: &[u8]) -> Option<String> {
    if e.len() < 128 || e[0..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] {
        return None;
    }
    for off in [54usize, 72, 90, 108] {
        let d = &e[off..off + 18];
        if d[0] == 0 && d[1] == 0 && d[3] == 0xFC {
            let s: String = d[5..18]
                .iter()
                .take_while(|&&b| b != b'\n' && b != 0)
                .map(|&b| b as char)
                .collect();
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    // Fall back to the PNP manufacturer id.
    let id = u16::from_be_bytes([e[8], e[9]]);
    let ch = |v: u16| (b'A' + ((v & 0x1f) as u8).saturating_sub(1)) as char;
    let pnp: String = [ch(id >> 10), ch(id >> 5), ch(id)].iter().collect();
    let product = u16::from_le_bytes([e[10], e[11]]);
    Some(format!("{pnp} {product:04X}"))
}

/// Read the current blob behind a property (None when unset).
pub fn current_blob(fd: RawFd, p: Prop) -> Option<Vec<u8>> {
    if p.value == 0 {
        None
    } else {
        get_blob(fd, p.value as u32).ok()
    }
}

pub fn driver_of(card: &str) -> String {
    fs::read_link(format!("/sys/class/drm/{card}/device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".into())
}
