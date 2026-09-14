//! The platform seam: everything the laptop side needs from the OS, expressed
//! as traits so a second OS can be added without touching feature logic.
//!
//! # Why this exists
//!
//! Until now every OS call was made inline against a Linux API — BlueZ over
//! D-Bus, logind, XDG directories, the freedesktop notification service — with
//! no `cfg(target_os)` anywhere in the tree. That is fine for one OS and
//! impossible for two. These traits are the boundary: **feature logic above,
//! OS below**. The rule is that nothing above this line names a Linux concept.
//!
//! # What is deliberately NOT here
//!
//! * **Storage.** [`crate::core::storage`] already has `IdentityStore` and
//!   `PeerStore`; a Windows Credential Manager implementation slots in beside
//!   `SecretServiceIdentityStore` with no new trait.
//! * **The wire protocol, crypto, framing, LAN and mDNS.** They are pure Rust
//!   and must stay byte-identical across platforms — the phone cannot tell the
//!   two laptops apart, and `shared/vectors/` exists to keep it that way.
//! * **Clipboard.** `arboard` already covers Linux and Windows.
//!
//! # Status
//!
//! Linux implementations delegate to the modules that already existed, so this
//! file adds a boundary without changing behaviour there. **Every trait here now
//! has a Windows implementation**, and so does the secret store — Credential
//! Manager in `storage::windows_credentials`, beside the Secret Service one.
//!
//! What no Windows build has yet is the layer ABOVE this: the Tauri app is
//! Linux-only (its tray, its D-Bus consumers, its uinput injector), and the
//! `vortex-l3d` CLI is a Linux BLE harness. This seam is what makes that layer
//! portable, not a substitute for porting it.
//!
//! Everything Windows-side is compiled only on Windows and none of it has ever
//! run: it type-checks against the WinRT/Win32 metadata, which catches wrong
//! signatures and wrong types and nothing about behaviour.
//!
//! **The daemon LIBRARY compiles for Windows.** Verify with:
//!
//! ```text
//! cargo check -p vortex-l3-daemon --lib --target x86_64-pc-windows-gnu
//! ```
//!
//! Two notes on that command. `--lib`, because `src/main.rs` is a Linux BLE CLI
//! harness and is not part of a Windows build (the product there is the Tauri
//! app). And `-gnu` rather than `-msvc`: an MSVC cross-check needs `lib.exe`,
//! which a Linux box does not have, so it dies in `cc-rs` before reaching our
//! code. The GNU target type-checks the same source.
//!
//! What compiles is the platform-neutral core: crypto, framing, the wire
//! protocol (`ble::frame`), LAN + mDNS, the pairing state machine, appstate,
//! the storage traits, and this seam. What is gated out — with the reason on
//! each `cfg` — is every direct BlueZ / D-Bus / PulseAudio / Secret Service
//! module.
//!
//! # BLE is the one trait with both sides written
//!
//! [`BleCentral`] / [`GattLink`] now have a BlueZ implementation
//! ([`linux::LinuxBleCentral`], wrapping the existing `ble::client` rather than
//! reimplementing its dual-mode connect dance) and a WinRT one
//! ([`windows::ble::WindowsBleCentral`]). Writing the second one is what
//! reshaped the trait: it needed a `read` (the capability handshake), a `has`
//! (the audio-signal characteristic is absent on older phones), an async
//! `is_connected` (BlueZ answers over D-Bus) and a `scan_for_peer` that returns
//! the advertisement PAYLOAD rather than a bare address — the phone rotates its
//! address, so the payload is what identifies a peer.
//!
//! Its callers moved over too: `pairing::{handshake, reconnect}` now take
//! `&dyn GattLink`, are no longer gated, and build for Windows. That also made
//! them testable for the first time — a full XX pairing with dual approval, and
//! the IK msg1 wire shape, now run as unit tests against [`FakeGattLink`] with
//! no adapter and no phone.
//!
//! `ble::audio_signal` — the post-handshake event stream, all nineteen frame
//! types plus the nonce-resync recovery — moved across too. Its one genuinely
//! local dependency, the earbuds handoff, went behind [`AudioHandoff`]; a
//! platform with no audio backend passes `None`, drops `AUDIO_OP`, and keeps
//! the other eighteen.
//!
//! # The gates are not the port
//!
//! A `cfg(target_os = "linux")` on a module means "no Windows implementation
//! yet", not "not needed on Windows". What is left behind one is a Linux
//! *implementation* — BlueZ, logind, MPRIS, PulseAudio, Secret Service — with
//! its trait already named here, or a subsystem with no Windows counterpart
//! written yet.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub mod linux;
pub mod toast_xml;
pub mod vk_to_evdev;
#[cfg(target_os = "windows")]
pub mod windows;

