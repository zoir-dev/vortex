//! BLE central over WinRT — the Windows half of [`BleCentral`] / [`GattLink`].
//!
//! The laptop is central-only: it scans, connects, writes and subscribes, and
//! never advertises or serves a GATT server. That asymmetry is what makes
//! Windows viable, because WinRT's central role is solid while its peripheral
//! role is not.
//!
//! # What WinRT does differently from BlueZ
//!
//! * **No connect call.** `BluetoothLEDevice::FromBluetoothAddressAsync` hands
//!   back a device object without opening a link; the ACL connection is created
//!   lazily by the first GATT operation. So "connected" here means "we resolved
//!   the service", and a failure to connect surfaces as a service-discovery
//!   error rather than a connect error.
//! * **No disconnect call either.** The link drops when the last reference to
//!   the device and its children is released. [`WindowsGattLink::disconnect`]
//!   therefore unsubscribes and drops its handles, and cannot report failure.
//! * **Notifications are a CCCD write plus an event handler**, per
//!   characteristic, rather than a start-notify on a D-Bus object.
//!
//! # Untested
//!
//! None of this has been run. It type-checks against the WinRT metadata for
//! `x86_64-pc-windows-gnu`, which catches wrong signatures and wrong types but
//! nothing about behaviour — BLE cannot be exercised from Linux at all. Treat
//! every "works" claim here as unverified until it runs on real hardware.
//!
//! # Known runtime risk: thread affinity
//!
//! [`ensure_winrt`] joins the apartment on whatever thread calls it, and every
//! entry point calls it. What that does NOT cover is a future that suspends on
//! one tokio worker and resumes on another: the resumed half runs on a thread
//! that may never have been initialized, and only the agile objects are safe to
//! touch from a different thread than they were made on.
//!
//! The likely fix is the pattern this codebase already uses for libsecret and
//! zbus — see `SECRET_RT` in [`crate::core::storage`], a dedicated
//! single-worker runtime that owns all traffic for one subsystem, added after a
//! live runtime freeze. WinRT wants the same treatment: one thread owning every
//! BLE call, with the async API in front of it. That is a bigger change than
//! this file and needs a real Windows box to justify, so it is written down
//! rather than guessed at.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::core::GUID;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEScanningMode,
};
use windows::core::IInspectable;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue,
    GattCommunicationStatus, GattSession, GattValueChangedEventArgs, GattWriteOption,
};
use windows::Devices::Bluetooth::{
    BluetoothAdapter, BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};

use super::ensure_winrt;
use crate::core::ble::{decode_service_data_128, AdvPayload, AD_TYPE_SERVICE_DATA_128};
use crate::core::platform::{AdvCandidate, BleCentral, BoxFuture, GattLink, PeerAddr, Uuid128};

/// Turn our platform-neutral [`Uuid128`] into the WinRT GUID. Both treat the
/// value as the big-endian UUID form, so this is a reinterpretation, not a
/// byte swap.
fn guid(uuid: Uuid128) -> GUID {
    GUID::from_u128(uuid)
}

/// WinRT errors carry an HRESULT and a message; keep both, since "the radio is
/// off" and "the device is out of range" are different HRESULTs and the message
/// alone doesn't always say which.
fn err(context: &str, e: windows::core::Error) -> String {
    format!("{context}: {} ({})", e.message(), e.code().0)
}

pub struct WindowsBleCentral;

