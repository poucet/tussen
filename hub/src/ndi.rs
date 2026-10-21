//! NDI in: one NDI source (TouchDesigner's NDI Out TOP on the TD laptop)
//! received by the hub and served to local pages as binary WebSocket
//! frames on the console's `/ndi` route, for WebGL compositing.
//!
//! The NDI runtime is loaded at run time (`libndi.dylib`; NDI's licence
//! does not let us vendor it), so a hub without it builds and runs, and
//! only `[ndi] enabled = true` asks for it. Install it from
//! <https://ndi.video/tools/> (NDI Tools / NDI SDK for Apple).
//!
//! The feed is **latest frame wins**: the receiver replaces one slot and
//! bumps a sequence number; each `/ndi` connection sends the newest frame
//! when it is ready for one, so a slow page drops frames and never backs
//! the receiver up.
//!
//! One frame on the wire, little-endian, 24-byte header then tight pixels:
//!
//! ```text
//! "TNDI" · u32 fourcc ("UYVY" | "RGBA" | "RGBX" | "BGRA" | "BGRX") · u32 width · u32 height · u64 hub_us
//! pixels: UYVY = width*2 bytes per row, the others width*4; no padding
//! ```
//!
//! `hub_us` is the hub clock ([`crate::console::now_us`]) when the frame
//! was received, the same clock the console's `ts` and the take use.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

/// `[ndi]` in hub.toml.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct NdiConfig {
    /// Receive NDI and serve it on the console's `/ndi`.
    pub enabled: bool,
    /// The source to receive: a substring of its NDI name
    /// ("TD-LAPTOP (TouchDesigner)"); "" takes the first one found.
    /// `simply-tussen ndi` lists what is visible.
    pub source: String,
    /// Comma-separated IPs to look for sources on, for networks where
    /// NDI's mDNS discovery does not cross (the TD laptop's IP).
    pub extra_ips: String,
    /// `highest` (full quality) or `lowest` (NDI's low-bandwidth proxy).
    pub bandwidth: String,
    /// Path to libndi.dylib, if it is not in a standard place.
    pub lib: String,
}

impl Default for NdiConfig {
    fn default() -> Self {
        NdiConfig {
            enabled: false,
            source: String::new(),
            extra_ips: String::new(),
            bandwidth: "highest".into(),
            lib: String::new(),
        }
    }
}

// ---- the latest-frame slot --------------------------------------------

/// The newest frame, already in wire format, and its sequence number.
pub struct Feed {
    slot: Mutex<(u64, Option<Arc<Vec<u8>>>)>,
    fresh: Condvar,
}

impl Feed {
    fn new() -> Feed {
        Feed { slot: Mutex::new((0, None)), fresh: Condvar::new() }
    }

    pub fn publish(&self, frame: Vec<u8>) {
        let mut s = self.slot.lock().unwrap();
        s.0 += 1;
        s.1 = Some(Arc::new(frame));
        self.fresh.notify_all();
    }

    /// The first frame newer than `seen`, waiting up to `timeout`.
    pub fn next(&self, seen: u64, timeout: Duration) -> Option<(u64, Arc<Vec<u8>>)> {
        let s = self.slot.lock().unwrap();
        let (s, _) = self.fresh.wait_timeout_while(s, timeout, |s| s.0 == seen).unwrap();
        s.1.as_ref().filter(|_| s.0 != seen).map(|f| (s.0, Arc::clone(f)))
    }
}

/// The process's one feed: the receiver writes it, `/ndi` reads it.
pub fn feed() -> &'static Feed {
    static FEED: OnceLock<Feed> = OnceLock::new();
    FEED.get_or_init(Feed::new)
}