/// Standard user directories. Localised on both platforms and NOT derivable by
/// joining an English folder name onto `$HOME` — the French desktop this was
/// first written on uses `~/Téléchargements`, and Windows relocates the
/// Downloads folder freely (OneDrive moves it by default).
pub trait UserPaths: Send + Sync {
    /// Where received files are saved. Must be the user's real download folder.
    fn downloads(&self) -> Option<PathBuf>;
    /// Per-user config root (`~/.config/vortex`, `%APPDATA%\Vortex`).
    fn config(&self) -> Option<PathBuf>;
    /// Per-user cache root — icon cache, transient blobs.
    fn cache(&self) -> Option<PathBuf>;
    /// Where a log file goes, on a platform that writes one.
    ///
    /// Linux does not: the app runs under a systemd user unit and its output is
    /// the journal, which is where every diagnosis in this project has come
    /// from. Windows has no such thing for a desktop app, so it writes a file —
    /// and "where is the log" must not be a guess when the first run of
    /// never-executed code goes wrong.
    fn logs(&self) -> Option<PathBuf>;
}

/// This machine's name, as the user would recognise it.
///
/// Lives at the seam because there is no portable way to ask: Linux reads
/// `/proc`, Windows has an environment variable that Linux does not set. Both
/// the pairing APPROVE frame and every AppState heartbeat carry this, and they
/// have to agree — a heartbeat that reports `None` overwrites the name the
/// phone learned at pairing time with a blank, which is what made a
/// freshly-paired Windows laptop show up on the phone as "null".
pub fn host_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    // `COMPUTERNAME` is set for every interactive session, and the NetBIOS name
    // it holds is what the machine calls itself in every Windows UI.
    #[cfg(not(target_os = "linux"))]
    let raw = std::env::var("COMPUTERNAME").ok();
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A desktop notification carrying optional action buttons.
///
/// The hard part on both platforms is not showing it, it is getting the click
/// back. On Linux the sender must stay on the bus for Plasma to keep the
/// buttons, while GNOME needs a windowless sender. On Windows the toast needs
/// an AppUserModelID, and an unpackaged app needs a registered COM activator
/// before an action can round-trip at all.
pub trait Notifier: Send + Sync {
    /// Show (or replace, when `replaces` is non-zero) a notification. `actions`
    /// is `(key, label)`; the key comes back through [`Notifier::actions`].
    fn show(
        &self,
        summary: &str,
        body: &str,
        app_id: &str,
        actions: &[(String, String)],
        replaces: u32,
        urgent: bool,
    ) -> BoxFuture<Result<u32, String>>;

    /// Withdraw a notification we previously showed.
    fn close(&self, id: u32) -> BoxFuture<Result<(), String>>;

    /// Stream of `(notification id, action key)` for every button the user
    /// clicks. One process-wide stream: consumers filter by key prefix
    /// (`fc:` file consent, `call:` call banner, `act:` mirrored action).
    fn actions(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>);

    /// Stream of `(notification id, reason)` closures, so a dismissal on the
    /// laptop can be mirrored back to the phone.
    fn closures(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>);
}