impl BleCentral for WindowsBleCentral {
    /// Watch for an advertisement carrying the Vortex service UUID.
    ///
    /// Active scanning on purpose: the phone puts its service UUID in the
    /// advertisement, but a passive scan on Windows can miss the scan-response
    /// payload where a crowded advert spills it.
    fn scan_for_peer(&self, timeout_ms: u64) -> BoxFuture<Result<Option<AdvCandidate>, String>> {
        // The watcher is not agile either, and unlike the write path it has to
        // stay alive for the whole scan — so it gets a thread of its own and
        // never crosses an await. Only the address comes back, over a channel.
        // This also gives the COM object consistent thread affinity, which a
        // non-agile object is entitled to expect.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<
            Result<Option<(u64, AdvPayload, i16, Option<String>)>, String>,
        >();
        std::thread::spawn(move || {
            ensure_winrt();
            let outcome = (|| -> Result<Option<(u64, AdvPayload, i16, Option<String>)>, String> {
                let watcher = BluetoothLEAdvertisementWatcher::new()
                    .map_err(|e| err("advertisement watcher", e))?;
                watcher
                    .SetScanningMode(BluetoothLEScanningMode::Active)
                    .map_err(|e| err("scanning mode", e))?;

                // `recv_timeout` gives us the scan deadline for free. The
                // sender sits behind a Mutex because WinRT may invoke the
                // handler from any thread, so it must be Sync as well as Send.
                let (hit_tx, hit_rx) =
                    std::sync::mpsc::channel::<(u64, AdvPayload, i16, Option<String>)>();
                let hit_tx = std::sync::Mutex::new(hit_tx);
                let handler = TypedEventHandler::<
                    BluetoothLEAdvertisementWatcher,
                    BluetoothLEAdvertisementReceivedEventArgs,
                >::new(move |_watcher, args| {
                    // windows 0.62 passes `Ref<'_, T>`; `ok()` turns a null
                    // sender into the same error path as any WinRT failure.
                    let args = args.ok()?;
                    let addr = args.BluetoothAddress()?;
                    let rssi = args.RawSignalStrengthInDBm().unwrap_or(0);

                    // Matching `ServiceUuids` would only say "a Vortex phone is
                    // nearby"; the pairing and reconnect flows need the PAYLOAD,
                    // which rides in the service-data section. So walk the raw
                    // AD sections and let the protocol layer decide — same
                    // §5.2 filter both platforms use.
                    let advertisement = args.Advertisement()?;
                    // Cosmetic, for the pairing radar's row label. Absent from
                    // most adverts — the phone spends its name budget on the
                    // service data — and an empty HSTRING is "no name", not "".
                    let local_name = match advertisement.LocalName() {
                        Ok(n) if !n.is_empty() => Some(n.to_string_lossy()),
                        _ => None,
                    };
                    for section in advertisement.DataSections()? {
                        if section.DataType()? != AD_TYPE_SERVICE_DATA_128 {
                            continue;
                        }
                        let buffer = section.Data()?;
                        let len = buffer.Length()? as usize;
                        let reader = DataReader::FromBuffer(&buffer)?;
                        let mut bytes = vec![0u8; len];
                        reader.ReadBytes(&mut bytes)?;
                        if let Some(payload) = decode_service_data_128(&bytes) {
                            // A closed channel means we already have our
                            // answer; dropping this advert is right then.
                            if let Ok(g) = hit_tx.lock() {
                                let _ = g.send((addr, payload, rssi, local_name));
                            }
                            break;
                        }
                    }
                    Ok(())
                });
                let token = watcher
                    .Received(&handler)
                    .map_err(|e| err("watcher.Received", e))?;
                watcher.Start().map_err(|e| err("watcher.Start", e))?;

                let hit = hit_rx
                    .recv_timeout(std::time::Duration::from_millis(timeout_ms))
                    .ok();

                // Stop before returning either way — a watcher left running
                // keeps the radio scanning and Windows won't clean it up.
                let _ = watcher.Stop();
                let _ = watcher.RemoveReceived(token);
                Ok(hit)
            })();
            let _ = done_tx.send(outcome);
        });

        Box::pin(async move {
            match done_rx.await {
                Ok(Ok(hit)) => Ok(hit.map(|(addr, payload, rssi, local_name)| AdvCandidate {
                    addr: PeerAddr::from_u48(addr),
                    payload,
                    rssi: Some(rssi),
                    local_name,
                })),
                Ok(Err(e)) => Err(e),
                // The scan thread died without reporting. Treat it as "no peer
                // seen" rather than a hard error: the caller retries anyway,
                // and a scan is a poll, not a commitment.
                Err(_) => Ok(None),
            }
        })
    }

