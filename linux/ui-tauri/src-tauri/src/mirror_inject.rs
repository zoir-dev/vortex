//! Laptop → phone REAL touch+key injection (scrcpy-style), bypassing MIUI's
//! `INJECT_EVENTS` block via a shell-UID uinput helper.
//!
//! `vortex_inject` (a tiny arm64 native binary, embedded below — source in
//! `android/inject/`) is pushed to the
//! phone over adb and launched as the **shell** user, where it creates a virtual
//! multi-touch touchscreen + keyboard via `/dev/uinput`. The laptop streams
//! `D/M/U/E/K` commands to it over an **abstract unix socket** tunneled by
//! `adb forward tcp:<port> localabstract:vortex_inject`. A socket (not adb-shell
//! stdin, which batches) is what keeps the stream low-latency and un-stuttered —
//! the same reason scrcpy uses one.
//!
//! Real `MotionEvent`s/`KeyEvent`s mean low latency, true multitouch and a real
//! keyboard — the only way to reach first-class control on a non-rooted MIUI
//! device that blocks the InputManager / `adb shell input` path. When adb is
//! unavailable, [`start`] returns false and the caller falls back to the
//! AccessibilityService path.

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

/// The embedded arm64 injector binary. Source: `android/inject/vortex_inject.c`,
/// rebuilt (and verified byte-identical) by `android/inject/build.sh`.
const BINARY: &[u8] = include_bytes!("../assets/vortex_inject");
const REMOTE_PATH: &str = "/data/local/tmp/vortex_inject";
const SOCKET_NAME: &str = "localabstract:vortex_inject";
/// Default host-side TCP port the adb tunnel forwards to the device socket.
/// Only a seed — [`start`] asks adb to allocate a free port (`tcp:0`) and
/// stores the real one in [`ACTIVE_PORT`], so a busy 28250 can't knock control
/// out to the accessibility fallback.
const LOCAL_PORT: u16 = 28250;

/// The host port adb actually bound for this session (set in [`start`]).
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(LOCAL_PORT);

static INJECT: Mutex<Option<Injector>> = Mutex::new(None);

/// Set while `stop()` is tearing the session down, so the writer's self-heal
/// (which fires on a socket error) doesn't race it and re-launch the injector
/// just after the user closed the mirror.
static STOPPING: AtomicBool = AtomicBool::new(false);

enum Cmd {
    Line(String),
    Quit,
}

struct Injector {
    child: Child,
    /// Commands are queued here (never blocks the caller) and drained by a
    /// dedicated socket-writer thread — so the GStreamer navigation thread can
    /// NEVER block on socket I/O (which froze the whole pipeline).
    tx: Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
}

/// Which device `adb` should talk to, remembered between calls.
static ADB_SERIAL: Mutex<Option<String>> = Mutex::new(None);

/// The port `adb tcpip` puts the phone's daemon on, and [`redial`]'s last resort.
const WIRELESS_PORT: u16 = 5555;

/// Port the most recent network transport was actually attached on.
///
/// `adb tcpip` always lands on [`WIRELESS_PORT`], but Android 11+ *Wireless
/// debugging* — the only way to run adb with **USB debugging switched off**,
/// which many banking/DRM apps insist on — picks a RANDOM port and keeps it for
/// as long as the toggle stays on. Assuming 5555 there means [`redial`] can
/// never get back on after a Wi-Fi roam or a suspend: `scan_transports` finds
/// nothing, the redial dials a port nobody is listening on, and Universal
/// Control arms with no cursor ever appearing on the phone.
///
/// Persisted, because the phone keeps that port across our own restarts.
static LAST_ADB_PORT: Mutex<Option<u16>> = Mutex::new(None);

fn adb_port_path() -> Option<std::path::PathBuf> {
    crate::peer_cache::peer_file("last_adb_port")
}

/// Note the port a network transport is attached on, in memory and on disk.
fn remember_adb_port(port: u16) {
    let changed = {
        let mut g = LAST_ADB_PORT.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *g != Some(port);
        *g = Some(port);
        changed
    };
    if changed {
        if let Some(p) = adb_port_path() {
            let _ = vortex_l3_daemon::core::fs_private::write_private(
                &p,
                port.to_string().as_bytes(),
            );
        }
        tracing::debug!(port, "mirror inject: remembered wireless adb port");
    }
}

/// Ports [`redial`] should try, best guess first and never duplicated.
///
/// The remembered port comes first; 5555 stays as a fallback so a stale
/// remembered port (phone re-paired, Wireless debugging toggled off and the
/// legacy `adb tcpip` used instead) still recovers on the same pass.
fn redial_ports() -> Vec<u16> {
    let remembered = LAST_ADB_PORT
        .lock()
        .ok()
        .and_then(|g| *g)
        .or_else(|| {
            adb_port_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<u16>().ok())
        });
    redial_port_order(remembered)
}

/// The pure half of [`redial_ports`], split out so the ordering is testable
/// without a global or the filesystem in the way.
fn redial_port_order(remembered: Option<u16>) -> Vec<u16> {
    match remembered {
        Some(p) if p != WIRELESS_PORT => vec![p, WIRELESS_PORT],
        Some(p) => vec![p],
        None => vec![WIRELESS_PORT],
    }
}

/// Floor between redial attempts: `adb connect` blocks for about a second when
/// nothing answers, and [`adb_serial`] sits in front of every injector call.
const REDIAL_COOLDOWN: Duration = Duration::from_secs(10);

/// When the last redial was attempted, successful or not.
static LAST_REDIAL: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Split `adb devices` into (USB serial, network serial). A network transport
/// is "host:port"; USB serials have no colon.
fn transports() -> (Option<String>, Option<String>) {
    let Ok(out) = Command::new("adb").arg("devices").output() else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut usb, mut net) = (None, None);
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        let (Some(serial), Some("device")) = (it.next(), it.next()) else {
            continue;
        };
        let slot = if serial.contains(':') { &mut net } else { &mut usb };
        slot.get_or_insert_with(|| serial.to_string());
    }
    (usb, net)
}

/// Read the attached transports and pick one, USB first.
fn scan_transports() -> Option<String> {
    let (usb, net) = transports();
    // Whatever port the network transport is on is the one worth redialling —
    // `rsplit` also handles the bracketed IPv6 form (`[::1]:37129`).
    if let Some(port) = net
        .as_deref()
        .and_then(|n| n.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok())
    {
        remember_adb_port(port);
    }
    usb.or(net)
}

/// Did WE put the phone's adbd on TCP? Only then may we put it back — a port
/// the user opened for their own work is theirs to close.
static WE_OPENED_TCPIP: AtomicBool = AtomicBool::new(false);

fn tcpip_marker_path() -> Option<std::path::PathBuf> {
    let mut p = std::path::PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/opened_adb_tcpip");
    Some(p)
}