/// Lock / unlock the desktop session and report its current state — the
/// proximity feature's entire OS surface.
///
/// Windows can lock (`LockWorkStation`) but deliberately cannot unlock
/// programmatically, so proximity *auto-unlock* is Linux-only and the trait
/// lets an implementation say so rather than fail at the call site.
pub trait SessionControl: Send + Sync {
    fn lock(&self) -> BoxFuture<Result<(), String>>;
    fn unlock(&self) -> BoxFuture<Result<(), String>>;
    /// `None` when the platform can't report it.
    fn is_locked(&self) -> BoxFuture<Option<bool>>;
    /// Whether [`SessionControl::unlock`] can work at all here.
    fn can_unlock(&self) -> bool;
}

/// The audio-handoff side of the phone's event stream.
///
/// `ble::audio_signal` carries nineteen frame types, and exactly one of them —
/// `AUDIO_OP`, the earbuds handoff — needs to touch the local audio stack. This
/// trait is that touch point, so the other eighteen don't drag PulseAudio and
/// MPRIS into a build that has neither.
///
/// A platform with no audio backend passes `None` and simply drops `AUDIO_OP`
/// frames: no earbuds switching, everything else works.
pub trait AudioHandoff: Send + Sync {
    /// The phone is starting a buds-claim (almost always an incoming call).
    /// Pause local media BEFORE the buds are released — once the sink goes away
    /// the audio server migrates the stream and the player often auto-pauses on
    /// its own, leaving nothing to resume later.
    fn pause_for_call(&self) -> BoxFuture<()>;

    /// Drive the switch state machine with a frame from `peer`.
    fn on_incoming(&self, peer: [u8; 32], frame: crate::core::audio_op::AudioOpFrame)
        -> BoxFuture<()>;
}

/// Run Vortex at login.
pub trait Autostart: Send + Sync {
    fn is_enabled(&self) -> bool;
    fn set_enabled(&self, on: bool) -> Result<(), String>;
}

/// Quote an executable path for a Windows command line.
///
/// The registry `Run` value is a COMMAND LINE, not a path: Windows splits it on
/// whitespace, so `C:\Program Files\Vortex\vortex.exe` unquoted launches
/// `C:\Program`. Since the default install location contains a space, an
/// unquoted value fails on essentially every machine — silently, at the next
/// logon, where nobody is watching.
///
/// Lives here with a test rather than inline in the Windows module, for the same
/// reason as [`toast_xml`]: it is pure string work whose failure mode is
/// invisible.
pub fn quoted_command(exe: &std::path::Path) -> String {
    let s = exe.to_string_lossy();
    if s.starts_with('"') && s.ends_with('"') && s.len() > 1 {
        // Already quoted — double-quoting would make the path literal-quotes.
        return s.to_string();
    }
    format!("\"{s}\"")
}

/// Pointer/keyboard capture for Universal Control: hold the cursor at a screen
/// edge, take exclusive input, and stream events for forwarding to the phone.
///
/// This is the one subsystem that is *easier* on Windows — a low-level hook
/// plus `ClipCursor` does what Wayland needs the input-capture portal and libei
/// for, and it works the same on every Windows desktop.
pub trait InputCapture: Send + Sync {
    /// Arm capture on the given edge. Events flow to `tx` until released.
    fn arm(&self, edge: Edge, tx: tokio::sync::mpsc::UnboundedSender<InputEvent>)
        -> BoxFuture<Result<(), String>>;
    /// Release capture; the cursor returns to the laptop.
    fn release(&self) -> BoxFuture<Result<(), String>>;
    /// Hide the laptop's own cursor while control is on the phone. Best-effort:
    /// GNOME-only today, and `false` means "couldn't", not "failed".
    fn hide_cursor(&self, hidden: bool) -> bool;
}

/// Which screen edge the phone sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// One captured input event, already in the platform-neutral form the phone
/// side expects.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// Relative pointer motion.
    Motion { dx: f64, dy: f64 },
    /// Button 1 = left, 2 = middle, 3 = right.
    Button { button: u8, pressed: bool },
    /// Vertical / horizontal scroll, in notches.
    Scroll { dx: f64, dy: f64 },
    /// A Linux evdev keycode — the phone side already speaks these, so Windows
    /// translates its VK codes into this space rather than inventing a third.
    Key { keycode: u16, pressed: bool },
}

