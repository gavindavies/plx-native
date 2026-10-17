//! **HDMI-CEC: the TV's own remote drives the app.** With the laptop on the TV's HDMI input, an LG
//! TV forwards its remote's buttons (arrows, OK, Back, the transport keys) over CEC — LG calls it
//! SIMPLINK — to the device it is showing. This module is that device: it claims a Playback
//! logical address on the kernel's CEC adapter (`/dev/cecN`, the Linux CEC framework), announces
//! itself as the active source so the TV switches to it and routes its keys here, answers the
//! handful of queries a TV makes of a playback device, and turns `<User Control Pressed>` /
//! `<User Control Released>` into the same synthesized key edges the remote FIFO produces
//! (`app::events::synth_cec_key`), so the ONE real key handler runs unchanged.
//!
//! The Magic Remote's pointer does not travel over CEC; its buttons and wheel-click do.
//!
//! **It needs a CEC adapter.** A PC's HDMI port does not wire the CEC line. The kernel exposes one
//! as `/dev/cecN` for a USB-CEC adapter (Pulse-Eight, `pulse8-cec` through `inputattach`), or for
//! a DisplayPort/USB-C to HDMI adapter that tunnels CEC over the DP AUX channel (`drm_dp_cec`).
//! With no device the app runs exactly as before. The user needs read/write access to the node
//! (the `video` group on Ubuntu).
//!
//! Environment: `PITV_CEC=0` turns it off; `PITV_CEC_DEVICE=/dev/cec1` picks the adapter (default:
//! the first `/dev/cecN` that opens).
//!
//! Threads: one `cec` thread owns the device, polls it and runs [`Follower`] and [`KeyTracker`];
//! the main loop drains the key edges it queued ([`Cec::drain`]) once a frame, beside the FIFO.
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use std::time::Instant;

use plx_base::eventlog::log;

// ---- the kernel's CEC uAPI (`linux/cec.h`) ----------------------------------------------------

mod uapi {
    use std::os::raw::c_ulong;

    pub const MAX_MSG_SIZE: usize = 16;
    pub const MAX_LOG_ADDRS: usize = 4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Msg {
        pub tx_ts: u64,
        pub rx_ts: u64,
        pub len: u32,
        pub timeout: u32,
        pub sequence: u32,
        pub flags: u32,
        pub msg: [u8; MAX_MSG_SIZE],
        pub reply: u8,
        pub rx_status: u8,
        pub tx_status: u8,
        pub tx_arb_lost_cnt: u8,
        pub tx_nack_cnt: u8,
        pub tx_low_drive_cnt: u8,
        pub tx_error_cnt: u8,
    }

    impl Msg {
        pub fn zeroed() -> Self {
            // SAFETY: plain integers and byte arrays; all-zero is a valid value.
            unsafe { std::mem::zeroed() }
        }
    }

    #[repr(C)]
    pub struct Caps {
        pub driver: [u8; 32],
        pub name: [u8; 32],
        pub available_log_addrs: u32,
        pub capabilities: u32,
        pub version: u32,
    }

    #[repr(C)]
    pub struct LogAddrs {
        pub log_addr: [u8; MAX_LOG_ADDRS],
        pub log_addr_mask: u16,
        pub cec_version: u8,
        pub num_log_addrs: u8,
        pub vendor_id: u32,
        pub flags: u32,
        pub osd_name: [u8; 15],
        pub primary_device_type: [u8; MAX_LOG_ADDRS],
        pub log_addr_type: [u8; MAX_LOG_ADDRS],
        pub all_device_types: [u8; MAX_LOG_ADDRS],
        pub features: [[u8; 12]; MAX_LOG_ADDRS],
    }

    #[repr(C)]
    pub struct Event {
        pub ts: u64,
        pub event: u32,
        pub flags: u32,
        /// `state_change`: `phys_addr`, `log_addr_mask`, `have_conn_info` as three `u16`s.
        pub raw: [u32; 16],
    }

