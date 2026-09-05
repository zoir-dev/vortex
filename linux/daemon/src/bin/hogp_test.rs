//! Spike: can this laptop act as a **BLE** HID device (HOGP) for the phone?
//!
//! Why this exists. The Classic-Bluetooth HID path (`bt_hid.rs`) is blocked on
//! two changes we are not willing to make to a user's system:
//!
//!   * the adapter's Class of Device must say "peripheral", and `Class` is
//!     read-only on `org.bluez.Adapter1` — changing it means editing
//!     `/etc/bluetooth/main.conf` as root and restarting `bluetoothd`, which
//!     drops every Bluetooth connection including the user's earbuds;
//!   * BlueZ's `input` plugin very likely has to be disabled, which takes away
//!     Bluetooth mouse/keyboard support from every user who has one.
//!
//! HOGP avoids both: it rides BLE, so the Classic `input` plugin is untouched,
//! and a BLE peripheral declares what it is through the Appearance field in its
//! own advertisement — set at runtime, no root, no system file, nothing left
//! behind when we exit.
//!
//! This binary proves (or disproves) the idea in isolation, with no vortex
//! wiring: publish the three services HOGP requires, advertise as a mouse, and
//! push pointer reports once the phone subscribes.
//!
//! Run:  cargo run --features dev-tools --bin vortex-hogp-test
//! Then: pair from the phone's Bluetooth settings.
//! Pass:  a cursor appears on the phone and moves on its own.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bluer::adv::Advertisement;
use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, Descriptor, DescriptorRead,
    Service,
};
use bluer::Uuid;
use futures::FutureExt;
use tokio::sync::Mutex;

/// Expand a 16-bit assigned number into the full Bluetooth base UUID.
fn uuid16(v: u16) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_1000_8000_0080_5f9b_34fb_u128 | ((v as u128) << 96))
}

// ---- Assigned numbers we need (Bluetooth SIG) ----
const SVC_DEVICE_INFO: u16 = 0x180a;
const SVC_BATTERY: u16 = 0x180f;
const SVC_HID: u16 = 0x1812;

const CHR_BATTERY_LEVEL: u16 = 0x2a19;
const CHR_PNP_ID: u16 = 0x2a50;
const CHR_HID_INFO: u16 = 0x2a4a;
const CHR_REPORT_MAP: u16 = 0x2a4b;
const CHR_HID_CONTROL_POINT: u16 = 0x2a4c;
const CHR_REPORT: u16 = 0x2a4d;
const CHR_PROTOCOL_MODE: u16 = 0x2a4e;
/// Report Reference — tells the host which report this characteristic carries.
const DSC_REPORT_REFERENCE: u16 = 0x2908;

/// Appearance: Human Interface Device / Mouse. This is the field that makes
/// Android offer the laptop as a pointing device — the BLE stand-in for the
/// Class of Device we cannot set on the Classic path.
const APPEARANCE_MOUSE: u16 = 0x03c2;

/// The advertised name. Keep it SHORT: a legacy advertisement carries 31 bytes
/// total, and flags (3) + the 16-bit service UUID (4) + appearance (4) already
/// spend 11. "Vortex Laptop Mouse" wanted 21 more — 32 in all — leaving BlueZ
/// no room for the name.
const ADV_NAME: &str = "Vortex Mouse";

/// Report ID 1, matching the descriptor below and the Report Reference.
const REPORT_ID_MOUSE: u8 = 0x01;

/// A plain 3-button relative mouse: buttons byte, then dx/dy/wheel as signed
/// bytes. Report layout on the wire is `[buttons, dx, dy, wheel]`.
const REPORT_MAP: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xa1, 0x01, // Collection (Application)
    0x85, REPORT_ID_MOUSE, //   Report ID (1)
    0x09, 0x01, //   Usage (Pointer)
    0xa1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (Button 1)
    0x29, 0x03, //     Usage Maximum (Button 3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x03, //     Input (Constant) — padding to a whole byte
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7f, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x03, //     Report Count (3)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xc0, //   End Collection
    0xc0, // End Collection
];