/// Bootstrap wireless adb from a cable that is plugged in RIGHT NOW.
///
/// On Android 10 there is no *Wireless debugging* pairing flow — the only way
/// to reach adbd over Wi-Fi is legacy `adb tcpip 5555`, which needs an existing
/// transport to issue it on. So the useful moment is while the cable is still
/// in: run it then, and Universal Control keeps working after the user unplugs.
///
/// The port this opens is the reason [`close_wireless_adb`] exists. It listens
/// on every interface for the rest of the phone's uptime, and although adbd
/// still enforces its RSA key check (an unknown host gets the "Allow USB
/// debugging?" prompt, not a shell), an open port on a café network is not
/// something to leave behind. So we open it only while Universal Control is on,
/// and close it again the moment it is switched off — see [`close_wireless_adb`].
///
/// Returns true when a wireless transport is available afterwards.
pub fn ensure_wireless_adb() -> bool {
    let (usb, net) = transports();
    if net.is_some() {
        return true; // already reachable without the cable
    }
    let Some(usb) = usb else { return false };
    tracing::info!("mirror inject: enabling wireless adb over the cable (adb tcpip {WIRELESS_PORT})");
    let mut cmd = if have_timeout_bin() {
        let mut c = Command::new("timeout");
        c.args(["--kill-after=1", ADB_TIMEOUT_SECS, "adb"]);
        c
    } else {
        Command::new("adb")
    };
    let ok = cmd
        .args(["-s", &usb, "tcpip", &WIRELESS_PORT.to_string()])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        tracing::warn!("mirror inject: `adb tcpip` failed — staying on the cable");
        return false;
    }
    WE_OPENED_TCPIP.store(true, Ordering::SeqCst);
    if let Some(p) = tcpip_marker_path() {
        let _ = vortex_l3_daemon::core::fs_private::write_private(&p, b"1");
    }
    remember_adb_port(WIRELESS_PORT);
    // adbd restarts on the far side; the old USB transport goes away and the
    // listener needs a moment before it answers.
    std::thread::sleep(Duration::from_millis(1200));
    // A fresh bootstrap must not be blocked by the redial cooldown.
    if let Ok(mut last) = LAST_REDIAL.lock() {
        *last = None;
    }
    forget_adb_serial();
    redial()
}

/// Put the phone's adbd back on USB-only, closing the TCP port.
///
/// Called when Universal Control is switched off and on app shutdown, so the
/// port is open only while the one feature that needs it is enabled. A crash
/// can still leave it open, which is what the on-disk marker is for: the next
/// launch sees it and closes the port if Universal Control is off.
pub fn close_wireless_adb() {
    let ours = WE_OPENED_TCPIP.swap(false, Ordering::SeqCst)
        || tcpip_marker_path().is_some_and(|p| p.exists());
    if !ours {
        return;
    }
    if let Some(p) = tcpip_marker_path() {
        let _ = std::fs::remove_file(p);
    }
    // Issue it on whatever transport answers — after an unplug that is the
    // network one, and `adb usb` over TCP is exactly how the port gets closed
    // remotely. With no transport at all there is nothing to do; the port dies
    // with the phone's next reboot.
    if scan_transports().is_none() {
        tracing::debug!("mirror inject: no transport to close wireless adb on");
        return;
    }
    if adb(&["usb"]) {
        tracing::info!("mirror inject: wireless adb closed (adbd back to USB-only)");
    } else {
        tracing::warn!("mirror inject: could not close wireless adb");
    }
    forget_adb_serial();
}

/// Dial the phone's wireless adb back up.
///
/// `adb tcpip 5555` survives on the PHONE until it reboots, but the host's half
/// of the connection does not: a Wi-Fi roam, a suspend, an `adb kill-server`, or
/// simply unplugging the cable after a USB session leaves zero transports
/// attached. From then on Universal Control arms normally and the cursor just
/// never appears on the phone, with nothing on either screen to say why — which
/// is exactly how it failed in practice.
///
/// The phone's current address is already tracked for the LAN transport (it
/// rides every AppState push over BLE as well), so getting back on costs one
/// call and no cable.
fn redial() -> bool {
    {
        let mut last = LAST_REDIAL.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < REDIAL_COOLDOWN) {
            return false;
        }
        *last = Some(std::time::Instant::now());
    }
    let Some(ip) = *crate::lan::LAST_GOOD_PEER_IP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    else {
        return false;
    };
    for port in redial_ports() {
        let target = format!("{ip}:{port}");
        // `adb connect` exits 0 even when it fails ("failed to connect to …"), so
        // the stdout text is the only honest signal. "already connected to" counts.
        //
        // Timed out like every other adb call, and for the sharpest reason: this
        // is the one measured blocking for 100+ seconds against a phone asleep on
        // Wi-Fi — and with several ports to try, an untimed call would multiply
        // that by the number of candidates.
        let mut cmd = if have_timeout_bin() {
            let mut c = Command::new("timeout");
            c.args(["--kill-after=1", ADB_TIMEOUT_SECS, "adb"]);
            c
        } else {
            Command::new("adb")
        };
        let ok = cmd
            .args(["connect", &target])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("connected to"));
        if ok {
            // Also covers succeeding on the fallback: that port is now the one
            // to try first next time.
            remember_adb_port(port);
            tracing::info!("mirror inject: wireless adb redialled at {target}");
            return true;
        }
        tracing::debug!("mirror inject: no answer on {target}");
    }
    tracing::debug!(
        "mirror inject: wireless adb unreachable — needs `adb tcpip 5555` once, or \
         Wireless debugging paired (Android 11+, works with USB debugging OFF)"
    );
    false
}

/// Pick the phone to address, and pin every adb call to it with `-s`.
///
/// Plain `adb` fails outright — "more than one device/emulator" — as soon as
/// two transports are attached, which is the ordinary state the moment you plug
/// a wirelessly-connected phone in to charge. That would take injector startup,
/// display size and rotation down with it, i.e. Universal Control and mirror
/// control both. USB wins when both are present: same phone, lower latency.
fn adb_serial() -> Option<String> {
    if let Some(s) = ADB_SERIAL.lock().ok()?.clone() {
        return Some(s);
    }
    // Nothing attached is not the end of it: the phone is very likely still
    // listening on the network and only the host forgot about it.
    let pick = match scan_transports() {
        Some(s) => s,
        None if redial() => scan_transports()?,
        None => return None,
    };
    *ADB_SERIAL.lock().ok()? = Some(pick.clone());
    tracing::debug!("mirror inject: adb target {pick}");
    Some(pick)
}

/// Drop the cached target so the next call re-picks — the cable was plugged or
/// pulled, or the device dropped off the network.
fn forget_adb_serial() {
    if let Ok(mut g) = ADB_SERIAL.lock() {
        *g = None;
    }
}

/// Seconds any single adb call may take before it is killed.
///
/// adb has no timeout of its own, and the failure that matters is not slow but
/// INFINITE: `adb connect <ip>` to a phone that is asleep on Wi-Fi leaves the
/// SYN unanswered, and a measured redial to a stale address blocked for over
/// 100 seconds. `adb shell` against a transport that has silently gone away can
/// hang longer still. Six seconds is far above a healthy call (measured 5-93 ms
/// on this deployment) and far below anything a person would sit through.
const ADB_TIMEOUT_SECS: &str = "6";