    const fn iowr(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
        (dir << 30) | ((size as c_ulong) << 16) | ((b'a' as c_ulong) << 8) | nr
    }
    const W: c_ulong = 1;
    const R: c_ulong = 2;
    pub const ADAP_G_CAPS: c_ulong = iowr(R | W, 0, std::mem::size_of::<Caps>());
    pub const ADAP_G_LOG_ADDRS: c_ulong = iowr(R, 3, std::mem::size_of::<LogAddrs>());
    pub const ADAP_S_LOG_ADDRS: c_ulong = iowr(R | W, 4, std::mem::size_of::<LogAddrs>());
    pub const TRANSMIT: c_ulong = iowr(R | W, 5, std::mem::size_of::<Msg>());
    pub const RECEIVE: c_ulong = iowr(R | W, 6, std::mem::size_of::<Msg>());
    pub const DQEVENT: c_ulong = iowr(R | W, 7, std::mem::size_of::<Event>());
    pub const S_MODE: c_ulong = iowr(W, 9, std::mem::size_of::<u32>());

    pub const CAP_LOG_ADDRS: u32 = 1 << 1;
    pub const CAP_TRANSMIT: u32 = 1 << 2;
    pub const MODE_INITIATOR: u32 = 0x1;
    pub const MODE_FOLLOWER: u32 = 0x10;
    pub const MODE_EXCL_FOLLOWER: u32 = 0x20;
    pub const EVENT_STATE_CHANGE: u32 = 1;
    pub const EVENT_LOST_MSGS: u32 = 2;
    pub const LOG_ADDR_TYPE_PLAYBACK: u8 = 3;
    pub const PRIM_DEVTYPE_PLAYBACK: u8 = 4;
    pub const ALL_DEVTYPE_PLAYBACK: u8 = 0x10;
    pub const CEC_VERSION_1_4: u8 = 5;
    pub const VENDOR_ID_NONE: u32 = 0xffff_ffff;
}

// ---- the protocol: what the TV says and what a playback device answers -----------------------

/// CEC opcodes this module reads or writes (HDMI 1.4b, CEC table 8).
mod op {
    pub const FEATURE_ABORT: u8 = 0x00;
    pub const IMAGE_VIEW_ON: u8 = 0x04;
    pub const STANDBY: u8 = 0x36;
    pub const USER_CONTROL_PRESSED: u8 = 0x44;
    pub const USER_CONTROL_RELEASED: u8 = 0x45;
    pub const GIVE_OSD_NAME: u8 = 0x46;
    pub const ROUTING_CHANGE: u8 = 0x80;
    pub const ROUTING_INFORMATION: u8 = 0x81;
    pub const ACTIVE_SOURCE: u8 = 0x82;
    pub const GIVE_PHYSICAL_ADDR: u8 = 0x83;
    pub const REQUEST_ACTIVE_SOURCE: u8 = 0x85;
    pub const SET_STREAM_PATH: u8 = 0x86;
    pub const GIVE_DEVICE_VENDOR_ID: u8 = 0x8C;
    pub const MENU_REQUEST: u8 = 0x8D;
    pub const MENU_STATUS: u8 = 0x8E;
    pub const GIVE_DEVICE_POWER_STATUS: u8 = 0x8F;
    pub const REPORT_POWER_STATUS: u8 = 0x90;
    pub const INACTIVE_SOURCE: u8 = 0x9D;
    pub const GET_CEC_VERSION: u8 = 0x9F;
    pub const GIVE_FEATURES: u8 = 0xA5;
}

const TV: u8 = 0x0;
const BROADCAST: u8 = 0xF;

/// The app's name for a remote button — the remote FIFO's token, which `app::events` resolves to
/// the key the real handler expects — for a CEC `[UI Command]` (HDMI 1.4b, CEC table 27).
pub(crate) fn ui_key(code: u8) -> Option<&'static str> {
    Some(match code {
        0x00 => "ok", // Select
        0x01 => "up",
        0x02 => "down",
        0x03 => "left",
        0x04 => "right",
        0x0D => "back", // Exit: what an LG remote's Back sends
        0x30 => "chup",
        0x31 => "chdown",
        0x37 => "pageup",
        0x38 => "pagedown",
        0x44 | 0x60 => "play", // Play, Play Function
        0x45 | 0x64 => "stop", // Stop, Stop Function
        0x46 => "pause",
        0x48 => "rew",
        0x49 => "ff",
        0x61 => "playpause", // Pause-Play Function
        _ => return None,
    })
}