/// BLE central role: scan for the phone's advertisement, connect, and talk GATT.
///
/// The laptop is central-**only** — it never advertises and never serves a GATT
/// server, which is what makes Windows viable at all (WinRT's peripheral role is
/// far weaker than its central role).
///
/// # Construction is deliberately not part of this trait
///
/// There is no `platform::ble()` factory to match [`paths`] / [`notifier`] /
/// [`session`], because the two platforms genuinely differ in what they need to
/// exist:
///
/// * Linux takes the process's ONE shared `bluer::Adapter`
///   ([`linux::LinuxBleCentral::new`]). Creating a session per use accumulated
///   D-Bus connections and hung the app after a few call cycles, so the adapter
///   is passed in rather than acquired — the same reason the heartbeat and the
///   BLE loop already share one.
/// * Windows needs no handle at all; WinRT resolves the radio per call.
///
/// A uniform factory would have to hide that, and hiding it is how the leak
/// came back. Callers construct the platform's central once at startup and pass
/// `Arc<dyn BleCentral>` down, which is what the BLE loop already does with its
/// adapter today.
pub trait BleCentral: Send + Sync {
    /// Scan until a Vortex advertisement is seen, or the timeout elapses.
    fn scan_for_peer(&self, timeout_ms: u64) -> BoxFuture<Result<Option<AdvCandidate>, String>>;
    /// Connect and resolve the Vortex GATT service.
    fn connect(&self, addr: PeerAddr) -> BoxFuture<Result<Box<dyn GattLink>, String>>;
    /// Addresses of already-bonded devices, for the reconnect fast path.
    fn bonded(&self) -> BoxFuture<Result<Vec<PeerAddr>, String>>;
    /// Whether the radio is present and powered.
    fn adapter_ready(&self) -> BoxFuture<bool>;
}

/// An open GATT connection to the phone.
///
/// The shape of this trait is set by what the pairing and reconnect flows
/// actually do over the link, which is: read the capability characteristic,
/// write frames without response (§9.1 — the flow is driven by notify-on-write,
/// so the ATT ack buys latency and no reliability), and subscribe for the
/// notifications those writes provoke.
pub trait GattLink: Send + Sync {
    /// Write one frame to a characteristic. `with_response = false` is an ATT
    /// Write Command, which is what the pairing and reconnect frames use.
    fn write(&self, char_uuid: Uuid128, data: &[u8], with_response: bool)
        -> BoxFuture<Result<(), String>>;

    /// Read a characteristic — the capability handshake (§9.1.5) needs this
    /// before any frame is written.
    fn read(&self, char_uuid: Uuid128) -> BoxFuture<Result<Vec<u8>, String>>;

    /// Subscribe to notifications; frames arrive on `tx` until disconnect.
    fn subscribe(&self, char_uuid: Uuid128, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>)
        -> BoxFuture<Result<(), String>>;

    /// Which peer this link talks to. Both platforms know it at connect time;
    /// it exists so log lines can name the device without the caller having to
    /// carry the address alongside the link.
    fn peer(&self) -> PeerAddr;

    /// Whether this link resolved `char_uuid` at all.
    ///
    /// Not every characteristic is guaranteed: the audio-signal one is absent
    /// on phone builds before P2.13, and those peers must keep working with the
    /// LAN heartbeat instead of failing the connect. Callers check rather than
    /// discovering it as a write error.
    fn has(&self, char_uuid: Uuid128) -> bool;

    fn disconnect(&self) -> BoxFuture<Result<(), String>>;

    /// Async because Linux has to ask BlueZ over D-Bus; Windows reads a
    /// property. A sync signature would have forced Linux to cache a flag and
    /// answer with something stale.
    fn is_connected(&self) -> BoxFuture<bool>;
}

