//! Bluetooth HID (Human Interface Device) Server & HID Report Encoder.
//!
//! Provides an ADB-free Universal Control transport for Linux to Android.
//! Emulates a Bluetooth HID Combo Device (Mouse + Keyboard) using BlueZ
//! `org.bluez.Profile1` DBus interface.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Standard Bluetooth HID Service UUID (`00001124-0000-1000-8000-00805f9b34fb`).
pub const HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";

/// SDP Record XML for a Mouse + Keyboard Combo HID Device.
pub const SDP_RECORD: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001">
    <sequence>
      <uuid value="0x1124" />
    </sequence>
  </attribute>
  <attribute id="0x0004">
    <sequence>
      <sequence>
        <uuid value="0x0100" />
        <uint16 value="0x0011" />
      </sequence>
      <sequence>
        <uuid value="0x00a1" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0005">
    <sequence>
      <uuid value="0x1002" />
    </sequence>
  </attribute>
  <attribute id="0x000d">
    <sequence>
      <sequence>
        <uuid value="0x0100" />
        <uint16 value="0x0013" />
      </sequence>
      <sequence>
        <uuid value="0x00a1" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0100">
    <text value="Vortex Universal Control" />
  </attribute>
  <attribute id="0x0201">
    <uint16 value="0x0100" />
  </attribute>
  <attribute id="0x0202">
    <uint8 value="0x08" />
  </attribute>
  <attribute id="0x0203">
    <uint8 value="0x00" />
  </attribute>
  <attribute id="0x0204">
    <bool value="true" />
  </attribute>
  <attribute id="0x0205">
    <bool value="true" />
  </attribute>
  <attribute id="0x0206">
    <sequence>
      <sequence>
        <uint8 value="0x22" />
        <text value="05010906a1018501050719e029e71500250175019508810295017508810395067508150025650507190029658100c005010902a10185020901a1000509190129051500250175019505810275039501810305010930093109381581257f750895038106c0c0" />
      </sequence>
    </sequence>
  </attribute>
</record>"#;

/// Formats Mouse and Keyboard state into raw Bluetooth HID Input Report frames (`0xa1`).
pub struct HidReportEncoder {
    buttons: u8,
    pressed_keys: HashSet<u8>,
    modifiers: u8,
}

impl Default for HidReportEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HidReportEncoder {
    pub fn new() -> Self {
        Self {
            buttons: 0,
            pressed_keys: HashSet::new(),
            modifiers: 0,
        }
    }

    /// Set mouse button state: button (0=left, 1=right, 2=middle, 3=back, 4=forward), pressed (bool).
    pub fn set_button(&mut self, button: u8, pressed: bool) {
        if button < 5 {
            if pressed {
                self.buttons |= 1 << button;
            } else {
                self.buttons &= !(1 << button);
            }
        }
    }

    /// Encode mouse movement into a 6-byte Bluetooth HID report frame (`[0xa1, 0x02, buttons, dx, dy, wheel]`).
    pub fn encode_mouse(&self, dx: i8, dy: i8, wheel: i8) -> [u8; 6] {
        [
            0xa1,         // DATA Input Report
            0x02,         // Mouse Report ID
            self.buttons, // Bitmask of buttons
            dx as u8,     // Relative X
            dy as u8,     // Relative Y
            wheel as u8,  // Scroll wheel
        ]
    }

    /// Handle evdev keypress/release and encode keyboard state into a 10-byte Bluetooth HID report frame.
    pub fn handle_key(&mut self, evdev_code: u16, pressed: bool) -> Option<[u8; 10]> {
        if let Some(modifier_bit) = evdev_to_modifier(evdev_code) {
            if pressed {
                self.modifiers |= modifier_bit;
            } else {
                self.modifiers &= !modifier_bit;
            }
        } else if let Some(hid_code) = evdev_to_usb_hid(evdev_code) {
            if pressed {
                self.pressed_keys.insert(hid_code);
            } else {
                self.pressed_keys.remove(&hid_code);
            }
        } else {
            return None;
        }

        let mut report = [0u8; 10];
        report[0] = 0xa1; // DATA Input Report
        report[1] = 0x01; // Keyboard Report ID
        report[2] = self.modifiers;
        report[3] = 0x00; // Reserved

        let mut idx = 4;
        for &key in &self.pressed_keys {
            if idx < 10 {
                report[idx] = key;
                idx += 1;
            }
        }

        Some(report)
    }
}