/// What one received message asks of us.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Response {
    /// Messages to transmit, header byte first.
    pub replies: Vec<Vec<u8>>,
    pub key: Option<KeyInput>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum KeyInput {
    Pressed(u8),
    Released,
}

/// A Playback device's side of the conversation, without the bus: our logical and physical
/// address and whether we are the source the TV shows.
pub(crate) struct Follower {
    pub la: u8,
    pub pa: u16,
    pub active: bool,
}

fn header(from: u8, to: u8) -> u8 {
    (from << 4) | (to & 0xF)
}

fn pa_bytes(pa: u16) -> [u8; 2] {
    pa.to_be_bytes()
}

fn pa_at(msg: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]))
}

impl Follower {
    /// `<Active Source>`, broadcast: "show me" — what makes the TV route its remote to us.
    pub fn active_source(&self) -> Vec<u8> {
        let [hi, lo] = pa_bytes(self.pa);
        vec![header(self.la, BROADCAST), op::ACTIVE_SOURCE, hi, lo]
    }

    /// One Touch Play: wake the TV's picture, then claim it.
    pub fn one_touch_play(&mut self) -> Vec<Vec<u8>> {
        self.active = true;
        vec![vec![header(self.la, TV), op::IMAGE_VIEW_ON], self.active_source()]
    }

    /// Handle one received message (`msg[0]` is the header).
    pub fn receive(&mut self, msg: &[u8]) -> Response {
        let mut r = Response::default();
        let (Some(&hdr), Some(&opcode)) = (msg.first(), msg.get(1)) else { return r };
        let (from, to) = (hdr >> 4, hdr & 0xF);
        let directed = to != BROADCAST;
        match opcode {
            op::USER_CONTROL_PRESSED => r.key = msg.get(2).map(|&c| KeyInput::Pressed(c)),
            op::USER_CONTROL_RELEASED => r.key = Some(KeyInput::Released),
            op::GIVE_DEVICE_POWER_STATUS if directed => {
                r.replies.push(vec![header(self.la, from), op::REPORT_POWER_STATUS, 0x00]); // on
            }
            // LG forwards its navigation keys only to a device whose menu is "activated".
            op::MENU_REQUEST if directed => r.replies.push(vec![header(self.la, from), op::MENU_STATUS, 0x00]),
            op::REQUEST_ACTIVE_SOURCE if self.active => r.replies.push(self.active_source()),
            op::SET_STREAM_PATH => {
                self.active = pa_at(msg, 2) == Some(self.pa);
                if self.active {
                    r.replies.push(self.active_source());
                }
            }
            op::ROUTING_CHANGE => {
                if pa_at(msg, 4) == Some(self.pa) {
                    self.active = true;
                    r.replies.push(self.active_source());
                } else if self.active {
                    self.active = false;
                }
            }
            op::ACTIVE_SOURCE => {
                if pa_at(msg, 2) != Some(self.pa) {
                    self.active = false;
                }
            }
            op::STANDBY => self.active = false,
            // Answered by the kernel's CEC core, or replies and notices that ask nothing back.
            op::FEATURE_ABORT
            | op::GIVE_OSD_NAME
            | op::GIVE_PHYSICAL_ADDR
            | op::GIVE_DEVICE_VENDOR_ID
            | op::GET_CEC_VERSION
            | op::GIVE_FEATURES
            | op::ROUTING_INFORMATION
            | op::INACTIVE_SOURCE
            | op::REPORT_POWER_STATUS
            | op::MENU_STATUS => {}
            // Anything else addressed to us is refused ("unrecognised opcode"), as the
            // specification requires, so the TV does not wait on an answer that never comes.
            _ if directed => r.replies.push(vec![header(self.la, from), op::FEATURE_ABORT, opcode, 0x00]),
            _ => {}
        }
        r
    }
}

// ---- held keys --------------------------------------------------------------------------------

/// A key edge for the main loop: the button's FIFO token and what happened to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct KeyEdge {
    pub token: &'static str,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EdgeKind {
    Down,
    Repeat,
    Up,
}

/// A follower assumes a key released when neither a repeat nor a release has come this long after
/// the last press (HDMI 1.4b CEC 13.13.3: the initiator repeats a held key at most 450 ms apart).
pub(crate) const RELEASE_AFTER_MS: u64 = 550;