/// A Vortex peer seen on air.
///
/// Carries the advertisement payload, not just the address, because the address
/// alone cannot answer the questions the callers ask: the pairing UI needs the
/// `pairable` flag and the instance id to match the window the user just opened
/// on the phone, and the reconnect path needs the presence token to know WHICH
/// trusted peer this is. The phone rotates its address every few minutes, so it
/// is the payload that identifies, not the address.
// Not `Copy`: `local_name` is an owned `String`. Nothing needs it to be — the
// two fields callers pass around by value (`addr`, `rssi`) are `Copy` on their
// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvCandidate {
    pub addr: PeerAddr,
    pub payload: crate::core::ble::AdvPayload,
    /// Signal strength, where the platform reports it — used only to prefer a
    /// nearer peer, never to decide identity.
    pub rssi: Option<i16>,
    /// The advert's Complete Local Name, when it carries one.
    ///
    /// Cosmetic and untrusted: it is what the pairing radar labels a row with
    /// so the user recognises their own phone instead of reading a rotating
    /// random address. The name that ends up in the trust store is the one
    /// inside the authenticated APPROVE frame, never this.
    pub local_name: Option<String>,
}

/// A Bluetooth device address. Deliberately a plain newtype rather than
/// `bluer::Address`: Windows hands out a `u64`, and the resolvable private
/// addresses the phone rotates through mean the *value* is never a stable
/// identity anyway — the peer's static public key is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerAddr(pub [u8; 6]);

impl PeerAddr {
    /// From the `u64` WinRT hands out (`BluetoothLEDevice::BluetoothAddress`,
    /// `BluetoothLEAdvertisementReceivedEventArgs::BluetoothAddress`).
    ///
    /// The 48-bit address occupies the low six bytes, most-significant byte
    /// first in printed order: `0x0000_AABB_CCDD_EEFF` is `AA:BB:CC:DD:EE:FF`.
    /// The top two bytes are always zero and are dropped. Kept here rather than
    /// in the Windows module so it can be tested on either platform — it is
    /// pure arithmetic, and getting it backwards would mean connecting to a
    /// mirrored address that simply never answers.
    pub fn from_u48(addr: u64) -> Self {
        let b = addr.to_be_bytes();
        Self([b[2], b[3], b[4], b[5], b[6], b[7]])
    }

    /// The inverse of [`PeerAddr::from_u48`], for handing an address back to a
    /// WinRT call.
    pub fn to_u48(self) -> u64 {
        let a = self.0;
        u64::from_be_bytes([0, 0, a[0], a[1], a[2], a[3], a[4], a[5]])
    }
}

impl std::fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

/// A 128-bit GATT UUID, in the same byte order both platforms accept.
pub type Uuid128 = u128;

/// Boxed future alias — these traits are object-safe on purpose (the active
/// platform is chosen at runtime through `dyn`, so feature code never carries a
/// platform type parameter).
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// The active platform's user paths.
pub fn paths() -> &'static dyn UserPaths {
    #[cfg(target_os = "linux")]
    {
        &linux::LinuxPaths
    }
    #[cfg(target_os = "windows")]
    {
        &windows::WindowsPaths
    }
}

/// The active platform's notifier.
pub fn notifier() -> &'static dyn Notifier {
    #[cfg(target_os = "linux")]
    {
        &linux::LinuxNotifier
    }
    #[cfg(target_os = "windows")]
    {
        &windows::notify::WindowsNotifier
    }
}

/// The active platform's session control.
pub fn session() -> &'static dyn SessionControl {
    #[cfg(target_os = "linux")]
    {
        &linux::LinuxSession
    }
    #[cfg(target_os = "windows")]
    {
        &windows::WindowsSession
    }
}

