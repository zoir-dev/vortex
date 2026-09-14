//! vortex-l3d — Linux daemon CLI.

use std::env;
use std::time::Duration;

use bluer::Session;
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use vortex_l3_daemon::core::ble::{
    client::VortexClient,
    frame::Frame,
    scanner::{run_filtered_scan, VortexCandidate},
    AdvFlags,
};
use vortex_l3_daemon::core::identity::{IdentityPublicView, Platform};
use vortex_l3_daemon::core::lan::discovery::discover_first;
use vortex_l3_daemon::core::lan::tcp_client::run_lan_reconnect;
use vortex_l3_daemon::core::pairing::handshake::{
    run_pairing_initiator, run_xx_initiator, LocalDecision,
};
use vortex_l3_daemon::core::pairing::backoff::{BackoffState, NextAction};
use vortex_l3_daemon::core::pairing::reconnect::run_ik_initiator;
use vortex_l3_daemon::core::platform::linux::LinuxGattLink;
use vortex_l3_daemon::core::storage::{
    load_or_generate,
    peers::{PeerStore, TrustedPeer},
    peers_secret_service::SecretServicePeerStore,
    secret_service::SecretServiceIdentityStore,
    IdentityStore, InMemoryIdentityStore,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "scan" => cmd_scan().await,
        "identity" => cmd_identity().await,
        "connect" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d connect <BD_ADDR>")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            cmd_connect(addr).await
        }
        "echo" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d echo <BD_ADDR> [hex_payload]")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            let payload = args
                .get(3)
                .map(|s| hex::decode(s).map_err(|e| format!("bad hex payload: {e}")))
                .transpose()?
                .unwrap_or_else(|| b"vortex".to_vec());
            cmd_echo(addr, payload).await
        }
        "handshake" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d handshake <BD_ADDR>")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            cmd_handshake(addr).await
        }
        "pair" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d pair <BD_ADDR> [--auto-approve]")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            let auto_approve = args.iter().any(|a| a == "--auto-approve");
            cmd_pair(addr, auto_approve).await
        }
        "reconnect" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d reconnect <BD_ADDR> [<peer_pub_hex>]")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            let peer_pub_hex = args.get(3).cloned();
            cmd_reconnect(addr, peer_pub_hex).await
        }
        "auto-reconnect" => {
            let addr = args
                .get(2)
                .ok_or("usage: vortex-l3d auto-reconnect <BD_ADDR> [--fast] [--max-attempts N]")?
                .parse()
                .map_err(|e| format!("bad address: {e}"))?;
            let fast = args.iter().any(|a| a == "--fast");
            let max_attempts = args
                .iter()
                .position(|a| a == "--max-attempts")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<usize>().ok());
            cmd_auto_reconnect(addr, fast, max_attempts).await
        }
        "lan-reconnect" => cmd_lan_reconnect().await,
        "peers" => cmd_peers().await,
        "forget" => {
            let pub_hex = args.get(2).ok_or("usage: vortex-l3d forget <peer_static_pub_hex>")?;
            let pub_bytes = hex::decode(pub_hex).map_err(|e| format!("bad hex: {e}"))?;
            if pub_bytes.len() != 32 {
                return Err("peer_static_pub must be 32 bytes (64 hex chars)".into());
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&pub_bytes);
            cmd_forget(arr).await
        }
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("vortex-l3d v0.1.0 — Vortex Linux daemon");
    println!();
    println!("USAGE:");
    println!("  vortex-l3d <command>");
    println!();
    println!("COMMANDS:");
    println!("  scan                 Scan for Vortex BLE candidates (Ctrl+C to stop).");
    println!("  identity             Generate or load this device's V1 identity and");
    println!("                       print the public view as JSON.");
    println!("  connect <BD_ADDR>    Connect to a peer's Vortex GATT service and read");
    println!("                       its Capability characteristic (Phase 5a).");
    println!("  echo <BD_ADDR> [hex] Round-trip a frame on Pairing Control via the");
    println!("                       Phase 5b transport echo (default payload \"vortex\").");
    println!("  handshake <BD_ADDR>  Run Noise XX over Pairing Control (Phase 5c).");
    println!("                       Prints transcript hash + peer static pub.");
    println!("  pair <BD_ADDR> [--auto-approve]");
    println!("                       Run XX, show SAS, exchange dual approval (Phase 5d).");
    println!("                       Persists the trusted peer + PRS in the keyring.");
    println!("  reconnect <BD_ADDR> [<peer_pub_hex>]");
    println!("                       Trusted reconnect via Noise IK + ping/pong (Phase 6).");
    println!("                       Defaults to the first trusted peer.");
    println!("  auto-reconnect <BD_ADDR> [--fast] [--max-attempts N]");
    println!("                       Loop reconnect with backoff [0,5s,15s,1m,5m,passive]");
    println!("                       (Phase 7). --fast uses ms delays for testing.");
    println!("  lan-reconnect        Discover via mDNS, run Noise IK over TCP (Phase 8).");
    println!("                       Defaults to the first trusted peer.");
    println!("  peers                List trusted peers from secure storage.");
    println!("  forget <peer_pub_hex>");
    println!("                       Forget a trusted peer (delete record + PRS).");
    println!("  help                 Show this help.");
}