/// Is coreutils `timeout` available to wrap adb in? Resolved once.
fn have_timeout_bin() -> bool {
    static HAVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAVE.get_or_init(|| {
        Command::new("timeout")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// `adb …`, wrapped in a hard timeout so no adb call can ever hang a caller.
/// Falls back to a bare invocation where coreutils `timeout` is missing.
fn adb_command(args: &[&str]) -> Command {
    let serial = adb_serial();
    let mut cmd = if have_timeout_bin() {
        let mut c = Command::new("timeout");
        // --kill-after: adb ignoring TERM must not survive as a zombie holding
        // the transport we are about to re-scan.
        c.args(["--kill-after=1", ADB_TIMEOUT_SECS, "adb"]);
        c
    } else {
        Command::new("adb")
    };
    if let Some(s) = serial {
        cmd.arg("-s").arg(s);
    }
    cmd.args(args);
    cmd
}

/// `adb …` against an EXPLICIT serial, bypassing the cached one — for cleaning
/// up state on a transport that is not the one we would otherwise address.
fn adb_raw(args: &[&str]) -> bool {
    let mut cmd = if have_timeout_bin() {
        let mut c = Command::new("timeout");
        c.args(["--kill-after=1", ADB_TIMEOUT_SECS, "adb"]);
        c
    } else {
        Command::new("adb")
    };
    cmd.args(args);
    cmd.output().is_ok_and(|o| o.status.success())
}

fn adb(args: &[&str]) -> bool {
    match adb_command(args).output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            // Surface WHY (device offline/unauthorized, "Text file busy", no
            // device…) — this used to fail silently and drop to accessibility.
            tracing::debug!(
                ?args,
                "adb command failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            forget_adb_serial();
            false
        }
        Err(e) => {
            tracing::debug!(?args, "adb not runnable: {e}");
            forget_adb_serial();
            false
        }
    }
}

/// The phone's display size in pixels, via `adb shell wm size`. Universal
/// Control needs it to know where the phone's cursor actually is: we only ever
/// send relative deltas, so without the bounds we cannot tell "at the edge"
/// from "somewhere in the middle".
static CACHED_SIZE: Mutex<(i32, i32)> = Mutex::new((1080, 2340));

pub(crate) fn display_size() -> Option<(i32, i32)> {
    if let Some(out) = adb_capture(&["shell", "wm", "size"]) {
        if let Some(pick) = out
            .lines()
            .find(|l| l.contains("Override size:"))
            .or_else(|| out.lines().find(|l| l.contains("Physical size:")))
        {
            if let Some((w, h)) = pick.rsplit_once(':').and_then(|(_, s)| s.trim().split_once('x')) {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    if w > 0 && h > 0 {
                        if let Ok(mut g) = CACHED_SIZE.lock() {
                            *g = (w, h);
                        }
                        return Some((w, h));
                    }
                }
            }
        }
    }
    let cached = CACHED_SIZE.lock().map(|g| *g).unwrap_or((1080, 2340));
    Some(cached)
}

/// The phone's current display rotation as a quarter-turn count (0–3).
///
/// [`display_size`] reports PHYSICAL bounds, but Android clamps the pointer to
/// the *rotated* display — so in landscape the two are transposed, and anything
/// reasoning about screen edges is wrong by more than half a screen without
/// this. Read from `dumpsys display` rather than `settings get system
/// user_rotation`, which only reflects the manual setting and lies whenever
/// auto-rotate is on.
/// Last rotation we managed to read. See [`rotation_cached`].
static ROTATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// A refresh is in flight; more requests are pointless until it lands.
static ROTATION_BUSY: AtomicBool = AtomicBool::new(false);

/// The phone's rotation as of the last refresh. NEVER blocks.
///
/// [`rotation`] runs a full `dumpsys display` on the phone and carries the
/// output back over adb — hundreds of milliseconds on a good day, and seconds
/// over a wireless transport. Universal Control used to call it at the moment of
/// the crossing, on the capture thread, while the compositor was holding the
/// pointer still: the cursor stuck to the screen edge for as long as adb took.
/// The rotation is also the least urgent thing in that path — it changes when
/// the user turns the phone over, not between one frame and the next.
pub(crate) fn rotation_cached() -> u32 {
    ROTATION.load(Ordering::Relaxed)
}

/// Update [`rotation_cached`] in the background. Cheap to call often.
pub(crate) fn refresh_rotation() {
    if ROTATION_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Some(r) = rotation() {
            ROTATION.store(r, Ordering::Relaxed);
        }
        ROTATION_BUSY.store(false, Ordering::SeqCst);
    });
}

/// Android's pointer-acceleration curve. See [`pointer_curve`].
///
/// Android does not move its cursor by the delta a mouse reports; it multiplies
/// it by a factor that grows with how fast the mouse is going. Anything that
/// wants the cursor to end up somewhere specific — or that keeps its own idea of
/// where the cursor is — has to account for that, so the curve is modelled here
/// rather than guessed at.
#[derive(Clone, Copy)]
pub(crate) struct PointerCurve {
    /// Flat multiplier, from the user's pointer-speed setting.
    pub scale: f32,
    /// Below this speed (px/s) a delta passes through untouched…
    pub low: f32,
    /// …at and above this one it is multiplied by `accel`; between the two the
    /// factor ramps linearly.
    pub high: f32,
    pub accel: f32,
}

impl PointerCurve {
    /// Android's own defaults, and what every phone measured so far reports.
    /// Used until [`refresh_pointer_curve`] has heard back from this one.
    const DEFAULT: Self = Self { scale: 1.0, low: 500.0, high: 3000.0, accel: 3.0 };

    /// The factor a delta gets at saturation — which is what a delta written
    /// back-to-back with another one always gets, since zero elapsed time reads
    /// as infinite speed. Parking the cursor (slam, then step out) is exactly
    /// that pair, and measuring it on a Redmi 9C confirms the model: ask the
    /// step for 100, get 300; ask for 200, get 600. Both axes, exactly.
    pub fn saturated(&self) -> f32 {
        self.scale * self.accel
    }

    /// How many pixels to send for `want` pixels of travel to actually happen,
    /// given that the previous delta went out `dt` seconds ago — the curve,
    /// inverted.
    ///
    /// Android sees speed `w = sent/dt × scale` and applies `scale × f(w)`, so
    /// the travel is `w × dt × f(w)`. Asking for `q = want/dt` therefore means
    /// solving `w·f(w) = q`, which is linear at both ends of the curve and a
    /// quadratic on the ramp between them.
    pub fn undo(&self, want: f32, dt: f32) -> f32 {
        if want <= 0.0 || dt <= 0.0 || self.scale <= 0.0 {
            return want;
        }
        let q = want / dt;
        let k = if self.high > self.low {
            (self.accel - 1.0) / (self.high - self.low)
        } else {
            0.0
        };
        let w = if q <= self.low || k <= 0.0 {
            q
        } else if q >= self.high * self.accel {
            q / self.accel
        } else {
            let b = 1.0 - k * self.low;
            (-b + (b * b + 4.0 * k * q).sqrt()) / (2.0 * k)
        };
        w * dt / self.scale
    }
}