    /// Resolve the device and its Vortex service, and cache every
    /// characteristic we might write to or subscribe on.
    ///
    /// The characteristics are fetched once here rather than per operation:
    /// each `GetCharacteristicsAsync` is a round trip over the air, and the
    /// call path (pairing, then reconnect, then the audio-signal subscribe)
    /// would otherwise pay it repeatedly on a link that is already the slowest
    /// part of the handshake.
    fn connect(&self, addr: PeerAddr) -> BoxFuture<Result<Box<dyn GattLink>, String>> {
        let service_uuid = guid(crate::core::ble::VORTEX_SERVICE_UUID.as_u128());
        Box::pin(async move {
            ensure_winrt();
            let device = BluetoothLEDevice::FromBluetoothAddressAsync(addr.to_u48())
                .map_err(|e| err("FromBluetoothAddressAsync", e))?
                .await
                .map_err(|e| err("FromBluetoothAddressAsync await", e))?;

            let services = device
                .GetGattServicesForUuidAsync(service_uuid)
                .map_err(|e| err("GetGattServicesForUuidAsync", e))?
                .await
                .map_err(|e| err("GetGattServicesForUuidAsync await", e))?;
            let status = services.Status().map_err(|e| err("services.Status", e))?;
            if status != GattCommunicationStatus::Success {
                // This is also where an out-of-range or radio-off failure
                // lands, since WinRT has no connect step of its own.
                return Err(format!("vortex service not reachable: status {status:?}"));
            }
            let service = services
                .Services()
                .map_err(|e| err("services.Services", e))?
                .into_iter()
                .next()
                .ok_or_else(|| "vortex service absent on this device".to_string())?;

            let chars_result = service
                .GetCharacteristicsAsync()
                .map_err(|e| err("GetCharacteristicsAsync", e))?
                .await
                .map_err(|e| err("GetCharacteristicsAsync await", e))?;
            if chars_result.Status().map_err(|e| err("chars.Status", e))?
                != GattCommunicationStatus::Success
            {
                return Err("could not enumerate vortex characteristics".to_string());
            }
            let mut chars: HashMap<Uuid128, GattCharacteristic> = HashMap::new();
            for c in chars_result
                .Characteristics()
                .map_err(|e| err("chars.Characteristics", e))?
            {
                let u = c.Uuid().map_err(|e| err("characteristic.Uuid", e))?;
                chars.insert(u.to_u128(), c);
            }

            // Hold the link open for as long as this object lives.
            //
            // This is the one WinRT behaviour with no BlueZ counterpart at all,
            // and the reason the first real pairing attempt failed. Windows has
            // no connect call and no disconnect call: it opens the ACL for a
            // GATT operation and closes it again once it decides nothing needs
            // it. Holding the device, its service and a subscribed
            // characteristic does NOT count as needing it — an idle link is
            // dropped in seconds.
            //
            // Every frame of the XX handshake is a write or a prompt reply, so
            // the link survived all of it. Then the flow waits for the phone's
            // user to compare three emoji and tap Approve, which is the first
            // idle gap in the protocol — Windows tore the link down inside it,
            // the phone's approval notification had nowhere to land, and the
            // handshake died 60 s later on `timeout: peer approval` while the
            // phone showed the laptop as "Disconnected".
            //
            // `MaintainConnection` is what tells the OS otherwise. It has to be
            // a `GattSession` we keep: dropping the session drops the intent
            // with it.
            let session = GattSession::FromDeviceIdAsync(
                &device
                    .BluetoothDeviceId()
                    .map_err(|e| err("BluetoothDeviceId", e))?,
            )
            .map_err(|e| err("GattSession::FromDeviceIdAsync", e))?
            .await
            .map_err(|e| err("GattSession::FromDeviceIdAsync await", e))?;
            session
                .SetMaintainConnection(true)
                .map_err(|e| err("SetMaintainConnection", e))?;

            // Say so when the link goes up or down. Without this a dropped link
            // is invisible until some later operation times out, and a timeout
            // cannot distinguish "the peer never answered" from "there was no
            // longer a connection to answer over" — the exact ambiguity that
            // made the failure above take a round trip to hardware to explain.
            let status_handler = TypedEventHandler::<BluetoothLEDevice, IInspectable>::new(
                move |dev, _| {
                    let dev = dev.ok()?;
                    tracing::info!(status = ?dev.ConnectionStatus()?, "BLE link state changed");
                    Ok(())
                },
            );
            let status_token = device
                .ConnectionStatusChanged(&status_handler)
                .map_err(|e| err("ConnectionStatusChanged", e))?;

            Ok(Box::new(WindowsGattLink {
                addr,
                device,
                session,
                status_token,
                chars,
                subscriptions: Mutex::new(Vec::new()),
            }) as Box<dyn GattLink>)
        })
    }