async fn cmd_connect(addr: bluer::Address) -> Result<(), Box<dyn std::error::Error>> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    info!(adapter = %adapter.name(), %addr, "connecting");

    let client = VortexClient::connect(&adapter, addr).await?;
    info!(%addr, "Vortex Service discovered");

    let capability = client.read_capability().await?;
    println!(
        "capability: version=0x{:02x} capability_bits=0x{:04x}",
        capability.version, capability.capability_bits,
    );
    Ok(())
}

async fn cmd_echo(
    addr: bluer::Address,
    payload: Vec<u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    info!(adapter = %adapter.name(), %addr, "connecting for echo");

    let client = VortexClient::connect(&adapter, addr).await?;
    let pretty_in = hex::encode(&payload);

    let response: Frame = client
        .echo_round_trip(payload, std::time::Duration::from_secs(5))
        .await?;
    let pretty_out = hex::encode(&response.payload);

    println!("echo request   : type=0x30 sub=0x01 payload={pretty_in}");
    println!(
        "echo response  : type=0x{:02x} sub=0x{:02x} payload={pretty_out}",
        response.ty, response.sub,
    );
    if response.ty == 0x30 && response.sub == 0x02 && pretty_out == pretty_in {
        println!("✅ round-trip ok — bytes identical");
        Ok(())
    } else {
        Err("echo response did not match request".into())
    }
}

/// Open the identity store: Secret Service in production, and — only when
/// `VORTEX_INSECURE=1` is set explicitly — a dev-only in-memory fallback
/// (per spec §3.2; the identity then dies with the process, so trusted
/// reconnects will fail after a restart). Returns the store plus a label
/// for display.
fn open_identity_store(
) -> Result<(Box<dyn IdentityStore>, &'static str), Box<dyn std::error::Error>> {
    match SecretServiceIdentityStore::new() {
        Ok(s) => Ok((Box::new(s), "secret-service")),
        Err(err) if std::env::var("VORTEX_INSECURE").as_deref() == Ok("1") => {
            warn!(%err, "secret service unavailable — INSECURE in-memory identity (dev only)");
            eprintln!("warning: secret service unavailable ({err}); insecure in-memory identity");
            Ok((Box::new(InMemoryIdentityStore::new()), "in-memory (INSECURE)"))
        }
        Err(err) => Err(format!(
            "secret service unavailable: {err}\n       \
             set VORTEX_INSECURE=1 to fall back to in-memory storage (dev only)."
        )
        .into()),
    }
}