/// A [`GattLink`] with no radio behind it: writes are recorded, reads replay a
/// scripted value, and notifications are whatever the test pushes.
///
/// This is the payoff for having a seam at all. The pairing and reconnect flows
/// are the code most worth testing and the least testable — today they need a
/// phone, a BlueZ adapter and a human. Written against `&dyn GattLink` they can
/// be driven from a unit test on either platform, and this is the harness that
/// makes that possible. It lives here rather than in a test module so the port
/// work can use it as it moves those flows onto the trait.
#[cfg(test)]
pub struct FakeGattLink {
    /// Characteristics this link pretends to have.
    pub present: Vec<Uuid128>,
    /// `(uuid, bytes, with_response)` in call order.
    pub writes: std::sync::Mutex<Vec<(Uuid128, Vec<u8>, bool)>>,
    /// What [`GattLink::read`] answers, per characteristic.
    pub reads: std::collections::HashMap<Uuid128, Vec<u8>>,
    /// Senders handed to [`GattLink::subscribe`], so a test can push frames.
    pub subscribers: std::sync::Mutex<Vec<(Uuid128, tokio::sync::mpsc::UnboundedSender<Vec<u8>>)>>,
    pub connected: bool,
}

#[cfg(test)]
impl FakeGattLink {
    pub fn new(present: Vec<Uuid128>) -> Self {
        Self {
            present,
            writes: std::sync::Mutex::new(Vec::new()),
            reads: std::collections::HashMap::new(),
            subscribers: std::sync::Mutex::new(Vec::new()),
            connected: true,
        }
    }

    /// Deliver `bytes` as a notification on `uuid`, as the phone would.
    pub fn push_notification(&self, uuid: Uuid128, bytes: Vec<u8>) {
        for (u, tx) in self.subscribers.lock().unwrap().iter() {
            if *u == uuid {
                let _ = tx.send(bytes.clone());
            }
        }
    }
}

#[cfg(test)]
impl GattLink for FakeGattLink {
    fn write(
        &self,
        char_uuid: Uuid128,
        data: &[u8],
        with_response: bool,
    ) -> BoxFuture<Result<(), String>> {
        let ok = self.present.contains(&char_uuid);
        if ok {
            self.writes
                .lock()
                .unwrap()
                .push((char_uuid, data.to_vec(), with_response));
        }
        Box::pin(async move {
            if ok {
                Ok(())
            } else {
                Err("no such characteristic".to_string())
            }
        })
    }

    fn read(&self, char_uuid: Uuid128) -> BoxFuture<Result<Vec<u8>, String>> {
        let v = self.reads.get(&char_uuid).cloned();
        Box::pin(async move { v.ok_or_else(|| "nothing scripted for this read".to_string()) })
    }