/// One frame in wire format.
pub fn encode(fourcc: [u8; 4], width: u32, height: u32, hub_us: u64, rows: impl Iterator<Item = impl AsRef<[u8]>>) -> Vec<u8> {
    let row_bytes = width as usize * if &fourcc == b"UYVY" { 2 } else { 4 };
    let mut out = Vec::with_capacity(24 + row_bytes * height as usize);
    out.extend_from_slice(b"TNDI");
    out.extend_from_slice(&fourcc);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&hub_us.to_le_bytes());
    for r in rows {
        out.extend_from_slice(&r.as_ref()[..row_bytes]);
    }
    out
}

// ---- the NDI runtime, loaded by hand ----------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Source {
    name: *const c_char,
    url: *const c_char,
}

#[repr(C)]
struct FindCreate {
    show_local_sources: bool,
    groups: *const c_char,
    extra_ips: *const c_char,
}

#[repr(C)]
struct RecvCreateV3 {
    source: Source,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    name: *const c_char,
}

#[repr(C)]
struct VideoFrameV2 {
    xres: i32,
    yres: i32,
    fourcc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    aspect: f32,
    frame_format: i32,
    timecode: i64,
    data: *mut u8,
    line_stride: i32,
    metadata: *const c_char,
    timestamp: i64,
}

const COLOR_UYVY_RGBA: i32 = 3;
const BANDWIDTH_LOWEST: i32 = 0;
const BANDWIDTH_HIGHEST: i32 = 100;
const FRAME_VIDEO: i32 = 1;
const FRAME_ERROR: i32 = 4;

struct Lib {
    _lib: libloading::Library,
    initialize: unsafe extern "C" fn() -> bool,
    find_create: unsafe extern "C" fn(*const FindCreate) -> *mut c_void,
    find_wait: unsafe extern "C" fn(*mut c_void, u32) -> bool,
    find_sources: unsafe extern "C" fn(*mut c_void, *mut u32) -> *const Source,
    find_destroy: unsafe extern "C" fn(*mut c_void),
    recv_create: unsafe extern "C" fn(*const RecvCreateV3) -> *mut c_void,
    recv_capture: unsafe extern "C" fn(*mut c_void, *mut VideoFrameV2, *mut c_void, *mut c_void, u32) -> i32,
    recv_free_video: unsafe extern "C" fn(*mut c_void, *const VideoFrameV2),
}

fn candidates(explicit: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if !explicit.is_empty() {
        v.push(PathBuf::from(explicit));
    }
    for var in ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"] {
        if let Some(d) = std::env::var_os(var) {
            v.push(PathBuf::from(d).join("libndi.dylib"));
        }
    }
    for p in [
        "/usr/local/lib/libndi.dylib",
        "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
        "/Library/NDI Advanced SDK for Apple/lib/macOS/libndi_advanced.dylib",
        "/opt/homebrew/lib/libndi.dylib",
        "/usr/lib/libndi.so",
        "/usr/local/lib/libndi.so",
    ] {
        v.push(PathBuf::from(p));
    }
    v
}

impl Lib {
    fn load(explicit: &str) -> anyhow::Result<Lib> {
        let paths = candidates(explicit);
        let Some(path) = paths.iter().find(|p| p.exists()) else {
            bail!(
                "the NDI runtime (libndi.dylib) is not installed — get NDI Tools or the NDI SDK from https://ndi.video/tools/, \
                 or set [ndi] lib; looked in {}",
                paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            );
        };
        // SAFETY: libndi is a plain C library; the symbols' signatures are
        // the NDI SDK's (Processing.NDI.Find.h / Recv.h, v5 and v6).
        unsafe {
            let lib = libloading::Library::new(path).with_context(|| format!("loading {}", path.display()))?;
            macro_rules! sym {
                ($n:literal) => {
                    *lib.get(concat!($n, "\0").as_bytes()).with_context(|| format!("{} in {}", $n, path.display()))?
                };
            }
            let l = Lib {
                initialize: sym!("NDIlib_initialize"),
                find_create: sym!("NDIlib_find_create_v2"),
                find_wait: sym!("NDIlib_find_wait_for_sources"),
                find_sources: sym!("NDIlib_find_get_current_sources"),
                find_destroy: sym!("NDIlib_find_destroy"),
                recv_create: sym!("NDIlib_recv_create_v3"),
                recv_capture: sym!("NDIlib_recv_capture_v2"),
                recv_free_video: sym!("NDIlib_recv_free_video_v2"),
                _lib: lib,
            };
            if !(l.initialize)() {
                bail!("NDIlib_initialize failed (unsupported CPU?)");
            }
            log::info!("ndi: runtime {}", path.display());
            Ok(l)
        }
    }
}