/// Presses, repeats and releases from the bus turned into one Down, its Repeats and one Up —
/// including the Up a lost `<User Control Released>` never sent.
#[derive(Default)]
pub(crate) struct KeyTracker {
    held: Option<(&'static str, u64)>,
}

impl KeyTracker {
    pub fn input(&mut self, input: KeyInput, now_ms: u64, out: &mut Vec<KeyEdge>) {
        match input {
            KeyInput::Pressed(code) => {
                let Some(token) = ui_key(code) else {
                    log(&format!("cec: unmapped UI command 0x{code:02x}"));
                    self.release(out);
                    return;
                };
                match self.held {
                    Some((held, _)) if held == token => out.push(KeyEdge { token, kind: EdgeKind::Repeat }),
                    _ => {
                        self.release(out);
                        out.push(KeyEdge { token, kind: EdgeKind::Down });
                    }
                }
                self.held = Some((token, now_ms));
            }
            KeyInput::Released => self.release(out),
        }
    }

    /// Release a key whose repeats stopped without a release.
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<KeyEdge>) {
        if self.held.is_some_and(|(_, at)| now_ms.saturating_sub(at) >= RELEASE_AFTER_MS) {
            self.release(out);
        }
    }

    /// Milliseconds until [`tick`](Self::tick) would release the held key.
    pub fn due_in(&self, now_ms: u64) -> Option<u64> {
        self.held.map(|(_, at)| (at + RELEASE_AFTER_MS).saturating_sub(now_ms))
    }

    fn release(&mut self, out: &mut Vec<KeyEdge>) {
        if let Some((token, _)) = self.held.take() {
            out.push(KeyEdge { token, kind: EdgeKind::Up });
        }
    }
}

// ---- the device -------------------------------------------------------------------------------

/// The main loop's end of the CEC thread.
pub(crate) struct Cec {
    edges: mpsc::Receiver<KeyEdge>,
}

impl Cec {
    /// Open the adapter and start its thread. `None` (logged) when CEC is off or there is no
    /// adapter; the app then runs without it.
    pub fn start() -> Option<Cec> {
        if std::env::var("PITV_CEC").is_ok_and(|v| v == "0") {
            log("cec: off (PITV_CEC=0)");
            return None;
        }
        let (path, fd) = open_adapter()?;
        let (tx, rx) = mpsc::channel();
        let spawned = plx_base::task::spawn("cec", move || {
            let mut device = Device { fd, path };
            device.run(tx);
        });
        if spawned.is_none() {
            // SAFETY: the thread that would own it was never started.
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Cec { edges: rx })
    }

    /// Hand every key edge received since the last frame to `f`.
    pub fn drain(&self, mut f: impl FnMut(KeyEdge)) {
        while let Ok(edge) = self.edges.try_recv() {
            f(edge);
        }
    }
}