async fn cmd_pair(
    addr: bluer::Address,
    auto_approve: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (store, _) = open_identity_store()?;
    let identity = load_or_generate(&*store, Platform::Linux)?;
    info!(
        device_id = %hex::encode(identity.device_id),
        "loaded local identity"
    );

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    let client = VortexClient::connect(&adapter, addr).await?;
    info!(%addr, "connected; running pairing");
    let link = LinuxGattLink::from_client(adapter.clone(), &client);

    let local_name = read_local_hostname();
    let outcome = run_pairing_initiator(
        &link,
        &identity.static_priv.0,
        std::time::Duration::from_secs(60),
        |sas: &str| {
            // The decide callback is async to support UI-driven approval
            // flows, but the CLI prompt is blocking stdin — wrap it in
            // an immediate Future. We move the SAS in by value since the
            // closure's &str borrow doesn't outlive the Future.
            let sas_owned = sas.to_string();
            async move {
                println!();
                println!("=========================================================");
                println!("  Vortex pairing — verify the SAS shown on the peer");
                println!("=========================================================");
                println!("  SAS: {sas_owned}");
                println!("=========================================================");
                if auto_approve {
                    println!("  --auto-approve: approving without prompt");
                    return LocalDecision::Approve;
                }
                print!("  Press Enter to approve, or type 'reject' + Enter: ");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(std::io::stdin()), &mut line).ok();
                if line.trim().eq_ignore_ascii_case("reject") {
                    LocalDecision::Reject
                } else {
                    LocalDecision::Approve
                }
            }
        },
        local_name.as_deref(),
    )
    .await?;

    let paired_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let trusted = TrustedPeer {
        peer_static_pub: outcome.xx.peer_static_pub,
        prs: outcome.prs,
        paired_at,
        peer_name: outcome.peer_name.clone(),
    };
    let peer_store = SecretServicePeerStore::new()?;
    peer_store.save(&trusted)?;

    println!();
    println!("✅ Paired");
    println!("  peer_static_pub : {}", hex::encode(&outcome.xx.peer_static_pub));
    println!("  transcript_hash : {}", hex::encode(&outcome.xx.transcript_hash));
    // PRS MUST NOT appear in production output (spec §3.5 T-LOC-3). Only
    // print it when an explicit insecure-debug flag is set; the default
    // path leaves the secret in the trust store and never on stdout.
    if std::env::var("VORTEX_INSECURE_DEBUG").as_deref() == Ok("1") {
        // LOG_REDACTION_ALLOW: gated behind VORTEX_INSECURE_DEBUG=1 (dev only)
        eprintln!("  prs             : {} [insecure debug]", hex::encode(&outcome.prs));
    } else {
        println!("  prs             : [redacted; set VORTEX_INSECURE_DEBUG=1 to reveal]");
    }
    println!("  trust persisted via Secret Service (paired_at = {paired_at})");
    Ok(())
}