/// Convert Linux Evdev modifier keycodes to USB HID modifier bitmask flags.
fn evdev_to_modifier(evdev_code: u16) -> Option<u8> {
    match evdev_code {
        29 => Some(1 << 0),  // KEY_LEFTCTRL
        42 => Some(1 << 1),  // KEY_LEFTSHIFT
        56 => Some(1 << 2),  // KEY_LEFTALT
        125 => Some(1 << 3), // KEY_LEFTMETA (Super/Cmd)
        97 => Some(1 << 4),  // KEY_RIGHTCTRL
        54 => Some(1 << 5),  // KEY_RIGHTSHIFT
        100 => Some(1 << 6), // KEY_RIGHTALT
        126 => Some(1 << 7), // KEY_RIGHTMETA
        _ => None,
    }
}

/// Map common Linux Evdev keycodes to USB HID Keyboard Usage IDs.
pub fn evdev_to_usb_hid(evdev_code: u16) -> Option<u8> {
    match evdev_code {
        30 => Some(0x04),  // A
        48 => Some(0x05),  // B
        46 => Some(0x06),  // C
        32 => Some(0x07),  // D
        18 => Some(0x08),  // E
        33 => Some(0x09),  // F
        34 => Some(0x0a),  // G
        35 => Some(0x0b),  // H
        23 => Some(0x0c),  // I
        36 => Some(0x0d),  // J
        37 => Some(0x0e),  // K
        38 => Some(0x0f),  // L
        50 => Some(0x10),  // M
        49 => Some(0x11),  // N
        24 => Some(0x12),  // O
        25 => Some(0x13),  // P
        16 => Some(0x14),  // Q
        19 => Some(0x15),  // R
        31 => Some(0x16),  // S
        20 => Some(0x17),  // T
        22 => Some(0x18),  // U
        47 => Some(0x19),  // V
        17 => Some(0x1a),  // W
        45 => Some(0x1b),  // X
        21 => Some(0x1c),  // Y
        44 => Some(0x1d),  // Z
        2..=10 => Some(0x1e + (evdev_code as u8 - 2)), // 1-9
        11 => Some(0x27),  // 0
        28 => Some(0x28),  // Return/Enter
        1 => Some(0x29),   // Escape
        14 => Some(0x2a),  // Backspace
        15 => Some(0x2b),  // Tab
        57 => Some(0x2c),  // Space
        12 => Some(0x2d),  // Minus (-)
        13 => Some(0x2e),  // Equal (=)
        26 => Some(0x2f),  // LeftBracket ([)
        27 => Some(0x30),  // RightBracket (])
        43 => Some(0x31),  // Backslash (\)
        39 => Some(0x33),  // Semicolon (;)
        40 => Some(0x34),  // Apostrophe (')
        41 => Some(0x35),  // Grave (`)
        51 => Some(0x36),  // Comma (,)
        52 => Some(0x37),  // Dot (.)
        53 => Some(0x38),  // Slash (/)
        106 => Some(0x4f), // Right Arrow
        105 => Some(0x50), // Left Arrow
        108 => Some(0x51), // Down Arrow
        103 => Some(0x52), // Up Arrow
        _ => None,
    }
}

