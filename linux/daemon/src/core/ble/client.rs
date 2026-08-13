//! BLE GATT client per spec §6.1 and §9.1.
//!
//! L3 acts as the BLE Initiator: it scans, connects, discovers the Vortex
//! service, and reads/writes characteristics on Android's GATT server.

use std::time::Duration;

use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest, Service};
use bluer::gatt::WriteOp;
use bluer::{Adapter, Address, Device, Result as BluerResult};
use futures::future::try_join_all;
use futures::{pin_mut, StreamExt};
use tokio::time::timeout;
use tracing::{debug, info};

use super::frame::{Frame, FrameDecodeError};
use super::{
    AdvDecodeError, AUDIO_SIGNAL_UUID, CAPABILITY_UUID, PAIRING_CONTROL_UUID,
    RECONNECT_CONTROL_UUID, VORTEX_SERVICE_UUID,
};

/// V1 Capability characteristic response (spec §9.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityResponse {
    pub version: u8,
    pub capability_bits: u16,
}

#[derive(Debug)]
pub enum ClientError {
    Bluer(bluer::Error),
    Timeout(&'static str),
    NoVortexService,
    NoCharacteristic(&'static str),
    BadCapabilityResponse {
        len: usize,
    },
    UnsupportedVersion(u8),
    InvalidPayload(AdvDecodeError),
    FrameDecode(FrameDecodeError),
    /// BlueZ brought up the classic (BR/EDR) bearer instead of LE, so no
    /// GATT service can ever appear on the device object. See the bearer
    /// note in `VortexClient::connect` for the mechanism and the way out.
    ClassicBearerOnly,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bluer(e) => write!(f, "bluer: {e}"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::NoVortexService => write!(f, "Vortex Service UUID not found on peer"),
            Self::NoCharacteristic(name) => write!(f, "{name} characteristic not found"),
            Self::BadCapabilityResponse { len } => {
                write!(f, "Capability response wrong length: {len} (expected 3)")
            }
            Self::UnsupportedVersion(v) => write!(f, "unsupported V1 version byte: {v:#04x}"),
            Self::InvalidPayload(e) => write!(f, "invalid advertisement payload: {e:?}"),
            Self::FrameDecode(e) => write!(f, "frame decode: {e}"),
            Self::ClassicBearerOnly => write!(
                f,
                "BlueZ kept the classic (BR/EDR) bearer, so no GATT service is reachable. \
                 This phone is also paired to this laptop as a Bluetooth *audio* device, and \
                 BlueZ always prefers the bonded bearer. Unpair it as an audio device \
                 (`bluetoothctl remove <addr>`), then pair Vortex."
            ),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<bluer::Error> for ClientError {
    fn from(e: bluer::Error) -> Self {
        Self::Bluer(e)
    }
}

/// Connection handle to a peer's Vortex GATT service.
pub struct VortexClient {
    pub address: Address,
    pub service: Service,
    pub capability: Characteristic,
    pub pairing_control: Characteristic,
    pub reconnect_control: Characteristic,
    /// Audio-signal characteristic (Phase 2). `None` for older peers
    /// (A2 / A3 builds before P2.13) that didn't ship the characteristic
    /// yet — those still work, they just don't get the BLE fast-path
    /// and fall back to the 5-s LAN heartbeat.
    pub audio_signal: Option<Characteristic>,
}

impl VortexClient {
    /// Connect to the device at `address`, discover the Vortex Service,
    /// and resolve the four V1 characteristics.
    pub async fn connect(adapter: &Adapter, address: Address) -> Result<Self, ClientError> {
        let device = adapter.device(address)?;
        let t0 = std::time::Instant::now();

        // Bring up an LE/ATT link — which can take TWO `Connect()` calls on a
        // dual-mode phone.
        //
        // `Device1.Connect()` picks the bearer itself, and BlueZ's
        // `select_conn_bearer()` opens with
        //
        // ```text
        // if (bredr_state.prefer || (bredr_state.bonded && !le_state.bonded))
        //         return BDADDR_BREDR;
        // ```
        //
        // Vortex never creates an LE bond on purpose (see `ui-tauri`'s
        // `pairing.rs`: on a dual-mode phone every bond attempt gets routed
        // over BR/EDR and yields no IRK anyway). So as soon as the phone is
        // *also* paired to this laptop as a Bluetooth audio device — an
        // entirely ordinary thing to do — `bredr_state.bonded` is true,
        // `le_state.bonded` is false, and every connect lands on BR/EDR:
        // A2DP/HFP come up, the device object gets zero GATT services, and
        // pairing dies in the discovery loop below. `PreferredBearer = "le"`
        // does NOT rescue this; that clause is evaluated before
        // `le_state.prefer`, and the property is experimental-gated anyway.
        //
        // The way through is BlueZ's documented "connect any disconnected
        // bearer if one is already connected" rule. With BR/EDR up *and* a
        // profile connected, `dev_connect()` takes
        //
        // ```text
        // if (dev->bredr_state.svc_resolved && find_service_with_state(CONNECTED))
        //         bdaddr_type = dev->bdaddr_type;   /* LE for a dual-mode dev */
        // ```
        //
        // and routes to `device_connect_le()`. So: connect, and if we end up
        // holding a classic-only link, connect again to add the LE bearer.
        //
        // Note the state test is `gatt_link_state`, not `is_connected()`.
        // `Connected` is one property per device, true when *either* bearer is
        // up, so a phone merely streaming A2DP used to satisfy it and send us
        // straight into the discovery loop with no ATT channel at all.
        let mut connect_err: Option<ClientError> = None;

        // Round 1 — establish a link if we hold none.
        if gatt_link_state(&device).await == GattLink::Absent {
            info!(%address, "GATT connect");
            connect_err = connect_round(&device).await;
            // The bearer switch below only happens once BlueZ has finished
            // resolving this link and has a profile connected, so let it
            // settle rather than racing it.
            settle_link(&device, Duration::from_secs(3)).await;
        }

        // Round 2 — only when what we got is provably classic-only. Gating on
        // `ClassicOnly` (rather than "not Up") matters: on a healthy but
        // still-resolving LE link a second `Connect()` would take the
        // `le_state.connected && dev->bredr` branch and pull up A2DP/HFP for
        // no reason.
        if gatt_link_state(&device).await == GattLink::ClassicOnly {
            info!(%address, "classic-only link; asking BlueZ to add the LE bearer");
            connect_err = connect_round(&device).await;
            settle_link(&device, Duration::from_secs(3)).await;
        }

        // Only surface a connect error if we came away with no link at all;
        // the second round routinely reports "already connected" once the
        // first one gave us what we needed.
        if let Some(e) = connect_err {
            if gatt_link_state(&device).await == GattLink::Absent {
                return Err(e);
            }
        }
        let connect_ms = t0.elapsed().as_millis();

        // Wait briefly for service discovery to populate. We resolve each
        // service's UUID asynchronously (via D-Bus) before checking the
        // match — see `find_vortex_service` for why we cannot block_on
        // from within an already-async context.
        let t1 = std::time::Instant::now();
        let discovery = timeout(Duration::from_secs(15), async {
            loop {
                // On a freshly established LE link BlueZ has not set
                // ServicesResolved yet, and bluer turns that into an *error*
                // ("GATT services have not been resolved") rather than an
                // empty list. Propagating it aborted the whole connect after
                // a single poll — the one case this loop exists to wait out.
                // Every error here is "not ready yet"; the enclosing timeout
                // is what bounds the wait.
                match device.services().await {
                    Ok(svcs) => match find_vortex_service(svcs).await {
                        Ok(Some(s)) => return Ok::<Service, bluer::Error>(s),
                        Ok(None) => {}
                        Err(e) => debug!(%address, "service UUID read not ready: {e}"),
                    },
                    Err(e) => debug!(%address, "services() not ready: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        // A timeout here is ambiguous on its own: the peer may simply not be
        // running Vortex, or BlueZ may have handed us a classic link on which
        // no GATT service will *ever* appear. Distinguish the two so the UI
        // can say something the user can act on.
        let service: Service = match discovery {
            Ok(res) => res.map_err(ClientError::Bluer)?,
            Err(_) => {
                return Err(if gatt_link_state(&device).await == GattLink::ClassicOnly {
                    ClientError::ClassicBearerOnly
                } else {
                    ClientError::Timeout("service discovery")
                })
            }
        };
        info!(
            %address,
            connect_ms,
            discovery_ms = t1.elapsed().as_millis(),
            "GATT connect+discovery timing"
        );
        let chars = service.characteristics().await?;

        let capability = find_char(&chars, CAPABILITY_UUID, "Capability").await?;
        let pairing_control = find_char(&chars, PAIRING_CONTROL_UUID, "PairingControl").await?;
        let reconnect_control = find_char(&chars, RECONNECT_CONTROL_UUID, "ReconnectControl").await?;
        // Optional — A2/A3 builds before P2.13 didn't expose this.
        let audio_signal = find_char_opt(&chars, AUDIO_SIGNAL_UUID).await;

        Ok(Self {
            address,
            service,
            capability,
            pairing_control,
            reconnect_control,
            audio_signal,
        })
    }

    pub async fn read_capability(&self) -> Result<CapabilityResponse, ClientError> {
        let bytes = self.capability.read().await?;
        debug!(len = bytes.len(), "capability bytes");
        if bytes.len() < 3 {
            return Err(ClientError::BadCapabilityResponse { len: bytes.len() });
        }
        let version = bytes[0];
        if version != 0x01 {
            return Err(ClientError::UnsupportedVersion(version));
        }
        let capability_bits = u16::from_be_bytes([bytes[1], bytes[2]]);
        Ok(CapabilityResponse {
            version,
            capability_bits,
        })
    }

    /// Write a frame to the Pairing Control characteristic.
    ///
    /// Uses write-without-response per spec §9.1: pairing flow is
    /// driven by notify-on-write, the ATT-level ACK adds latency
    /// without any reliability benefit since we already gate on the
    /// notification echo.
    pub async fn write_pairing_control(&self, frame: &Frame) -> Result<(), ClientError> {
        let bytes = frame.encode();
        let req = CharacteristicWriteRequest {
            offset: 0,
            op_type: WriteOp::Command,
            prepare_authorize: false,
            ..Default::default()
        };
        self.pairing_control.write_ext(&bytes, &req).await?;
        Ok(())
    }

    /// Write a frame to the Reconnect Control characteristic.
    ///
    /// Same write-without-response rationale as `write_pairing_control`.
    pub async fn write_reconnect_control(&self, frame: &Frame) -> Result<(), ClientError> {
        let bytes = frame.encode();
        let req = CharacteristicWriteRequest {
            offset: 0,
            op_type: WriteOp::Command,
            prepare_authorize: false,
            ..Default::default()
        };
        self.reconnect_control.write_ext(&bytes, &req).await?;
        Ok(())
    }

    /// Send an echo request and wait for the matching echo response on
    /// Pairing Control notifications. Round-trip is bounded by `wait`.
    pub async fn echo_round_trip(
        &self,
        payload: Vec<u8>,
        wait: Duration,
    ) -> Result<Frame, ClientError> {
        let notifies = self.pairing_control.notify().await?;
        pin_mut!(notifies);
        let request = Frame::echo_request(payload);
        self.write_pairing_control(&request).await?;

        let bytes = timeout(wait, notifies.next())
            .await
            .map_err(|_| ClientError::Timeout("echo notify"))?
            .ok_or(ClientError::Timeout("notify stream closed"))?;
        let response = Frame::decode(&bytes).map_err(ClientError::FrameDecode)?;
        Ok(response)
    }

    pub async fn disconnect(self) -> BluerResult<()> {
        // The Service handle's parent device disconnect can be reached via
        // the adapter; bluer's high-level API does not expose a direct
        // disconnect on the Service. Callers can drop and let bluer GC.
        Ok(())
    }
}

/// What kind of link — if any — the device object currently carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GattLink {
    /// Connected with GATT services present: usable as-is.
    Up,
    /// Connected, services resolved, yet not one GATT service exists. That
    /// is a BR/EDR link (its "ServicesResolved" refers to SDP records, and
    /// classic profiles hang off `sepN`/`avrcp` objects instead).
    ClassicOnly,
    /// Not connected, or an LE link whose services have not resolved yet.
    Absent,
}

async fn gatt_link_state(device: &Device) -> GattLink {
    if !device.is_connected().await.unwrap_or(false) {
        return GattLink::Absent;
    }
    if !device.services().await.map(|s| s.is_empty()).unwrap_or(true) {
        return GattLink::Up;
    }
    // Empty service list: only conclusive once BlueZ says it finished
    // resolving. Mid-LE-connect the list is legitimately empty for a moment.
    if device.is_services_resolved().await.unwrap_or(false) {
        GattLink::ClassicOnly
    } else {
        GattLink::Absent
    }
}

/// One bounded `Device1.Connect()` attempt. Returns the error, if any.
async fn connect_round(device: &Device) -> Option<ClientError> {
    match timeout(Duration::from_secs(15), device.connect()).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(ClientError::Bluer(e)),
        Err(_) => Some(ClientError::Timeout("connect")),
    }
}

/// Wait (bounded) for BlueZ to finish resolving whatever bearer it just
/// brought up. A follow-up `Connect()` only switches to the LE bearer once
/// `bredr_state.svc_resolved` is set and a profile is connected, so racing it
/// would just re-run the BR/EDR path.
async fn settle_link(device: &Device, budget: Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        // Both properties, not either: right after `Connect()` returns, BlueZ
        // may not have propagated `Connected` yet, and bailing on that made
        // this return instantly and defeat its own purpose.
        if device.is_connected().await.unwrap_or(false)
            && device.is_services_resolved().await.unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn find_char(
    chars: &[Characteristic],
    uuid: uuid::Uuid,
    name: &'static str,
) -> Result<Characteristic, ClientError> {
    for c in chars {
        if c.uuid().await? == uuid {
            return Ok(c.clone());
        }
    }
    Err(ClientError::NoCharacteristic(name))
}

/// Same as `find_char` but returns `None` instead of an error when the
/// peer doesn't expose the UUID. Used for characteristics added in
/// later protocol revisions where missing is OK (older peer).
async fn find_char_opt(chars: &[Characteristic], uuid: uuid::Uuid) -> Option<Characteristic> {
    for c in chars {
        if c.uuid().await.ok()? == uuid {
            return Some(c.clone());
        }
    }
    None
}

/// Resolve every service's UUID concurrently and return the first one
/// whose UUID matches `VORTEX_SERVICE_UUID`. Replaces an older sync
/// helper that called `futures::executor::block_on` from inside the
/// tokio runtime — that pattern deadlocks bluer's D-Bus dispatcher
/// because the inner future yields back to the same executor.
async fn find_vortex_service(services: Vec<Service>) -> Result<Option<Service>, bluer::Error> {
    if services.is_empty() {
        return Ok(None);
    }
    let uuids = try_join_all(services.iter().map(|s| s.uuid())).await?;
    Ok(services
        .into_iter()
        .zip(uuids)
        .find(|(_, u)| *u == VORTEX_SERVICE_UUID)
        .map(|(s, _)| s))
}