/// A finder; owns the strings its sources point into.
struct Finder<'a> {
    lib: &'a Lib,
    raw: *mut c_void,
    _ips: Option<CString>,
}

impl<'a> Finder<'a> {
    fn new(lib: &'a Lib, extra_ips: &str) -> anyhow::Result<Finder<'a>> {
        let ips = (!extra_ips.is_empty()).then(|| CString::new(extra_ips)).transpose()?;
        let create = FindCreate {
            show_local_sources: true,
            groups: std::ptr::null(),
            extra_ips: ips.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        };
        // SAFETY: the settings outlive the call.
        let raw = unsafe { (lib.find_create)(&create) };
        if raw.is_null() {
            bail!("NDIlib_find_create_v2 failed");
        }
        Ok(Finder { lib, raw, _ips: ips })
    }

    /// The visible sources after waiting up to `wait`: (name, url) copies
    /// plus the raw entries (valid until the next call).
    fn sources(&self, wait: Duration) -> Vec<(String, Source)> {
        // SAFETY: the finder is live; the array is valid until the next call.
        unsafe {
            (self.lib.find_wait)(self.raw, wait.as_millis() as u32);
            let mut n = 0u32;
            let p = (self.lib.find_sources)(self.raw, &mut n);
            if p.is_null() {
                return Vec::new();
            }
            std::slice::from_raw_parts(p, n as usize)
                .iter()
                .map(|s| (CStr::from_ptr(s.name).to_string_lossy().into_owned(), *s))
                .collect()
        }
    }
}

impl Drop for Finder<'_> {
    fn drop(&mut self) {
        // SAFETY: created by find_create, destroyed once.
        unsafe { (self.lib.find_destroy)(self.raw) }
    }
}

/// `simply-tussen ndi`: the sources visible from here.
pub fn list(cfg: &NdiConfig, wait: Duration) -> anyhow::Result<Vec<String>> {
    let lib = Lib::load(&cfg.lib)?;
    let finder = Finder::new(&lib, &cfg.extra_ips)?;
    let deadline = std::time::Instant::now() + wait;
    let mut names = Vec::new();
    while std::time::Instant::now() < deadline {
        names = finder.sources(Duration::from_millis(500)).into_iter().map(|(n, _)| n).collect();
    }
    Ok(names)
}

/// Start the receiver thread: find the configured source, receive it,
/// publish every video frame to [`feed`]. Runs for the life of the hub.
pub fn start(cfg: NdiConfig) -> anyhow::Result<()> {
    let lib = Lib::load(&cfg.lib)?;
    std::thread::Builder::new().name("simply-tussen ndi".into()).spawn(move || {
        if let Err(e) = receive(&lib, &cfg) {
            log::warn!("ndi: {e:#}");
        }
    })?;
    Ok(())
}