/// The curve as of the last refresh, in `[scale, low, high, accel]` order. Zero
/// = never read; see [`pointer_curve`].
static POINTER_CURVE: [std::sync::atomic::AtomicU32; 4] = [
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
];

/// The phone's [`PointerCurve`], or Android's defaults until it has been read.
///
/// NEVER blocks: crossings happen while the compositor is holding the laptop
/// pointer still, and an adb round-trip there is felt as the cursor sticking to
/// the screen edge.
pub(crate) fn pointer_curve() -> PointerCurve {
    let f = |i: usize| f32::from_bits(POINTER_CURVE[i].load(Ordering::Relaxed));
    let (scale, low, high, accel) = (f(0), f(1), f(2), f(3));
    if scale <= 0.0 || accel < 1.0 || high <= low {
        return PointerCurve::DEFAULT;
    }
    PointerCurve { scale, low, high, accel }
}

/// Ask the phone for its real [`pointer_curve`], in the background.
pub(crate) fn refresh_pointer_curve() {
    std::thread::spawn(|| {
        if let Some(c) = read_pointer_curve() {
            for (slot, v) in POINTER_CURVE.iter().zip([c.scale, c.low, c.high, c.accel]) {
                slot.store(v.to_bits(), Ordering::Relaxed);
            }
            tracing::info!(
                "mirror inject: phone pointer curve scale={:.3} {:.0}→{:.0}px/s accel={:.3} \
                 (saturated {:.3}×)",
                c.scale,
                c.low,
                c.high,
                c.accel,
                c.saturated()
            );
        }
    });
}