    /// Addresses of paired BLE devices, for the reconnect fast path.
    ///
    /// Each entry costs a `FromIdAsync` because the address is not in the
    /// `DeviceInformation`. The alternative — parsing it out of the device Id
    /// string — depends on an undocumented format, and a wrong parse here means
    /// dialing a made-up address.
    fn bonded(&self) -> BoxFuture<Result<Vec<PeerAddr>, String>> {
        Box::pin(async move {
            ensure_winrt();
            let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true)
                .map_err(|e| err("GetDeviceSelectorFromPairingState", e))?;
            let found = DeviceInformation::FindAllAsyncAqsFilter(&selector)
                .map_err(|e| err("FindAllAsyncAqsFilter", e))?
                .await
                .map_err(|e| err("FindAllAsyncAqsFilter await", e))?;

            // Collect the ids BEFORE any await: the WinRT collection iterator
            // is not agile, so holding one across an await makes this future
            // non-Send and it can't be spawned. `HSTRING` is agile, so the ids
            // themselves travel fine.
            let ids: Vec<windows::core::HSTRING> = found
                .into_iter()
                .filter_map(|info| info.Id().ok())
                .collect();

            let mut out = Vec::new();
            for id in ids {
                // Skip anything that won't open rather than failing the whole
                // list: one stale pairing record must not hide the others.
                let Ok(op) = BluetoothLEDevice::FromIdAsync(&id) else {
                    continue;
                };
                let Ok(dev) = op.await else { continue };
                if let Ok(addr) = dev.BluetoothAddress() {
                    out.push(PeerAddr::from_u48(addr));
                }
            }
            Ok(out)
        })
    }

    fn adapter_ready(&self) -> BoxFuture<bool> {
        Box::pin(async move {
            ensure_winrt();
            let Ok(op) = BluetoothAdapter::GetDefaultAsync() else {
                return false;
            };
            let Ok(adapter) = op.await else { return false };
            // Present is not enough: a machine can have a Bluetooth radio with
            // no LE support, and every call we make is LE.
            adapter.IsLowEnergySupported().unwrap_or(false)
        })
    }
}

/// An open GATT link, holding the device plus its resolved characteristics.
///
/// `Arc<Mutex<..>>`-free for the characteristic map: it is written once during
/// [`BleCentral::connect`] and read-only afterwards. Only the subscription
/// tokens need a lock, because unsubscribing happens on a different thread from
/// the one that subscribed.
pub struct WindowsGattLink {
    /// Kept from the connect call rather than re-read from the device: the
    /// property read can fail, and a link always knows who it dialled.
    addr: PeerAddr,
    device: BluetoothLEDevice,
    /// Held, not used: this is the `MaintainConnection` intent that stops
    /// Windows dropping an idle link out from under the protocol. See
    /// [`BleCentral::connect`] for what happens without it.
    session: GattSession,
    /// So the link-state logging can be unhooked on the way down.
    status_token: i64,
    chars: HashMap<Uuid128, GattCharacteristic>,
    subscriptions: Mutex<Vec<(GattCharacteristic, i64)>>,
}