use std::os::fd::{AsRawFd, FromRawFd};
use zbus::zvariant::{Fd, ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{connection::Connection, interface};

/// BlueZ DBus Profile1 interface implementation for Bluetooth HID.
pub struct BluezHidProfile {
    pub active_stream: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
    pub connected: Arc<AtomicBool>,
}

#[interface(name = "org.bluez.Profile1")]
impl BluezHidProfile {
    async fn new_connection(
        &self,
        device: ObjectPath<'_>,
        fd: Fd<'_>,
        properties: std::collections::HashMap<String, Value<'_>>,
    ) {
        tracing::info!(?device, ?properties, "BlueZ HID: New connection received!");
        let raw_fd: std::os::unix::io::RawFd = fd.as_raw_fd();
        let dup_fd = unsafe { libc::dup(raw_fd) };
        if dup_fd >= 0 {
            unsafe {
                let std_stream = std::os::unix::net::UnixStream::from_raw_fd(dup_fd);
                let _ = std_stream.set_nonblocking(true);
                if let Ok(async_stream) = UnixStream::from_std(std_stream) {
                    let mut g = self.active_stream.lock().await;
                    *g = Some(async_stream);
                    self.connected.store(true, Ordering::SeqCst);
                    tracing::info!("BlueZ HID: active_stream stored and connected = true");
                }
            }
        }
        let dev_path = device.as_str().to_string();
        tokio::spawn(async move {
            if let Ok(path) = zbus::zvariant::OwnedObjectPath::try_from(dev_path) {
                if let Ok(conn) = zbus::Connection::system().await {
                    if let Ok(proxy) = zbus::Proxy::new(&conn, "org.bluez", path, "org.freedesktop.DBus.Properties").await {
                        let _ : Result<(), _> = proxy.call("Set", &("org.bluez.Device1", "Trusted", Value::from(true))).await;
                    }
                }
            }
        });
    }

    async fn request_disconnection(&self, device: ObjectPath<'_>) {
        tracing::info!(?device, "BlueZ HID: Disconnected");
        self.connected.store(false, Ordering::SeqCst);
        let mut g = self.active_stream.lock().await;
        *g = None;
    }

    async fn release(&self) {
        tracing::info!("BlueZ HID: Profile released");
        self.connected.store(false, Ordering::SeqCst);
    }
}

/// State holder for Bluetooth HID device emulation.
#[derive(Clone)]
pub struct BtHidServer {
    encoder: Arc<Mutex<HidReportEncoder>>,
    active_stream: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
    connected: Arc<AtomicBool>,
    registered: Arc<AtomicBool>,
}

impl Default for BtHidServer {
    fn default() -> Self {
        Self::new()
    }
}

impl BtHidServer {
    pub fn new() -> Self {
        Self {
            encoder: Arc::new(Mutex::new(HidReportEncoder::new())),
            active_stream: Arc::new(tokio::sync::Mutex::new(None)),
            connected: Arc::new(AtomicBool::new(false)),
            registered: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn is_registered(&self) -> bool {
        self.registered.load(Ordering::SeqCst)
    }

    /// Register BlueZ HID Profile on DBus.
    pub async fn register(&self, connection: &Connection) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let profile = BluezHidProfile {
            active_stream: self.active_stream.clone(),
            connected: self.connected.clone(),
        };

        let profile_path = "/com/vortex/bt_hid_profile";
        let _ = connection.object_server().at(profile_path, profile).await;

        let manager = zbus::Proxy::new(
            connection,
            "org.bluez",
            "/org/bluez",
            "org.bluez.ProfileManager1",
        )
        .await?;

        let mut options = std::collections::HashMap::new();
        options.insert("Name", Value::from("Vortex Universal Control"));
        options.insert("Role", Value::from("server"));
        options.insert("ServiceRecord", Value::from(SDP_RECORD));
        options.insert("RequireAuthentication", Value::from(false));
        options.insert("RequireAuthorization", Value::from(false));
        options.insert("AutoConnect", Value::from(true));

        let path_val = ObjectPath::try_from(profile_path)?;
        let _ = manager.call::<_, _, ()>("UnregisterProfile", &(path_val.clone(),)).await;

        let reg_res: Result<(), zbus::Error> = manager
            .call("RegisterProfile", &(path_val, HID_UUID, options))
            .await;

        if let Err(e) = reg_res {
            let err_msg = e.to_string();
            if err_msg.contains("already registered") {
                tracing::info!("BlueZ HID Profile already registered on system.");
                self.registered.store(true, Ordering::SeqCst);
                return Ok(());
            }
            return Err(Box::new(e));
        }

        self.registered.store(true, Ordering::SeqCst);
        tracing::info!("BlueZ HID Profile registered successfully!");
        Ok(())
    }

    /// Attempt to trigger Connect on paired Classic Bluetooth devices so HID connects seamlessly.
    pub async fn try_connect_paired_devices(&self, connection: &Connection) {
        if self.is_connected() {
            return;
        }
        let object_manager = match zbus::Proxy::new(
            connection,
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
        )
        .await {
            Ok(om) => om,
            Err(e) => {
                tracing::debug!("BlueZ ObjectManager not available: {e}");
                return;
            }
        };

        let objects: Result<std::collections::HashMap<OwnedObjectPath, std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>>>, zbus::Error> =
            object_manager.call("GetManagedObjects", &()).await;

        if let Ok(objs) = objects {
            for (path, ifaces) in objs {
                if let Some(dev_props) = ifaces.get("org.bluez.Device1") {
                    let is_paired = dev_props.get("Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
                    let is_bonded = dev_props.get("Bonded").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
                    let is_connected = dev_props.get("Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
                    let name = dev_props.get("Name").and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default();
                    let alias = dev_props.get("Alias").and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default();
                    let is_target = is_paired || is_bonded || name.to_lowercase().contains("redmi") || alias.to_lowercase().contains("redmi");

                    if is_target && !is_connected {
                        if let Ok(dev_proxy) = zbus::Proxy::new(
                            connection,
                            "org.bluez",
                            path.as_str(),
                            "org.bluez.Device1",
                        ).await {
                            if !is_paired && !is_bonded {
                                tracing::info!(device = %path, "Auto-pairing with Vortex device over Bluetooth");
                                let _ : Result<(), _> = dev_proxy.call("Pair", &()).await;
                            }
                            tracing::info!(device = %path, "Attempting Connect for Classic Bluetooth HID");
                            let res: Result<(), _> = dev_proxy.call("ConnectProfile", &(HID_UUID,)).await;
                            if res.is_err() {
                                let _ : Result<(), _> = dev_proxy.call("Connect", &()).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Send relative mouse movement over Bluetooth HID.
    pub async fn send_mouse(&self, dx: i8, dy: i8, wheel: i8) -> bool {
        let frame = {
            let enc = self.encoder.lock().unwrap_or_else(|e| e.into_inner());
            enc.encode_mouse(dx, dy, wheel)
        };
        self.send_frame(&frame).await
    }

    /// Send mouse button press/release over Bluetooth HID.
    pub async fn send_button(&self, button: u8, pressed: bool) -> bool {
        let frame = {
            let mut enc = self.encoder.lock().unwrap_or_else(|e| e.into_inner());
            enc.set_button(button, pressed);
            enc.encode_mouse(0, 0, 0)
        };
        self.send_frame(&frame).await
    }

    /// Send keyboard event over Bluetooth HID.
    pub async fn send_key(&self, evdev_code: u16, pressed: bool) -> bool {
        let frame_opt = {
            let mut enc = self.encoder.lock().unwrap_or_else(|e| e.into_inner());
            enc.handle_key(evdev_code, pressed)
        };

        if let Some(frame) = frame_opt {
            self.send_frame(&frame).await
        } else {
            false
        }
    }

    async fn send_frame(&self, frame: &[u8]) -> bool {
        let mut stream_guard = self.active_stream.lock().await;
        if let Some(stream) = stream_guard.as_mut() {
            if stream.write_all(frame).await.is_ok() {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mouse_report_encoding() {
        let mut encoder = HidReportEncoder::new();
        let report_idle = encoder.encode_mouse(0, 0, 0);
        assert_eq!(report_idle, [0xa1, 0x02, 0x00, 0x00, 0x00, 0x00]);

        // Left button down + relative movement
        encoder.set_button(0, true);
        let report_move = encoder.encode_mouse(10, -5, 1);
        assert_eq!(report_move, [0xa1, 0x02, 0x01, 0x0a, 0xfb, 0x01]);
    }

    #[test]
    fn test_keyboard_report_encoding() {
        let mut encoder = HidReportEncoder::new();

        // Press Left Ctrl (evdev 29) -> modifier bit 0 (0x01)
        let report_ctrl = encoder.handle_key(29, true).unwrap();
        assert_eq!(report_ctrl[2], 0x01);

        // Press 'A' (evdev 30) -> USB HID 0x04
        let report_a = encoder.handle_key(30, true).unwrap();
        assert_eq!(report_a[2], 0x01);
        assert_eq!(report_a[4], 0x04);

        // Release 'A'
        let report_release = encoder.handle_key(30, false).unwrap();
        assert_eq!(report_release[2], 0x01);
        assert_eq!(report_release[4], 0x00);
    }
}