/// Parse `PointerVelocityControlParameters: scale=1.000, lowThreshold=500.000,
/// highThreshold=3000.000, acceleration=3.000`.
fn read_pointer_curve() -> Option<PointerCurve> {
    let out = adb_capture(&["shell", "dumpsys input | grep -m1 PointerVelocityControl"])?;
    let field = |name: &str| -> Option<f32> {
        out.split(&format!("{name}="))
            .nth(1)?
            .split(|c: char| c != '.' && !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    let c = PointerCurve {
        scale: field("scale")?,
        low: field("lowThreshold")?,
        high: field("highThreshold")?,
        accel: field("acceleration")?,
    };
    (c.scale > 0.0 && c.accel >= 1.0 && c.high > c.low && c.low >= 0.0).then_some(c)
}

pub(crate) fn rotation() -> Option<u32> {
    // grep on the device: `dumpsys display` is large and we want one line.
    let out = adb_capture(&["shell", "dumpsys display | grep mCurrentOrientation"])?;
    let line = out.lines().find(|l| l.contains("mCurrentOrientation"))?;
    line.rsplit_once('=')?.1.trim().parse().ok()
}

/// Like [`adb`] but returns trimmed stdout on success — used for
/// `adb forward tcp:0 …`, which prints the host port adb allocated.
fn adb_capture(args: &[&str]) -> Option<String> {
    let out = adb_command(args).output().ok()?;
    if !out.status.success() {
        tracing::debug!(?args, "adb (capture) failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        forget_adb_serial();
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Push the embedded injector to the phone (overwrites any stale copy) and make
/// it executable. Returns false if adb isn't available / there's no device.
fn ensure_pushed() -> bool {
    let tmp = std::env::temp_dir().join("vortex_inject");
    if std::fs::write(&tmp, BINARY).is_err() {
        tracing::warn!("mirror inject: couldn't stage injector to temp");
        return false;
    }
    let Some(tmp_str) = tmp.to_str() else { return false };
    // Kill any still-running injector FIRST and drop the old file: a live copy
    // holds /data/local/tmp/vortex_inject open, so `adb push` fails with "Text
    // file busy" — which silently dropped control to the accessibility fallback
    // (the user's "can't control" / "froze"). Doing this before the push fixes
    // it; the forward/launch below re-creates a clean injector.
    let _ = adb(&["shell", "pkill", "-f", "vortex_inject"]);
    let _ = adb(&["shell", "rm", "-f", REMOTE_PATH]);
    if !adb(&["push", tmp_str, REMOTE_PATH]) {
        tracing::warn!("mirror inject: adb push failed (device offline / unauthorized?)");
        return false;
    }
    adb(&["shell", "chmod", "755", REMOTE_PATH]);
    true
}

/// Whether the injector can inject TOUCH on this phone.
///
/// It cannot everywhere. Touch needs `/dev/uinput`, and stock Android does not
/// give shell that node — the injector falls back to `/dev/uhid`, which carries
/// a cursor and a keyboard but has no digitizer. Callers that draw fingers
/// (mirror taps, the trackpad's drawn-finger scroll) have to know, or they spend
/// the session writing into a device that was never created.
static TOUCH_OK: AtomicBool = AtomicBool::new(false);

/// See [`TOUCH_OK`]. False until an injector has been started.
pub fn touch_available() -> bool {
    TOUCH_OK.load(Ordering::SeqCst)
}

/// Whether the phone shows its own keyboard even with a hardware one attached.
///
/// This decides how long the injected keyboard may live. Attached permanently it
/// costs one "Configure physical keyboard" notification per session instead of
/// one per crossing — but on a phone where this is off, it also costs the phone
/// its on-screen keyboard for the whole time, which is far worse. The setting is
/// off by default in Android, so this must be asked, not assumed; MIUI refuses
/// shell the permission to WRITE it, but reading is allowed.
fn soft_keyboard_survives_hardware() -> bool {
    let on = adb_capture(&["shell", "settings", "get", "secure", "show_ime_with_hard_keyboard"])
        .is_some_and(|v| v.trim() == "1");
    tracing::info!(
        "mirror inject: phone keeps its on-screen keyboard with a hardware one: {on}"
    );
    on
}

/// Start the real-touch injector for a mirror session. Replaces any prior one.
/// Returns false (→ accessibility fallback) when adb/device is unavailable.
pub fn start() -> bool {
    stop();
    // stop() raised STOPPING while tearing down; clear it now that we're
    // (re)starting so the new writer's self-heal can run.
    STOPPING.store(false, Ordering::SeqCst);
    // ensure_pushed() already pkills any stale injector before pushing.
    if !ensure_pushed() {
        tracing::warn!("mirror inject: adb push failed — using accessibility fallback");
        return false;
    }
    // Tunnel the device's abstract socket to a host TCP port. Clear every
    // forward of OURS first, then let adb allocate a FREE port (tcp:0 → it
    // prints the chosen port) instead of a fixed 28250 that might be taken.
    remove_our_forwards();
    let port = match adb_capture(&["forward", "tcp:0", SOCKET_NAME])
        .and_then(|s| s.parse::<u16>().ok())
    {
        Some(p) => p,
        None => {
            tracing::warn!("mirror inject: adb forward failed — accessibility fallback");
            return false;
        }
    };
    ACTIVE_PORT.store(port, Ordering::SeqCst);
    // Which backend the injector will land on, asked the same way it will: the
    // touchscreen is the device only uinput can make.
    let touch = adb_capture(&["shell", "test -w /dev/uinput && echo yes || echo no"])
        .is_some_and(|v| v.trim() == "yes");
    TOUCH_OK.store(touch, Ordering::SeqCst);
    if !touch {
        tracing::info!(
            "mirror inject: no /dev/uinput for shell — cursor and keyboard over UHID, \
             no touch injection"
        );
    }
    // Launch the injector DETACHED from the shell session that starts it.
    //
    // It used to run as a plain foreground `adb shell`, with the local child
    // process as its lifetime handle. But adbd kills every shell on a
    // connection when that connection goes away, and over a WIRELESS transport
    // the host connection is re-established regularly — the phone's own log
    // shows it plainly:
    //
    //     BlockingConnectionAdapter(host-14): destructed
    //     host-8 : read thread spawning
    //
    // Each of those took the injector with it. The tunnel stayed installed, so
    // adb went on accepting connections and closing them, and Universal Control
    // looked armed while nothing reached the phone. `nohup … &` reparents the
    // injector to init, so it now survives its launching shell — and the
    // control socket is the only thing that has to be re-established.
    //
    // The local child no longer owns its life, so `stop()` pkills it by name.
    let keep = if soft_keyboard_survives_hardware() { " --keep-keys" } else { "" };
    let launch = format!("nohup {REMOTE_PATH}{keep} >/dev/null 2>&1 </dev/null &");
    let spawn = adb_command(&["shell", &launch])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match spawn {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("mirror inject: spawn failed: {e} — accessibility fallback");
            return false;
        }
    };
    let Some(stream) = connect_socket() else {
        tracing::warn!("mirror inject: socket connect failed — accessibility fallback");
        let mut child = child;
        let _ = child.kill();
        return false;
    };
    // Dedicated writer thread: owns the socket, drains the queue. The producer
    // (GStreamer thread) only ever does a non-blocking channel send.
    let (tx, rx) = mpsc::channel::<Cmd>();
    let worker = std::thread::spawn(move || writer_loop(stream, rx));
    if let Ok(mut g) = INJECT.lock() {
        *g = Some(Injector { child, tx, worker: Some(worker) });
    }
    // Warm before the first crossing needs it — reading it then would stall the
    // pointer against the screen edge for as long as adb takes.
    refresh_pointer_curve();
    tracing::info!("mirror inject: uinput injector connected (real-touch, scrcpy-style)");
    true
}

/// Connect to the adb-forwarded injector socket, retrying while the device-side
/// helper finishes binding (it needs ~700ms to set up the uinput device).
/// Connect to the tunnelled control socket, and prove something is behind it.
///
/// A TCP connect alone proves nothing: with the tunnel installed but the
/// injector gone, adb accepts the connection and then closes it, so `start()`
/// would report success and Universal Control would arm with nothing at the far
/// end. Two writes a moment apart settle it — the first can land in the buffer
/// of a doomed socket, the second cannot, because the reset has arrived by then.
fn connect_socket() -> Option<TcpStream> {
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(80));
        let Ok(s) = TcpStream::connect(("127.0.0.1", ACTIVE_PORT.load(Ordering::SeqCst))) else {
            continue;
        };
        let _ = s.set_nodelay(true);
        let mut s = s;
        // An EMPTY line, for the same reason the health check uses one: `V 0`
        // destroys the phone's mouse device, and a writer reconnect can happen
        // while the cursor is on the phone, which would delete it mid-session.
        // The injector matches on the first character, so an empty line is no
        // command at all — and it still writes a byte, which is all this needs.
        if !write_line(&mut s, "") {
            continue;
        }
        std::thread::sleep(Duration::from_millis(60));
        if !write_line(&mut s, "") {
            continue;
        }
        return Some(s);
    }
    None
}

fn write_line(stream: &mut TcpStream, line: &str) -> bool {
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .is_ok()
}

/// Drain the command queue onto the socket. Self-heals a dropped adb tunnel:
/// on a write error it first tries to RECONNECT the socket (the device helper
/// usually outlives a brief TCP hiccup, replaying the lost line); if that keeps
/// failing the injector is presumed dead, so we clear the global — input then
/// falls back to the AccessibilityService control plane instead of freezing —
/// and kick ONE background re-establish to win real-touch back. Exits cleanly
/// on `Quit` or when the queue's sender is dropped (stopped / replaced).
fn writer_loop(mut stream: TcpStream, rx: Receiver<Cmd>) {
    // Reconnects since the last line that actually got through.
    //
    // A reconnect that fails again on the very next line is not a recovery:
    // when the injector PROCESS on the phone is dead but `adb forward` is still
    // installed, adb accepts the local connection and then closes it, so every
    // single line "reconnects" and none is delivered. The loop below used to
    // `continue` on that forever — logging "control socket reconnected" once
    // per line, never reaching `on_injector_lost`, which is the only path that
    // RELAUNCHES the injector. That is what made the cursor stop crossing until
    // the app was restarted, most often right after the transport the injector
    // was launched on went away (cable out, or adbd restarted for wireless).
    const MAX_RECONNECTS: u32 = 3;
    // A write into a socket adb is about to close still succeeds — it lands in
    // the buffer. So one success proves nothing and the burst counter must not
    // be reset by it, or it oscillates 1,0,1,0 and never trips. A RUN of them
    // does prove it: motion sends many lines a second, so a healthy link clears
    // this in a fraction of a second and a dead tunnel never does.
    const HEALTHY_RUN: u32 = 20;
    let mut reconnects: u32 = 0;
    let mut run: u32 = 0;
    loop {
        let line = match rx.recv() {
            Ok(Cmd::Line(l)) => l,
            Ok(Cmd::Quit) => {
                let _ = stream.write_all(b"Q\n");
                let _ = stream.flush();
                return;
            }
            Err(_) => return, // sender dropped — session stopped or replaced
        };
        if write_line(&mut stream, &line) {
            if reconnects > 0 {
                run += 1;
                if run >= HEALTHY_RUN {
                    reconnects = 0;
                    run = 0;
                }
            }
            continue;
        }
        // Write failed — try to reconnect the adb tunnel for ~2s.
        let mut recovered = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(s) = TcpStream::connect(("127.0.0.1", ACTIVE_PORT.load(Ordering::SeqCst))) {
                let _ = s.set_nodelay(true);
                stream = s;
                // The replayed line is the test: a tunnel with nothing behind
                // it accepts the connection and fails the write.
                recovered = write_line(&mut stream, &line);
                break;
            }
        }
        if recovered {
            reconnects += 1;
            run = 0;
            if reconnects > MAX_RECONNECTS {
                tracing::warn!(
                    "mirror inject: socket reconnects but delivers nothing \
                     ({reconnects}×) — the injector is gone; relaunching"
                );
                on_injector_lost();
                return;
            }
            tracing::info!("mirror inject: control socket reconnected");
            continue;
        }
        tracing::warn!("mirror inject: control socket lost — accessibility fallback + re-establish");
        on_injector_lost();
        return;
    }
}

/// The writer's socket died unrecoverably mid-session. Clear the injector so
/// `active()` flips false (→ accessibility control, no freeze), then kick ONE
/// delayed re-establish so real-touch returns if the device is still reachable.
/// No-op re-launch if a concurrent `stop()` is tearing down.
/// Drop every adb forward pointing at OUR socket, on every transport.
///
/// Removing only `ACTIVE_PORT` leaked one forward per restart that landed on a
/// different port, and each is a listening socket on the host. Scoped to
/// `localabstract:vortex_inject` on purpose — `adb forward --remove-all` would
/// take out forwards the user set up for their own work.
fn remove_our_forwards() {
    let Some(out) = adb_capture(&["forward", "--list"]) else { return };
    for line in out.lines() {
        // "<serial> tcp:<port> localabstract:vortex_inject"
        let mut it = line.split_whitespace();
        let (Some(serial), Some(local), Some(remote)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if remote == SOCKET_NAME {
            let _ = adb_raw(&["-s", serial, "forward", "--remove", local]);
        }
    }
}

fn on_injector_lost() {
    if let Some(mut inj) = INJECT.lock().ok().and_then(|mut g| g.take()) {
        // Our own worker handle lives in here — detach (never join ourselves),
        // then reap the dead adb child.
        inj.worker = None;
        let _ = inj.child.kill();
        let _ = inj.child.wait();
    }
    let _ = adb(&["forward", "--remove", &format!("tcp:{}", ACTIVE_PORT.load(Ordering::SeqCst))]);
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(1));
        if !STOPPING.load(Ordering::SeqCst) && !active() {
            // Through `start_async`, NOT `start` — the direct call bypassed the
            // in-flight guard, so this relaunch could run concurrently with one
            // the capture loop had already kicked off. Each `start` begins by
            // pkill-ing `vortex_inject`, so the two killed each other's freshly
            // launched injector, roughly every ten seconds.
            start_async();
        }
    });
}