/// A read-only characteristic whose value never changes.
///
/// Every HOGP read is `encrypt_read`: the profile requires an encrypted link,
/// and Android will not treat an unencrypted HID service as usable.
fn const_read(uuid: u16, value: Vec<u8>) -> Characteristic {
    Characteristic {
        uuid: uuid16(uuid),
        read: Some(CharacteristicRead {
            read: true,
            encrypt_read: true,
            fun: Box::new(move |_req| {
                let value = value.clone();
                async move { Ok(value) }.boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> bluer::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    println!("adapter {} [{}]", adapter.name(), adapter.address().await?);

    // The notifier the host subscribes to. Reports are pushed through it once
    // the phone enables notifications on the Report characteristic.
    let notifier = Arc::new(Mutex::new(None));

    let hid_service = Service {
        uuid: uuid16(SVC_HID),
        primary: true,
        characteristics: vec![
            // bcdHID 0x0111, country code 0 (not localised), flags 0x03 =
            // remote-wake + normally-connectable.
            const_read(CHR_HID_INFO, vec![0x11, 0x01, 0x00, 0x03]),
            const_read(CHR_REPORT_MAP, REPORT_MAP.to_vec()),
            // Report protocol (1), not boot protocol (0). Writable because the
            // host is allowed to switch us, but we only ever report in report
            // protocol, so the write is accepted and ignored.
            Characteristic {
                uuid: uuid16(CHR_PROTOCOL_MODE),
                read: Some(CharacteristicRead {
                    read: true,
                    encrypt_read: true,
                    fun: Box::new(|_| async move { Ok(vec![0x01]) }.boxed()),
                    ..Default::default()
                }),
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Fun(Box::new(|v, _| {
                        async move {
                            println!("host set protocol mode -> {v:?}");
                            Ok(())
                        }
                        .boxed()
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // Suspend / exit-suspend from the host. Nothing to do, but the
            // characteristic has to exist or the host rejects the service.
            Characteristic {
                uuid: uuid16(CHR_HID_CONTROL_POINT),
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Fun(Box::new(|v, _| {
                        async move {
                            println!("host wrote control point -> {v:?}");
                            Ok(())
                        }
                        .boxed()
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // The input report itself: read for the host's initial fetch,
            // notify for everything after. The Report Reference descriptor is
            // what ties these bytes to report ID 1 as an *Input* report — omit
            // it and the host has no idea what it just subscribed to.
            Characteristic {
                uuid: uuid16(CHR_REPORT),
                read: Some(CharacteristicRead {
                    read: true,
                    encrypt_read: true,
                    fun: Box::new(|_| async move { Ok(vec![0, 0, 0, 0]) }.boxed()),
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: CharacteristicNotifyMethod::Fun({
                        let notifier = notifier.clone();
                        Box::new(move |n| {
                            let notifier = notifier.clone();
                            async move {
                                println!("*** host subscribed to input reports ***");
                                *notifier.lock().await = Some(n);
                            }
                            .boxed()
                        })
                    }),
                    ..Default::default()
                }),
                descriptors: vec![Descriptor {
                    uuid: uuid16(DSC_REPORT_REFERENCE),
                    read: Some(DescriptorRead {
                        read: true,
                        encrypt_read: true,
                        // [report ID, type] where type 1 = Input.
                        fun: Box::new(|_| async move { Ok(vec![REPORT_ID_MOUSE, 0x01]) }.boxed()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    // HOGP expects Device Information and Battery alongside the HID service.
    // Android reads the PnP ID to name and classify the device; a HID service
    // arriving without these is routinely ignored.
    let app = Application {
        services: vec![
            Service {
                uuid: uuid16(SVC_DEVICE_INFO),
                primary: true,
                characteristics: vec![const_read(
                    CHR_PNP_ID,
                    // Vendor ID source 0x02 (USB IF), VID 0x1d6b (Linux
                    // Foundation), PID 0x0246, version 1.0.0.
                    vec![0x02, 0x6b, 0x1d, 0x46, 0x02, 0x00, 0x01],
                )],
                ..Default::default()
            },
            Service {
                uuid: uuid16(SVC_BATTERY),
                primary: true,
                characteristics: vec![const_read(CHR_BATTERY_LEVEL, vec![100])],
                ..Default::default()
            },
            hid_service,
        ],
        ..Default::default()
    };
    let _app_handle = adapter.serve_gatt_application(app).await?;
    println!("GATT application registered (HID + DIS + Battery)");

    let adv = Advertisement {
        advertisement_type: bluer::adv::Type::Peripheral,
        service_uuids: BTreeSet::from([uuid16(SVC_HID)]),
        discoverable: Some(true),
        local_name: Some(ADV_NAME.to_string()),
        appearance: Some(APPEARANCE_MOUSE),
        ..Default::default()
    };
    let _adv_handle = adapter.advertise(adv).await?;
    println!("advertising as a BLE mouse (appearance 0x{APPEARANCE_MOUSE:04x})");

    println!();
    println!("→ On the phone: Settings → Bluetooth → pair '{ADV_NAME}'.");
    println!("→ PASS if a cursor appears on the phone and drifts on its own.");
    println!("→ Ctrl-C to stop; nothing is left configured on this machine.");
    println!();

    // Once subscribed, walk the pointer in a slow square so movement is
    // obviously ours and not a stray touch.
    let steps: [(i8, i8); 4] = [(6, 0), (0, 6), (-6, 0), (0, -6)];
    let mut i = 0usize;
    loop {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut guard = notifier.lock().await;
        let Some(n) = guard.as_mut() else { continue };
        let (dx, dy) = steps[(i / 25) % steps.len()];
        i += 1;
        // [buttons, dx, dy, wheel]
        if let Err(err) = n.notify(vec![0x00, dx as u8, dy as u8, 0x00]).await {
            println!("notify failed ({err}) — host went away; waiting for a new subscription");
            *guard = None;
        }
    }
}