fn open_adapter() -> Option<(String, RawFd)> {
    let candidates: Vec<String> = match std::env::var("PITV_CEC_DEVICE") {
        Ok(p) if !p.trim().is_empty() => vec![p],
        _ => (0..10).map(|n| format!("/dev/cec{n}")).collect(),
    };
    let mut refused = Vec::new();
    for path in &candidates {
        let Ok(c) = std::ffi::CString::new(path.as_str()) else { continue };
        // SAFETY: a NUL-terminated path; the descriptor is owned by the returned pair.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd >= 0 {
            return Some((path.clone(), fd));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::NotFound {
            refused.push(format!("{path}: {err}"));
        }
    }
    if refused.is_empty() {
        log("cec: no CEC adapter (/dev/cecN); the TV's remote will not reach the app");
    } else {
        log(&format!("cec: could not open {} (is the user in the video group?)", refused.join(", ")));
    }
    None
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct Device {
    fd: RawFd,
    path: String,
}

impl Device {
    fn ioctl<T>(&self, request: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
        // SAFETY: `request` encodes `T`'s size and direction (`uapi`), and `arg` outlives the call.
        let r = unsafe { libc::ioctl(self.fd, request as _, arg as *mut T) };
        if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    fn transmit(&self, bytes: &[u8]) {
        let mut m = uapi::Msg::zeroed();
        let n = bytes.len().min(uapi::MAX_MSG_SIZE);
        m.msg[..n].copy_from_slice(&bytes[..n]);
        m.len = n as u32;
        if let Err(e) = self.ioctl(uapi::TRANSMIT, &mut m) {
            log(&format!("cec: transmit {:02x?} failed: {e}", &bytes[..n]));
        }
    }

    /// Become an initiator and the exclusive follower, and claim a Playback address unless the
    /// adapter is already configured (by `cec-ctl --playback`, say).
    fn configure(&self) -> bool {
        let mut caps: uapi::Caps = unsafe { std::mem::zeroed() };
        if let Err(e) = self.ioctl(uapi::ADAP_G_CAPS, &mut caps) {
            log(&format!("cec: {} is not a CEC adapter: {e}", self.path));
            return false;
        }
        let name = |b: &[u8]| String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(b)).into_owned();
        log(&format!("cec: {} — {} ({})", self.path, name(&caps.name), name(&caps.driver)));
        if caps.capabilities & uapi::CAP_TRANSMIT == 0 {
            log("cec: the adapter cannot transmit; nothing to do");
            return false;
        }
        let mut mode = uapi::MODE_INITIATOR | uapi::MODE_EXCL_FOLLOWER;
        if self.ioctl(uapi::S_MODE, &mut mode).is_err() {
            // Another program is the exclusive follower (cec-follower, libcec). Share instead.
            let mut shared = uapi::MODE_INITIATOR | uapi::MODE_FOLLOWER;
            if let Err(e) = self.ioctl(uapi::S_MODE, &mut shared) {
                log(&format!("cec: could not follow the bus: {e}"));
                return false;
            }
            log("cec: another program follows the bus too");
        }
        let mut las: uapi::LogAddrs = unsafe { std::mem::zeroed() };
        if let Err(e) = self.ioctl(uapi::ADAP_G_LOG_ADDRS, &mut las) {
            log(&format!("cec: could not read the logical addresses: {e}"));
            return false;
        }
        if las.num_log_addrs == 0 && caps.capabilities & uapi::CAP_LOG_ADDRS != 0 {
            let mut want: uapi::LogAddrs = unsafe { std::mem::zeroed() };
            want.cec_version = uapi::CEC_VERSION_1_4;
            want.num_log_addrs = 1;
            want.vendor_id = uapi::VENDOR_ID_NONE;
            want.osd_name[..4].copy_from_slice(b"PiTV");
            want.primary_device_type[0] = uapi::PRIM_DEVTYPE_PLAYBACK;
            want.log_addr_type[0] = uapi::LOG_ADDR_TYPE_PLAYBACK;
            want.all_device_types[0] = uapi::ALL_DEVTYPE_PLAYBACK;
            if let Err(e) = self.ioctl(uapi::ADAP_S_LOG_ADDRS, &mut want) {
                log(&format!("cec: could not claim a Playback address: {e}"));
                return false;
            }
            log("cec: claiming a Playback address as \"PiTV\"");
        }
        true
    }

    fn run(&mut self, keys: mpsc::Sender<KeyEdge>) {
        if !self.configure() {
            // SAFETY: this thread owns the descriptor.
            unsafe { libc::close(self.fd) };
            return;
        }
        let mut follower: Option<Follower> = None;
        let mut tracker = KeyTracker::default();
        let mut edges = Vec::new();
        loop {
            let timeout = tracker.due_in(now_ms()).map_or(-1, |ms| ms as i32);
            let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN | libc::POLLPRI, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                log(&format!("cec: poll failed, stopping: {e}"));
                break;
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                log("cec: the adapter went away");
                break;
            }
            if pfd.revents & libc::POLLPRI != 0 {
                self.events(&mut follower);
            }
            if pfd.revents & libc::POLLIN != 0 {
                self.messages(&mut follower, &mut tracker, &mut edges);
            }
            tracker.tick(now_ms(), &mut edges);
            for edge in edges.drain(..) {
                if keys.send(edge).is_err() {
                    return;
                }
                plx_machine::idle::invalidate();
            }
        }
        // SAFETY: this thread owns the descriptor.
        unsafe { libc::close(self.fd) };
    }

    /// Address changes: once a logical address is claimed on a valid physical address, ask the
    /// TV to show us.
    fn events(&self, follower: &mut Option<Follower>) {
        let mut ev: uapi::Event = unsafe { std::mem::zeroed() };
        while self.ioctl(uapi::DQEVENT, &mut ev).is_ok() {
            match ev.event {
                uapi::EVENT_STATE_CHANGE => {
                    let pa = (ev.raw[0] & 0xFFFF) as u16;
                    let mask = (ev.raw[0] >> 16) as u16;
                    if pa == 0xFFFF || mask == 0 {
                        if follower.take().is_some() || pa == 0xFFFF {
                            log("cec: not connected (no physical or logical address yet)");
                        }
                        continue;
                    }
                    let la = mask.trailing_zeros() as u8;
                    if follower.as_ref().is_some_and(|f| f.la == la && f.pa == pa) {
                        continue;
                    }
                    log(&format!("cec: logical address {la}, physical address {}", fmt_pa(pa)));
                    let mut f = Follower { la, pa, active: false };
                    for m in f.one_touch_play() {
                        self.transmit(&m);
                    }
                    *follower = Some(f);
                }
                uapi::EVENT_LOST_MSGS => log(&format!("cec: {} messages lost", ev.raw[0])),
                _ => {}
            }
        }
    }

    fn messages(&self, follower: &mut Option<Follower>, tracker: &mut KeyTracker, edges: &mut Vec<KeyEdge>) {
        let mut m = uapi::Msg::zeroed();
        while self.ioctl(uapi::RECEIVE, &mut m).is_ok() {
            // A non-blocking transmit's result comes back this way, carrying its sequence number.
            if m.sequence != 0 || m.len == 0 {
                continue;
            }
            let Some(f) = follower.as_mut() else { continue };
            let bytes = &m.msg[..(m.len as usize).min(uapi::MAX_MSG_SIZE)];
            let r = f.receive(bytes);
            for reply in &r.replies {
                self.transmit(reply);
            }
            if let Some(key) = r.key {
                tracker.input(key, now_ms(), edges);
            }
            m = uapi::Msg::zeroed();
        }
    }
}