/// Keep the injector alive while Universal Control is armed but idle.
///
/// The writer only discovers a dead injector when it has something to write —
/// and it has nothing to write while the cursor is on the laptop. So the
/// injector would die during a quiet spell and the loss surfaced as the user's
/// NEXT crossing silently failing: the push is refused (nothing may block while
/// the pointer is captured), the cursor stays put, and only the push after that
/// works. That is the "sometimes it just doesn't go across".
///
/// A harmless line every few seconds gives the writer something to fail on, so
/// the existing detect-and-relaunch path runs in the quiet time instead of on
/// the user's hand.
///
/// The line is EMPTY, and that matters. It was `V 0` — described here as an
/// idempotent way to hide the phone-side cursor. It is not: on the phone `V 0`
/// DESTROYS the mouse device, which is how "no cursor while control is on the
/// laptop" is implemented. So every five seconds this reached in and deleted
/// the pointer the user was at that moment using, and whether a crossing
/// survived came down to where the tick happened to land — "sometimes it works,
/// sometimes the cursor just disappears". Verified on the phone: with a client
/// holding the socket and `V 1` sent, `dumpsys input` listed `vortex-touch` and
/// `vortex-keyboard` and no `vortex-mouse`; with the health check out of the
/// way, `vortex-mouse` is there.
///
/// An empty line parses to no command at all on the injector, so it costs a
/// write on the socket — which is the entire point — and nothing else.
pub fn spawn_health_check(armed: impl Fn() -> bool + Send + 'static) {
    const EVERY: Duration = Duration::from_secs(5);
    std::thread::spawn(move || loop {
        std::thread::sleep(EVERY);
        if !armed() || STOPPING.load(Ordering::SeqCst) {
            continue;
        }
        // Keep the cached HOGP subscription answer fresh — `has_transport`
        // reads it and cannot await.
        refresh_hogp_ready();
        if INJECT.lock().map(|g| g.is_some()).unwrap_or(false) {
            send("");
        } else if !active() {
            // No injector at all: try to get one back, so the next crossing
            // finds a live transport rather than starting one from scratch.
            start_async();
        }
    });
}

/// The Bluetooth HID server, when one is registered.
///
/// Linux-only: registering a HID profile means owning a BlueZ profile object
/// over D-Bus. The adb transport above is what carries Universal Control
/// everywhere else.
#[cfg(target_os = "linux")]
static BT_HID: Mutex<Option<vortex_l3_daemon::core::bt_hid::BtHidServer>> = Mutex::new(None);

/// BLE HID (HOGP) — the adb-free transport, and the only one that survives a
/// phone reboot without a cable.
///
/// Proven on hardware before being wired in; `core/hogp.rs` carries the
/// result. Ranked BELOW the adb injector because adb can draw a real finger on
/// the touchscreen while HID can only move a pointer — but unlike adb it needs
/// no developer mode, no `adb tcpip`, and no re-bootstrap after the phone
/// restarts, which is the single biggest thing standing between a new user and
/// this feature working.
#[cfg(target_os = "linux")]
static HOGP: Mutex<Option<vortex_l3_daemon::core::hogp::HogpServer>> = Mutex::new(None);

#[cfg(target_os = "linux")]
pub fn set_hogp(server: vortex_l3_daemon::core::hogp::HogpServer) {
    if let Ok(mut g) = HOGP.lock() {
        *g = Some(server);
    }
}

#[cfg(target_os = "linux")]
pub fn get_hogp() -> Option<vortex_l3_daemon::core::hogp::HogpServer> {
    HOGP.lock().ok().and_then(|g| g.clone())
}

/// Cached "a host has subscribed to our input reports".
///
/// `HogpServer::is_ready` is async, and this is asked from the capture loop
/// where nothing may block — the same rule that governs every other question
/// asked at crossing time. So the answer is kept here and refreshed off-thread
/// by the health check.
static HOGP_READY: AtomicBool = AtomicBool::new(false);

/// Mouse buttons currently held, for HOGP. A HID report carries the WHOLE
/// button state every time, so a move sent mid-drag has to repeat the held bit
/// or the phone reads it as a release.
static HOGP_BUTTONS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn hogp_ready() -> bool {
    HOGP_READY.load(Ordering::Relaxed)
}

/// Re-read whether a HOGP host is subscribed. Subscription — not the bond and
/// not the connection — is what means a report will actually be delivered.
/// No HID-over-GATT server off Linux, so readiness never changes from false.
#[cfg(not(target_os = "linux"))]
pub fn refresh_hogp_ready() {}

#[cfg(target_os = "linux")]
pub fn refresh_hogp_ready() {
    let Some(server) = get_hogp() else {
        HOGP_READY.store(false, Ordering::Relaxed);
        return;
    };
    tauri::async_runtime::spawn(async move {
        HOGP_READY.store(server.is_ready().await, Ordering::Relaxed);
    });
}