async fn cmd_reconnect(
    addr: bluer::Address,
    peer_pub_hex: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Load identity.
    let (id_store, _) = open_identity_store()?;
    let identity = load_or_generate(&*id_store, Platform::Linux)?;

    // Resolve trusted peer.
    let peer_store = SecretServicePeerStore::new()?;
    let peer = match peer_pub_hex {
        Some(hex_str) => {
            let bytes = hex::decode(hex_str).map_err(|e| format!("bad hex: {e}"))?;
            if bytes.len() != 32 {
                return Err("peer_static_pub must be 32 bytes".into());
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            peer_store.load(&arr)?
        }
        None => {
            let peers = peer_store.list()?;
            peers
                .into_iter()
                .next()
                .ok_or("no trusted peers — pair first with `vortex-l3d pair`")?
        }
    };
    info!(
        peer_static_pub = %hex::encode(peer.peer_static_pub),
        "using trusted peer for reconnect"
    );

    // Connect.
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // BlueZ hygiene: if a previous session left the device "connected"
    // from BlueZ's perspective, disconnect first so connect() doesn't
    // return InProgress. Do NOT remove_device pre-connect — that purges
    // the address from cache and connect() then fails with NotFound.
    if let Ok(device) = adapter.device(addr) {
        if device.is_connected().await.unwrap_or(false) {
            let _ = device.disconnect().await;
        }
    }

    let client = VortexClient::connect(&adapter, addr).await?;
    let link = LinuxGattLink::from_client(adapter.clone(), &client);

    let local_counter = peer_store
        .load_counter(&peer.peer_static_pub)
        .unwrap_or(0);
    let outcome = run_ik_initiator(
        &link,
        &identity.static_priv.0,
        &peer.peer_static_pub,
        &peer.prs,
        local_counter,
        std::time::Duration::from_secs(15),
    )
    .await?;
    if outcome.peer_counter < local_counter {
        tracing::warn!(
            "possible trust rollback: peer counter={} local={}",
            outcome.peer_counter,
            local_counter
        );
    }
    let _ = peer_store.bump_counter(&peer.peer_static_pub, outcome.peer_counter);

    println!();
    println!("✅ Trusted reconnect ok");
    println!("  peer_static_pub : {}", hex::encode(&outcome.peer_static_pub));
    println!("  transcript_hash : {}", hex::encode(&outcome.transcript_hash));
    println!("  liveness        : {}", if outcome.liveness_ok { "ping/pong ok" } else { "FAIL" });
    Ok(())
}

async fn cmd_lan_reconnect() -> Result<(), Box<dyn std::error::Error>> {
    let (id_store, _) = open_identity_store()?;
    let identity = load_or_generate(&*id_store, Platform::Linux)?;
    let peer_store = SecretServicePeerStore::new()?;
    let peer = peer_store
        .list()?
        .into_iter()
        .next()
        .ok_or("no trusted peers — pair first")?;

    info!("browsing _vortex._tcp.local. for up to 8s");
    let candidate = discover_first(std::time::Duration::from_secs(8))
        .await
        .map_err(|e| format!("mdns: {e}"))?
        .ok_or("no LAN candidate observed within timeout")?;
    info!(
        host = %candidate.host,
        port = candidate.port,
        addrs = ?candidate.addresses,
        "LAN candidate"
    );

    let ip = candidate
        .addresses
        .iter()
        .find(|a| matches!(a, std::net::IpAddr::V4(_)))
        .copied()
        .or_else(|| candidate.addresses.first().copied())
        .ok_or("no usable IP address")?;
    let socket_addr = std::net::SocketAddr::new(ip, candidate.port);

    let local_counter = peer_store
        .load_counter(&peer.peer_static_pub)
        .unwrap_or(0);
    // CLI override hooks for cross-device locale-sync testing:
    //   VORTEX_LOCALE=uz VORTEX_LOCALE_TS=1700000000 vortex-l3d lan-reconnect
    // Lets you simulate "user picked uz on the laptop just now" without
    // running the Tauri UI.
    let mut local_state = vortex_l3_daemon::core::appstate::AppState::now_laptop();
    if let Ok(loc) = std::env::var("VORTEX_LOCALE") {
        local_state.locale = Some(loc);
        local_state.locale_changed_at = std::env::var("VORTEX_LOCALE_TS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    // Earbuds passthrough — same logic as the Tauri worker.
    if let Ok(session) = bluer::Session::new().await {
        if let Ok(adapter) = session.default_adapter().await {
            local_state.earbuds =
                vortex_l3_daemon::core::earbuds::scan_local_earbuds(&adapter).await;
        }
    }
    let outcome = run_lan_reconnect(
        socket_addr,
        &identity.static_priv.0,
        &peer.peer_static_pub,
        &peer.prs,
        local_counter,
        local_state,
        std::time::Duration::from_secs(15),
        None, // CLI: no mirror caches to bulk-sync
    )
    .await?;
    if outcome.peer_counter < local_counter {
        tracing::warn!(
            "possible trust rollback (LAN): peer counter={} local={}",
            outcome.peer_counter,
            local_counter
        );
    }
    let _ = peer_store.bump_counter(&peer.peer_static_pub, outcome.peer_counter);

    println!();
    println!("✅ LAN reconnect ok — peer {} via {}", hex::encode(&outcome.peer_static_pub[..8]), outcome.remote);
    println!("  peer_static_pub : {}", hex::encode(&outcome.peer_static_pub));
    println!("  transcript_hash : {}", hex::encode(&outcome.transcript_hash));
    println!("  liveness        : {}", if outcome.liveness_ok { "ping/pong ok" } else { "FAIL" });
    if let Some(s) = &outcome.peer_state {
        println!(
            "  peer state      : battery={:?} class={:?} name={:?}\n                    locale={:?} (changed_at={}) theme={:?} (changed_at={})",
            s.battery, s.class, s.name, s.locale, s.locale_changed_at, s.theme, s.theme_changed_at,
        );
    }
    Ok(())
}

async fn cmd_auto_reconnect(
    addr: bluer::Address,
    fast: bool,
    max_attempts: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve identity + first trusted peer once.
    let (id_store, _) = open_identity_store()?;
    let identity = load_or_generate(&*id_store, Platform::Linux)?;
    let peer_store = SecretServicePeerStore::new()?;
    let peer = peer_store
        .list()?
        .into_iter()
        .next()
        .ok_or("no trusted peers — pair first with `vortex-l3d pair`")?;

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let mut backoff = if fast { BackoffState::fast() } else { BackoffState::standard() };
    let mut attempts_done: usize = 0;

    loop {
        if let Some(cap) = max_attempts {
            if attempts_done >= cap {
                println!("--max-attempts reached; exiting");
                return Ok(());
            }
        }

        match backoff.next_action() {
            NextAction::AttemptAfter(delay) => {
                if delay > std::time::Duration::ZERO {
                    info!(
                        attempt = backoff.attempt_number(),
                        delay_ms = delay.as_millis() as u64,
                        "sleeping before next attempt"
                    );
                    tokio::time::sleep(delay).await;
                }
                info!(attempt = backoff.attempt_number(), "attempting reconnect");
                attempts_done += 1;

                let connect = VortexClient::connect(&adapter, addr).await;
                let client = match connect {
                    Ok(c) => c,
                    Err(err) => {
                        eprintln!("attempt {} connect failed: {err}", backoff.attempt_number());
                        backoff.record_failure();
                        continue;
                    }
                };

                let local_counter = peer_store
                    .load_counter(&peer.peer_static_pub)
                    .unwrap_or(0);
                let link = LinuxGattLink::from_client(adapter.clone(), &client);
                match run_ik_initiator(
                    &link,
                    &identity.static_priv.0,
                    &peer.peer_static_pub,
                    &peer.prs,
                    local_counter,
                    std::time::Duration::from_secs(15),
                )
                .await
                {
                    Ok(outcome) => {
                        if outcome.peer_counter < local_counter {
                            tracing::warn!(
                                "possible trust rollback (auto-reconnect): peer={} local={}",
                                outcome.peer_counter,
                                local_counter
                            );
                        }
                        let _ = peer_store
                            .bump_counter(&peer.peer_static_pub, outcome.peer_counter);
                        println!(
                            "✅ attempt {} succeeded — transcript_hash={}",
                            backoff.attempt_number(),
                            hex::encode(&outcome.transcript_hash),
                        );
                        backoff.reset();
                        // For Phase 7 demonstration we exit on first success.
                        return Ok(());
                    }
                    Err(err) => {
                        eprintln!(
                            "attempt {} reconnect failed: {err}",
                            backoff.attempt_number()
                        );
                        backoff.record_failure();
                    }
                }
            }
            NextAction::Passive => {
                println!(
                    "backoff schedule reached passive; abandoning auto-reconnect after {} attempts",
                    attempts_done
                );
                return Ok(());
            }
        }
    }
}

async fn cmd_peers() -> Result<(), Box<dyn std::error::Error>> {
    let store = SecretServicePeerStore::new()?;
    let peers = store.list()?;
    if peers.is_empty() {
        println!("(no trusted peers)");
        return Ok(());
    }
    println!("trusted peers ({}):", peers.len());
    for (i, peer) in peers.iter().enumerate() {
        println!(
            "  [{i}] static_pub = {}  paired_at = {}",
            hex::encode(peer.peer_static_pub),
            peer.paired_at,
        );
    }
    Ok(())
}

async fn cmd_forget(peer_static_pub: [u8; 32]) -> Result<(), Box<dyn std::error::Error>> {
    let store = SecretServicePeerStore::new()?;
    store.forget(&peer_static_pub)?;
    println!("forgot peer {}", hex::encode(peer_static_pub));
    Ok(())
}

async fn cmd_handshake(addr: bluer::Address) -> Result<(), Box<dyn std::error::Error>> {
    // Load (or generate) our persistent V1 identity.
    let (store, _) = open_identity_store()?;
    let identity = load_or_generate(&*store, Platform::Linux)?;
    info!(
        device_id = %hex::encode(identity.device_id),
        static_pub = %hex::encode(identity.static_pub.0),
        "loaded local identity"
    );

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    info!(adapter = %adapter.name(), %addr, "connecting for XX handshake");

    let client = VortexClient::connect(&adapter, addr).await?;
    let link = LinuxGattLink::from_client(adapter.clone(), &client);
    let outcome = run_xx_initiator(
        &link,
        &identity.static_priv.0,
        std::time::Duration::from_secs(15),
    )
    .await?;

    println!("✅ Noise XX completed");
    println!("  transcript_hash : {}", hex::encode(&outcome.transcript_hash));
    println!("  peer_static_pub : {}", hex::encode(&outcome.peer_static_pub));
    Ok(())
}

async fn cmd_identity() -> Result<(), Box<dyn std::error::Error>> {
    // Try Secret Service first (production path); fall back to in-memory only
    // when explicitly running with VORTEX_INSECURE=1 (per spec §3.2).
    let (store, label) = match open_identity_store() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(3);
        }
    };
    let record = load_or_generate(&*store, Platform::Linux)?;
    let view: IdentityPublicView = (&record).into();
    let json = serde_json::to_string_pretty(&view)?;
    println!("{json}");
    eprintln!("(backend: {label})");
    Ok(())
}

async fn cmd_scan() -> Result<(), Box<dyn std::error::Error>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    info!(
        adapter = %adapter.name(),
        addr = %adapter.address().await?,
        "adapter ready"
    );

    let scan_task = tokio::spawn({
        let adapter = adapter.clone();
        async move {
            if let Err(err) = run_filtered_scan(adapter, on_candidate).await {
                error!(?err, "scan terminated with error");
            }
        }
    });

    // Run until Ctrl+C or 5 minutes elapse — whichever comes first.
    tokio::select! {
        _ = signal::ctrl_c() => info!("ctrl+c received; stopping scan"),
        _ = tokio::time::sleep(Duration::from_secs(5 * 60)) => info!("scan window timeout reached"),
    }

    scan_task.abort();
    Ok(())
}

fn on_candidate(c: VortexCandidate) {
    let rssi = c.rssi.map(|v| format!("{v} dBm")).unwrap_or_else(|| "?".into());
    let name = c.local_name.unwrap_or_else(|| "(no name)".into());
    let mode = if c.payload.flags.is_pairable() {
        "pairable"
    } else if c.payload.flags.is_trusted_presence() {
        "trusted-presence"
    } else {
        "unknown"
    };
    let payload8_hex = c
        .payload
        .payload_8
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    println!(
        "[{ts}] vortex {addr}  rssi={rssi}  mode={mode:<16}  payload8={payload8}  name={name}",
        ts = humantime::format_rfc3339_seconds(c.observed_at),
        addr = c.address,
        payload8 = payload8_hex,
    );
}

// Keep AdvFlags type available for any future scan filter logic in main.rs.
const _: AdvFlags = AdvFlags(0);

/// Read the local hostname from `/proc/sys/kernel/hostname` so we can
/// expose a friendly device name during pairing instead of "Linux laptop".
fn read_local_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