impl WindowsGattLink {
    fn characteristic(&self, uuid: Uuid128) -> Result<GattCharacteristic, String> {
        self.chars
            .get(&uuid)
            .cloned()
            .ok_or_else(|| format!("characteristic {:032x} not on this device", uuid))
    }
}

impl GattLink for WindowsGattLink {
    fn write(
        &self,
        char_uuid: Uuid128,
        data: &[u8],
        with_response: bool,
    ) -> BoxFuture<Result<(), String>> {
        let c = self.characteristic(char_uuid);
        let bytes = data.to_vec();
        Box::pin(async move {
            ensure_winrt();
            let c = c?;
            let option = if with_response {
                GattWriteOption::WriteWithResponse
            } else {
                GattWriteOption::WriteWithoutResponse
            };
            // `DataWriter` and `IBuffer` are NOT agile — they hold a raw COM
            // pointer that isn't `Send` — so they must not be alive across the
            // await, or this future can't be spawned. Build them, hand the
            // buffer to WinRT, and let the scope drop them before we suspend;
            // the returned `IAsyncOperation` IS agile and travels fine.
            //
            // `WriteValueWithResult...`, not `WriteValueAsync`: the former
            // reports the protocol error, and a silent write failure on the
            // handshake path would look like the phone never answering.
            // `...AndOptionAsync` is the overload that takes a write option;
            // plain `WriteValueWithResultAsync` always writes WITH response,
            // which would stall the unacknowledged frame path.
            let op = {
                let writer = DataWriter::new().map_err(|e| err("DataWriter", e))?;
                writer
                    .WriteBytes(&bytes)
                    .map_err(|e| err("DataWriter.WriteBytes", e))?;
                let buffer = writer
                    .DetachBuffer()
                    .map_err(|e| err("DataWriter.DetachBuffer", e))?;
                c.WriteValueWithResultAndOptionAsync(&buffer, option)
                    .map_err(|e| err("WriteValueWithResultAndOptionAsync", e))?
            };
            let result = op
                .await
                .map_err(|e| err("WriteValueWithResultAndOptionAsync await", e))?;
            let status = result.Status().map_err(|e| err("write status", e))?;
            if status != GattCommunicationStatus::Success {
                return Err(format!("gatt write failed: status {status:?}"));
            }
            Ok(())
        })
    }