/// Register the Bluetooth HID server for ADB-free Universal Control fallback.
#[cfg(target_os = "linux")]
pub fn set_bt_hid(server: vortex_l3_daemon::core::bt_hid::BtHidServer) {
    if let Ok(mut g) = BT_HID.lock() {
        *g = Some(server);
    }
}

/// True when the ADB real-touch injector is currently active.
/// Bring the injector up WITHOUT blocking the caller.
///
/// [`start`] is a long chain of blocking adb calls — push the binary, set up a
/// port forward, probe /dev/uinput, open a socket. Universal Control's capture
/// loop used to call it inline at the moment a push committed, which is the
/// moment the compositor is already holding the pointer and every keystroke for
/// us. With the phone unreachable that chain ran for seconds (100+ in the worst
/// case here), and for its whole length the pointer was pinned to the screen
/// edge, Esc did nothing, and the loop's own 60 ms and 6 s failsafes could not
/// fire because they live on the same thread. The user's only way out was to
/// lock the laptop from their phone.
///
/// So: never from the capture thread. Kick it off here and let the crossing be
/// abandoned; by the next push it is usually up (a healthy start measured
/// 236-513 ms). One at a time — a push that repeats must not stack up starts.
pub fn start_async() {
    static STARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let ok = start();
        STARTING.store(false, std::sync::atomic::Ordering::SeqCst);
        if !ok {
            tracing::debug!("mirror inject: background start did not come up");
        }
    });
}

/// True when either the ADB real-touch injector or Bluetooth HID server is live.
pub fn active() -> bool {
    if INJECT.lock().map(|g| g.is_some()).unwrap_or(false) {
        return true;
    }
    // Classic Bluetooth HID: a BlueZ profile, so Linux only.
    #[cfg(target_os = "linux")]
    if let Ok(g) = BT_HID.lock() {
        if let Some(hid) = g.as_ref() {
            if hid.is_connected() {
                return true;
            }
        }
    }
    hogp_ready()
}

/// Proactively connect paired Bluetooth devices when approaching screen edge.
/// Floor between HID connect attempts. Without it this fired on every single
/// edge crossing, so a phone that simply is not bonded as an input device got
/// a fresh connect attempt every few seconds.
const BT_CONNECT_COOLDOWN: Duration = Duration::from_secs(60);
static LAST_BT_CONNECT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Ask the paired phone to bring the Bluetooth HID link up.
///
/// Bluetooth HID is the FALLBACK for when adb cannot be reached, so this is a
/// no-op while the uinput injector is live: reaching for a second input
/// transport that is already unnecessary is how the phone ended up being asked
/// to connect over and over while the cursor was crossing perfectly well over
/// Wi-Fi.
/// Nothing to reach out to: the Bluetooth HID profile is a BlueZ object.
#[cfg(not(target_os = "linux"))]
pub fn trigger_bt_connect() {}

#[cfg(target_os = "linux")]
pub fn trigger_bt_connect() {
    if INJECT.lock().map(|g| g.is_some()).unwrap_or(false) {
        return; // adb injector is up — the fallback is not needed
    }
    {
        let mut last = LAST_BT_CONNECT.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < BT_CONNECT_COOLDOWN) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    // Classic Bluetooth HID: a BlueZ profile, so Linux only.
    #[cfg(target_os = "linux")]
    if let Ok(g) = BT_HID.lock() {
        if let Some(hid) = g.as_ref() {
            if !hid.is_connected() {
                let hid_clone = hid.clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(conn) = zbus::connection::Connection::system().await {
                        hid_clone.try_connect_paired_devices(&conn).await;
                    }
                });
            }
        }
    }
}

/// True when input can ACTUALLY be delivered to the phone right now — the ADB
/// uinput daemon is up, or the Bluetooth HID link is connected.
///
/// `is_registered()` is deliberately NOT accepted here. Registering the HID
/// profile only tells BlueZ that we exist; it says nothing about the phone
/// being on the other end. Counting it made this return true with no link at
/// all, which let the crossing guard in `universal_control` wave the pointer
/// through: the cursor left the laptop and never appeared on the phone —
/// stranded on a screen showing nothing, the exact failure the guard exists to
/// prevent. Observed 2026-08-24 with adb offline and BT HID registered but
/// unconnected.
///
/// Use `is_registered()` for "is the fallback wired up at all" questions, not
/// for "can we send".
pub fn has_transport() -> bool {
    if INJECT.lock().map(|g| g.is_some()).unwrap_or(false) {
        return true;
    }
    // Classic Bluetooth HID: a BlueZ profile, so Linux only.
    #[cfg(target_os = "linux")]
    if let Ok(g) = BT_HID.lock() {
        if let Some(hid) = g.as_ref() {
            if hid.is_connected() {
                return true;
            }
        }
    }
    hogp_ready()
}

/// Send one protocol line (`D slot nx ny`, `M slot nx ny`, `U slot`,
/// `E keycode val`, `K back`…). Best-effort: enqueue only — the writer thread
/// owns the socket and self-heals a dropped tunnel.
pub fn send(line: &str) {
    if let Ok(g) = INJECT.lock() {
        if let Some(inj) = g.as_ref() {
            // Non-blocking: just enqueue. The writer thread does the socket I/O.
            let _ = inj.tx.send(Cmd::Line(line.to_string()));
            return;
        }
    }
    // Fallback 1: BLE HID (HOGP). Tried before Classic HID because it is the
    // one that actually works on a phone the user has not put into developer
    // mode — Classic needs adapter changes we refuse to make to their system.
    //
    // Buttons are held here rather than in the report, because HOGP sends the
    // whole state on every report: a move sent while the left button is down
    // must repeat that bit or the phone sees the button release mid-drag.
    if hogp_ready() {
        #[cfg(target_os = "linux")]
        if let Some(server) = get_hogp() {
            let mut parts = line.trim().split_whitespace();
            let cmd = parts.next().unwrap_or("");
            let mut num = || parts.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
            match cmd {
                "P" => {
                    let (dx, dy) = (num(), num());
                    let (dx, dy) = (dx.clamp(-127, 127) as i8, dy.clamp(-127, 127) as i8);
                    let buttons = HOGP_BUTTONS.load(Ordering::Relaxed);
                    tauri::async_runtime::spawn(async move {
                        server.send_report(buttons, dx, dy, 0).await;
                    });
                    return;
                }
                "B" => {
                    let (btn, val) = (num(), num());
                    let bit = 1u8 << (btn.clamp(0, 4) as u8);
                    let buttons = if val == 1 {
                        HOGP_BUTTONS.fetch_or(bit, Ordering::Relaxed) | bit
                    } else {
                        HOGP_BUTTONS.fetch_and(!bit, Ordering::Relaxed) & !bit
                    };
                    tauri::async_runtime::spawn(async move {
                        server.send_report(buttons, 0, 0, 0).await;
                    });
                    return;
                }
                "W" => {
                    let dy = num().clamp(-127, 127) as i8;
                    let buttons = HOGP_BUTTONS.load(Ordering::Relaxed);
                    tauri::async_runtime::spawn(async move {
                        server.send_report(buttons, 0, 0, dy).await;
                    });
                    return;
                }
                // `V` (cursor overlay) and `D/M/U` (real touch) have no HID
                // equivalent: a HID mouse gets Android's own pointer, and it
                // cannot draw a finger. Dropping them is correct, not a gap.
                _ => return,
            }
        }
    }

    // Fallback 2: Classic Bluetooth HID report dispatch
    // Classic Bluetooth HID: a BlueZ profile, so Linux only.
    #[cfg(target_os = "linux")]
    if let Ok(g) = BT_HID.lock() {
        if let Some(hid) = g.as_ref() {
            let line_str = line.trim();
            let mut parts = line_str.split_whitespace();
            let cmd = parts.next().unwrap_or("");
            match cmd {
                "P" => {
                    let dx: i32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let dy: i32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let dx_i8 = dx.clamp(-127, 127) as i8;
                    let dy_i8 = dy.clamp(-127, 127) as i8;
                    let hid_clone = hid.clone();
                    tauri::async_runtime::spawn(async move {
                        hid_clone.send_mouse(dx_i8, dy_i8, 0).await;
                    });
                }
                "B" => {
                    let btn: u8 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let val: u8 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let hid_clone = hid.clone();
                    tauri::async_runtime::spawn(async move {
                        hid_clone.send_button(btn, val == 1).await;
                    });
                }
                "W" => {
                    let dy: i8 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let hid_clone = hid.clone();
                    tauri::async_runtime::spawn(async move {
                        hid_clone.send_mouse(0, 0, dy).await;
                    });
                }
                "E" => {
                    let code: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let val: u8 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let hid_clone = hid.clone();
                    tauri::async_runtime::spawn(async move {
                        hid_clone.send_key(code, val == 1).await;
                    });
                }
                _ => {}
            }
        }
    }
}