fn receive(lib: &Lib, cfg: &NdiConfig) -> anyhow::Result<()> {
    let finder = Finder::new(lib, &cfg.extra_ips)?;
    let want = if cfg.source.is_empty() { "the first NDI source".to_owned() } else { format!("an NDI source matching {:?}", cfg.source) };
    log::info!("ndi: looking for {want}");
    let mut warned = false;
    let (name, source) = loop {
        let found = finder.sources(Duration::from_secs(1));
        if let Some(s) = found.iter().find(|(n, _)| n.contains(&cfg.source)) {
            break s.clone();
        }
        if !warned && !found.is_empty() {
            log::warn!("ndi: no source matches {:?}; visible: {}", cfg.source, found.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
            warned = true;
        }
    };
    let recv_name = CString::new("simply-tussen")?;
    let create = RecvCreateV3 {
        source,
        color_format: COLOR_UYVY_RGBA,
        bandwidth: if cfg.bandwidth == "lowest" { BANDWIDTH_LOWEST } else { BANDWIDTH_HIGHEST },
        allow_video_fields: false,
        name: recv_name.as_ptr(),
    };
    // SAFETY: `source` points into the finder's list, which is alive (no
    // further find call happens before this one returns).
    let recv = unsafe { (lib.recv_create)(&create) };
    if recv.is_null() {
        bail!("NDIlib_recv_create_v3 failed for {name}");
    }
    log::info!("ndi: receiving {name} → ws://127.0.0.1:<console>/ndi");
    let mut frames = 0u64;
    loop {
        // SAFETY: an all-zero frame is a valid out-parameter.
        let mut v: VideoFrameV2 = unsafe { std::mem::zeroed() };
        // SAFETY: recv is live; null audio/metadata = not wanted.
        let kind = unsafe { (lib.recv_capture)(recv, &mut v, std::ptr::null_mut(), std::ptr::null_mut(), 1000) };
        match kind {
            FRAME_VIDEO => {
                let hub_us = crate::console::now_us();
                let fourcc = v.fourcc.to_le_bytes();
                let (w, h, stride) = (v.xres.max(0) as usize, v.yres.max(0) as usize, v.line_stride.max(0) as usize);
                let bpp = if &fourcc == b"UYVY" { 2 } else { 4 };
                if !v.data.is_null() && stride >= w * bpp {
                    // SAFETY: NDI hands us h rows of `stride` bytes at data.
                    let data = unsafe { std::slice::from_raw_parts(v.data, stride * h) };
                    let frame = encode(fourcc, w as u32, h as u32, hub_us, data.chunks(stride));
                    feed().publish(frame);
                    if frames == 0 {
                        log::info!("ndi: first frame {w}x{h} {}", String::from_utf8_lossy(&fourcc));
                    }
                    frames += 1;
                }
                // SAFETY: the frame came from this receiver's capture.
                unsafe { (lib.recv_free_video)(recv, &v) };
            }
            FRAME_ERROR => {
                log::warn!("ndi: {name} disconnected; waiting for it");
                std::thread::sleep(Duration::from_secs(1));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_a_header_then_tight_rows() {
        // 2x2 UYVY with 8-byte padded rows: the padding is dropped.
        let rows = [[1u8, 2, 3, 4, 0, 0, 0, 0], [5, 6, 7, 8, 0, 0, 0, 0]];
        let f = encode(*b"UYVY", 2, 2, 42, rows.iter());
        assert_eq!(&f[..8], b"TNDIUYVY");
        assert_eq!(u32::from_le_bytes(f[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(f[12..16].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(f[16..24].try_into().unwrap()), 42);
        assert_eq!(&f[24..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn the_feed_hands_out_only_newer_frames() {
        let feed = Feed::new();
        assert!(feed.next(0, Duration::from_millis(1)).is_none());
        feed.publish(vec![1]);
        feed.publish(vec![2]);
        let (seq, f) = feed.next(0, Duration::from_millis(1)).unwrap();
        assert_eq!((seq, f.as_slice()), (2, &[2u8][..]));
        assert!(feed.next(seq, Duration::from_millis(1)).is_none());
    }

    #[test]
    fn ndi_is_off_unless_asked_for() {
        let c: crate::config::Config = toml::from_str("").unwrap();
        assert!(!c.ndi.enabled);
        let c: crate::config::Config = toml::from_str("[ndi]\nenabled = true\nsource = \"TD\"\n").unwrap();
        assert_eq!((c.ndi.enabled, c.ndi.source.as_str(), c.ndi.bandwidth.as_str()), (true, "TD", "highest"));
    }
}