    fn read(&self, char_uuid: Uuid128) -> BoxFuture<Result<Vec<u8>, String>> {
        let c = self.characteristic(char_uuid);
        Box::pin(async move {
            ensure_winrt();
            let c = c?;
            // Uncached: the capability read happens at connect time and must
            // reflect the peer we are talking to now, not whatever Windows
            // cached from a previous session with an older phone build.
            let op = c
                .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)
                .map_err(|e| err("ReadValueWithCacheModeAsync", e))?;
            let result = op.await.map_err(|e| err("read await", e))?;
            let status = result.Status().map_err(|e| err("read status", e))?;
            if status != GattCommunicationStatus::Success {
                return Err(format!("gatt read failed: status {status:?}"));
            }
            // The buffer is non-agile, but there is no await left after this
            // point, so it never has to be `Send`.
            let buffer = result.Value().map_err(|e| err("read value", e))?;
            let len = buffer.Length().map_err(|e| err("buffer length", e))? as usize;
            let reader = DataReader::FromBuffer(&buffer).map_err(|e| err("DataReader", e))?;
            let mut bytes = vec![0u8; len];
            reader
                .ReadBytes(&mut bytes)
                .map_err(|e| err("DataReader.ReadBytes", e))?;
            Ok(bytes)
        })
    }

    /// Subscribe to notifications on `char_uuid`.
    ///
    /// Order matters: register the handler BEFORE writing the CCCD. The phone
    /// pushes state the moment it sees the subscribe, and a notification that
    /// arrives between the descriptor write and the handler registration is
    /// simply lost — on Linux that showed up as a missing first state push.
    fn subscribe(
        &self,
        char_uuid: Uuid128,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> BoxFuture<Result<(), String>> {
        let c = self.characteristic(char_uuid);
        let subs = &self.subscriptions;
        let registered: Result<(GattCharacteristic, i64), String> = (|| {
            let c = c?;
            let handler = TypedEventHandler::<GattCharacteristic, GattValueChangedEventArgs>::new(
                move |_c, args| {
                    let args = args.ok()?;
                    let buffer = args.CharacteristicValue()?;
                    let len = buffer.Length()? as usize;
                    let reader = DataReader::FromBuffer(&buffer)?;
                    let mut bytes = vec![0u8; len];
                    reader.ReadBytes(&mut bytes)?;
                    // A closed receiver means the consumer went away; the link
                    // is torn down separately, so drop the frame quietly.
                    let _ = tx.send(bytes);
                    Ok(())
                },
            );
            let token = c
                .ValueChanged(&handler)
                .map_err(|e| err("ValueChanged", e))?;
            Ok((c, token))
        })();
        let (c, token) = match registered {
            Ok(v) => v,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        if let Ok(mut g) = subs.lock() {
            g.push((c.clone(), token));
        }
        Box::pin(async move {
            let status = c
                .WriteClientCharacteristicConfigurationDescriptorAsync(
                    GattClientCharacteristicConfigurationDescriptorValue::Notify,
                )
                .map_err(|e| err("CCCD write", e))?
                .await
                .map_err(|e| err("CCCD write await", e))?;
            if status != GattCommunicationStatus::Success {
                return Err(format!("subscribe failed: status {status:?}"));
            }
            Ok(())
        })
    }

    /// Unsubscribe and release our handles.
    ///
    /// There is no disconnect API: Windows drops the link when the last
    /// reference to the device goes away. We can only stop notifications and
    /// let go — so this reports the CCCD write, and nothing about the link
    /// itself.
    fn disconnect(&self) -> BoxFuture<Result<(), String>> {
        let taken: Vec<(GattCharacteristic, i64)> = self
            .subscriptions
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        // Drop the keep-alive intent explicitly. Releasing the session object
        // would do it too, but the whole point of that field is that it is easy
        // to lose track of by accident — so the teardown says it out loud.
        let _ = self.session.SetMaintainConnection(false);
        let _ = self.device.RemoveConnectionStatusChanged(self.status_token);
        Box::pin(async move {
            for (c, token) in taken {
                let _ = c.RemoveValueChanged(token);
                // Best-effort: if the phone is already gone this write fails,
                // which is not an error worth surfacing on the way down.
                if let Ok(op) = c.WriteClientCharacteristicConfigurationDescriptorAsync(
                    GattClientCharacteristicConfigurationDescriptorValue::None,
                ) {
                    let _ = op.await;
                }
            }
            Ok(())
        })
    }

    fn peer(&self) -> PeerAddr {
        self.addr
    }

    /// Resolved at connect time, so this is a local lookup — a peer that
    /// predates the audio-signal characteristic simply doesn't have it.
    fn has(&self, char_uuid: Uuid128) -> bool {
        self.chars.contains_key(&char_uuid)
    }

    /// A property read, not a round trip — but async to match the trait, which
    /// Linux needs because BlueZ answers over D-Bus.
    fn is_connected(&self) -> BoxFuture<bool> {
        let connected = self
            .device
            .ConnectionStatus()
            .map(|s| s == BluetoothConnectionStatus::Connected)
            .unwrap_or(false);
        Box::pin(async move { connected })
    }
}

// No `unsafe impl Send/Sync` here on purpose. windows-rs marks the WinRT types
// that metadata says are agile — `BluetoothLEDevice`, `GattCharacteristic`,
// `IAsyncOperation` — as `Send + Sync` itself, so this struct derives both. The
// non-agile ones (`DataWriter`, `IBuffer`, the advertisement watcher) are kept
// off every await path above instead of being asserted safe. An `unsafe impl`
// would compile just as well and silence the next real violation.

/// Keeps the type usable through `Arc` in the same places the Linux side is.
pub fn central() -> Arc<dyn BleCentral> {
    Arc::new(WindowsBleCentral)
}