/// Tear down the injector (sends `Q`, stops the writer thread, kills the adb
/// process — which destroys the virtual device — and removes the forward).
pub fn stop() {
    // Block the writer's self-heal from re-launching us during teardown.
    STOPPING.store(true, Ordering::SeqCst);
    let taken = INJECT.lock().ok().and_then(|mut g| g.take());
    if let Some(mut inj) = taken {
        let _ = inj.tx.send(Cmd::Quit);
        if let Some(w) = inj.worker.take() {
            let _ = w.join();
        }
        let _ = inj.child.kill();
        let _ = inj.child.wait();
    }
    remove_our_forwards();
    // The injector outlives the shell that launched it (see `start`), so
    // killing the local child no longer stops it — name it explicitly.
    let _ = adb(&["shell", "pkill", "-f", "vortex_inject"]);
}

#[cfg(test)]
mod tests {
    use super::{redial_port_order, PointerCurve, WIRELESS_PORT};

    #[test]
    fn redial_tries_the_remembered_port_before_5555() {
        // Wireless-debugging port: try it first, keep 5555 as the fallback for a
        // phone that has since gone back to `adb tcpip`.
        assert_eq!(redial_port_order(Some(37129)), vec![37129, WIRELESS_PORT]);
        // Nothing remembered yet — the legacy port is the only sensible guess.
        assert_eq!(redial_port_order(None), vec![WIRELESS_PORT]);
        // Already 5555: don't dial the same port twice (each miss costs ~1 s).
        assert_eq!(redial_port_order(Some(WIRELESS_PORT)), vec![WIRELESS_PORT]);
    }

    /// Android's side of the bargain: what a delta of `sent` pixels, arriving
    /// `dt` after the last one, actually moves the cursor by.
    fn applied(c: &PointerCurve, sent: f32, dt: f32) -> f32 {
        let speed = sent / dt * c.scale;
        let f = if speed <= c.low {
            1.0
        } else if speed >= c.high {
            c.accel
        } else {
            1.0 + (speed - c.low) / (c.high - c.low) * (c.accel - 1.0)
        };
        sent * c.scale * f
    }

    /// The whole point of [`PointerCurve::undo`]: whatever it hands to Android
    /// comes back out as the movement that was asked for — over the flat part of
    /// the curve, the ramp, and the saturated end alike.
    #[test]
    fn undo_is_the_inverse_of_the_curve() {
        let c = PointerCurve::DEFAULT;
        for dt in [0.002f32, 0.008, 0.05] {
            for want in [0.5f32, 1.0, 3.0, 10.0, 40.0, 200.0] {
                let got = applied(&c, c.undo(want, dt), dt);
                assert!(
                    (got - want).abs() <= want * 0.01 + 0.01,
                    "want {want} at dt {dt}: sent {} → {got}",
                    c.undo(want, dt)
                );
            }
        }
    }

    /// Slow movement is not accelerated at all, so there is nothing to undo.
    #[test]
    fn slow_movement_passes_through_untouched() {
        let c = PointerCurve::DEFAULT;
        // 1px per 8ms = 125 px/s, well under the 500 px/s floor.
        assert!((c.undo(1.0, 0.008) - 1.0).abs() < 0.001);
    }

    /// A fast flick is at the far end of the curve, where the factor is fixed —
    /// the same one a back-to-back pair always gets.
    #[test]
    fn fast_movement_divides_by_the_saturated_factor() {
        let c = PointerCurve::DEFAULT;
        // 100px in 2ms = 50 000 px/s.
        assert!((c.undo(100.0, 0.002) - 100.0 / c.saturated()).abs() < 0.01);
    }

    /// The first delta of a crossing has no predecessor, so `dt` is whatever the
    /// caller had — possibly zero — and a parked cursor asks for zero travel.
    /// Both have to come back finite: a NaN here becomes a delta the injector
    /// writes to uinput, and the phone's cursor leaves for good.
    #[test]
    fn degenerate_asks_stay_finite() {
        let c = PointerCurve::DEFAULT;
        for (want, dt) in [(0.0f32, 0.008f32), (10.0, 0.0), (0.0, 0.0), (-5.0, 0.008)] {
            let got = c.undo(want, dt);
            assert!(got.is_finite(), "undo({want}, {dt}) = {got}");
            assert_eq!(got, want, "undo({want}, {dt}) should pass through");
        }
        // A pointer-speed setting of zero is not something Android reports, but
        // `pointer_curve` only rejects it on the read path — division by it here
        // would be silent.
        let dead = PointerCurve { scale: 0.0, ..PointerCurve::DEFAULT };
        assert!(dead.undo(10.0, 0.008).is_finite());
    }

    /// A pointer-speed setting other than the default scales everything, both
    /// the acceleration and the flat part below it.
    #[test]
    fn scale_is_undone_as_well() {
        let c = PointerCurve { scale: 2.0, ..PointerCurve::DEFAULT };
        assert!((c.undo(1.0, 0.05) - 0.5).abs() < 0.001);
        assert!((applied(&c, c.undo(30.0, 0.002), 0.002) - 30.0).abs() < 0.3);
    }
}