fn fmt_pa(pa: u16) -> String {
    format!("{}.{}.{}.{}", pa >> 12, (pa >> 8) & 0xF, (pa >> 4) & 0xF, pa & 0xF)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct sizes and ioctl numbers `linux/cec.h` defines, so a layout slip cannot reach a
    /// device.
    #[test]
    fn the_uapi_matches_the_kernel_header() {
        assert_eq!(std::mem::size_of::<uapi::Msg>(), 56);
        assert_eq!(std::mem::size_of::<uapi::Caps>(), 76);
        assert_eq!(std::mem::size_of::<uapi::LogAddrs>(), 92);
        assert_eq!(std::mem::size_of::<uapi::Event>(), 80);
        assert_eq!(uapi::ADAP_G_CAPS, 0xc04c_6100);
        assert_eq!(uapi::ADAP_G_LOG_ADDRS, 0x805c_6103);
        assert_eq!(uapi::ADAP_S_LOG_ADDRS, 0xc05c_6104);
        assert_eq!(uapi::TRANSMIT, 0xc038_6105);
        assert_eq!(uapi::RECEIVE, 0xc038_6106);
        assert_eq!(uapi::DQEVENT, 0xc050_6107);
        assert_eq!(uapi::S_MODE, 0x4004_6109);
    }

    fn follower() -> Follower {
        Follower { la: 4, pa: 0x1000, active: false }
    }

    #[test]
    fn one_touch_play_wakes_the_tv_and_claims_it() {
        let mut f = follower();
        assert_eq!(f.one_touch_play(), vec![vec![0x40, 0x04], vec![0x4F, 0x82, 0x10, 0x00]]);
        assert!(f.active);
    }

    #[test]
    fn the_tvs_remote_buttons_become_key_inputs() {
        let mut f = follower();
        assert_eq!(f.receive(&[0x04, 0x44, 0x01]).key, Some(KeyInput::Pressed(0x01)));
        assert_eq!(f.receive(&[0x04, 0x45]).key, Some(KeyInput::Released));
        assert!(f.receive(&[0x04, 0x44, 0x01]).replies.is_empty());
    }

    #[test]
    fn the_tvs_queries_are_answered() {
        let mut f = follower();
        assert_eq!(f.receive(&[0x04, 0x8F]).replies, vec![vec![0x40, 0x90, 0x00]]);
        assert_eq!(f.receive(&[0x04, 0x8D, 0x02]).replies, vec![vec![0x40, 0x8E, 0x00]]);
        // Not the active source: a broadcast request goes unanswered…
        assert!(f.receive(&[0x0F, 0x85]).replies.is_empty());
        // …until the TV switches to our path.
        assert_eq!(f.receive(&[0x0F, 0x86, 0x10, 0x00]).replies, vec![vec![0x4F, 0x82, 0x10, 0x00]]);
        assert!(f.active);
        assert_eq!(f.receive(&[0x0F, 0x85]).replies, vec![vec![0x4F, 0x82, 0x10, 0x00]]);
    }

    #[test]
    fn another_source_taking_over_makes_us_inactive() {
        let mut f = follower();
        f.one_touch_play();
        f.receive(&[0x8F, 0x82, 0x20, 0x00]);
        assert!(!f.active);
        f.receive(&[0x0F, 0x80, 0x20, 0x00, 0x10, 0x00]);
        assert!(f.active, "a routing change to our address makes us the source again");
    }

    #[test]
    fn unknown_directed_messages_are_refused_and_broadcasts_are_not() {
        let mut f = follower();
        assert_eq!(f.receive(&[0x04, 0xA0, 0x00, 0xE0, 0x91]).replies, vec![vec![0x40, 0x00, 0xA0, 0x00]]);
        assert!(f.receive(&[0x0F, 0xA0, 0x00]).replies.is_empty());
        assert!(f.receive(&[0x04, 0x00, 0x8F, 0x00]).replies.is_empty(), "never abort an abort");
        assert!(f.receive(&[0x04]).replies.is_empty(), "a poll carries no opcode");
    }

    fn edges(t: &mut KeyTracker, steps: &[(u64, Option<KeyInput>)]) -> Vec<(&'static str, EdgeKind)> {
        let mut out = Vec::new();
        for &(at, input) in steps {
            match input {
                Some(i) => t.input(i, at, &mut out),
                None => t.tick(at, &mut out),
            }
        }
        out.into_iter().map(|e| (e.token, e.kind)).collect()
    }

    #[test]
    fn a_held_key_is_one_down_its_repeats_and_one_up() {
        let mut t = KeyTracker::default();
        let got = edges(&mut t, &[
            (0, Some(KeyInput::Pressed(0x00))),
            (400, Some(KeyInput::Pressed(0x00))),
            (800, Some(KeyInput::Pressed(0x00))),
            (900, Some(KeyInput::Released)),
        ]);
        assert_eq!(got, [("ok", EdgeKind::Down), ("ok", EdgeKind::Repeat), ("ok", EdgeKind::Repeat), ("ok", EdgeKind::Up)]);
    }

    #[test]
    fn a_lost_release_is_supplied_after_550_ms() {
        let mut t = KeyTracker::default();
        let got = edges(&mut t, &[(0, Some(KeyInput::Pressed(0x01))), (549, None)]);
        assert_eq!(got, [("up", EdgeKind::Down)]);
        assert_eq!(t.due_in(549), Some(1));
        let got = edges(&mut t, &[(550, None)]);
        assert_eq!(got, [("up", EdgeKind::Up)]);
        assert_eq!(t.due_in(600), None);
    }

    #[test]
    fn a_new_key_releases_the_one_held() {
        let mut t = KeyTracker::default();
        let got = edges(&mut t, &[(0, Some(KeyInput::Pressed(0x01))), (100, Some(KeyInput::Pressed(0x02)))]);
        assert_eq!(got, [("up", EdgeKind::Down), ("up", EdgeKind::Up), ("down", EdgeKind::Down)]);
    }

    #[test]
    fn an_lg_remotes_buttons_map_to_the_apps_keys() {
        assert_eq!(ui_key(0x00), Some("ok"));
        assert_eq!(ui_key(0x0D), Some("back"));
        assert_eq!(ui_key(0x61), Some("playpause"));
        assert_eq!(ui_key(0x41), None, "volume stays with the TV");
        // Every mapped button names a key the app's token table knows.
        for code in 0..=0xFF {
            if let Some(token) = ui_key(code) {
                assert!(crate::app::events::remote_token_key(token).is_some(), "{token}");
            }
        }
    }
}