    fn subscribe(
        &self,
        char_uuid: Uuid128,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> BoxFuture<Result<(), String>> {
        let ok = self.present.contains(&char_uuid);
        if ok {
            self.subscribers.lock().unwrap().push((char_uuid, tx));
        }
        Box::pin(async move {
            if ok {
                Ok(())
            } else {
                Err("no such characteristic".to_string())
            }
        })
    }

    fn peer(&self) -> PeerAddr {
        PeerAddr([0xFA, 0xCE, 0x00, 0x00, 0x00, 0x01])
    }

    fn has(&self, char_uuid: Uuid128) -> bool {
        self.present.contains(&char_uuid)
    }

    fn disconnect(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn is_connected(&self) -> BoxFuture<bool> {
        let c = self.connected;
        Box::pin(async move { c })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte order is a wire-level detail with no error path: a mirrored
    /// address is a valid-looking address that nothing answers on.
    #[test]
    fn u48_round_trips_and_keeps_printed_order() {
        let a = PeerAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(a.to_u48(), 0x0000_AABB_CCDD_EEFF);
        assert_eq!(PeerAddr::from_u48(0x0000_AABB_CCDD_EEFF), a);
        assert_eq!(a.to_string(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn the_high_two_bytes_are_dropped() {
        // WinRT always zeroes them; be explicit that we don't fold them in.
        assert_eq!(
            PeerAddr::from_u48(0xFFFF_0011_2233_4455),
            PeerAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
        );
    }

    #[test]
    fn a_random_address_survives_a_round_trip() {
        for seed in [0u64, 1, 0x0000_0102_0304_0506, 0x0000_FFFF_FFFF_FFFF] {
            assert_eq!(PeerAddr::from_u48(seed).to_u48(), seed);
        }
    }

    #[test]
    fn a_command_line_quotes_a_path_with_spaces() {
        // The case that matters: the default install location has a space, and
        // an unquoted value launches "C:\Program".
        assert_eq!(
            quoted_command(std::path::Path::new("C:\\Program Files\\Vortex\\vortex.exe")),
            "\"C:\\Program Files\\Vortex\\vortex.exe\""
        );
    }

    #[test]
    fn quoting_is_idempotent_and_unconditional() {
        // Always quoted, even without a space — a conditional rule is one more
        // thing to get wrong, and quotes are harmless here.
        assert_eq!(quoted_command(std::path::Path::new("C:\\v.exe")), "\"C:\\v.exe\"");
        // An already-quoted path must not gain a second pair.
        let once = quoted_command(std::path::Path::new("C:\\v.exe"));
        assert_eq!(quoted_command(std::path::Path::new(&once)), once);
    }

    const PAIRING: Uuid128 = 0x0000_0000_0000_0000_0000_0000_0000_0001;
    const CAPABILITY: Uuid128 = 0x0000_0000_0000_0000_0000_0000_0000_0002;
    const ABSENT: Uuid128 = 0x0000_0000_0000_0000_0000_0000_0000_0099;

    /// A round of the shape the pairing flow uses — read capability, write a
    /// frame without response, receive the notification it provokes — driven
    /// entirely through `&dyn GattLink`.
    ///
    /// This is what the seam is FOR: the same caller runs against BlueZ, WinRT
    /// or this fake, so the protocol flow can be tested with no radio.
    #[tokio::test]
    async fn a_caller_can_drive_the_link_through_the_trait() {
        let mut fake = FakeGattLink::new(vec![PAIRING, CAPABILITY]);
        fake.reads.insert(CAPABILITY, vec![0x01, 0x00, 0x00]);
        let link: &dyn GattLink = &fake;

        assert_eq!(link.read(CAPABILITY).await.unwrap(), vec![0x01, 0x00, 0x00]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        link.subscribe(PAIRING, tx).await.unwrap();
        link.write(PAIRING, b"msg1", false).await.unwrap();

        // The peer answers on the characteristic we wrote to.
        fake.push_notification(PAIRING, b"msg2".to_vec());
        assert_eq!(rx.recv().await.unwrap(), b"msg2".to_vec());

        // Write-without-response is what §9.1 specifies for this path; a
        // caller that flipped it would still "work" against real hardware but
        // pay an ATT ack per frame.
        let writes = fake.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], (PAIRING, b"msg1".to_vec(), false));
    }

    /// An absent characteristic must be reportable BEFORE a write is attempted
    /// — that is how a peer without the audio-signal characteristic keeps
    /// working instead of failing the connect.
    #[tokio::test]
    async fn an_absent_characteristic_is_visible_and_unwritable() {
        let fake = FakeGattLink::new(vec![PAIRING]);
        let link: &dyn GattLink = &fake;
        assert!(link.has(PAIRING));
        assert!(!link.has(ABSENT));
        assert!(link.write(ABSENT, b"x", false).await.is_err());
        assert!(fake.writes.lock().unwrap().is_empty());
    }

    /// The trait has to be usable as a spawned, shared object — that is how the
    /// BLE loop will hold it. Fails to compile if a signature stops being
    /// `Send + Sync` or the futures stop being `Send`.
    #[tokio::test]
    async fn the_link_survives_being_shared_across_tasks() {
        let link: std::sync::Arc<dyn GattLink> =
            std::sync::Arc::new(FakeGattLink::new(vec![PAIRING]));
        let l2 = std::sync::Arc::clone(&link);
        let joined = tokio::spawn(async move {
            l2.write(PAIRING, b"from another task", false).await.unwrap();
            l2.is_connected().await
        })
        .await
        .unwrap();
        assert!(joined);
    }
}
