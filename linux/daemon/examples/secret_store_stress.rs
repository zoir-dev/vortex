//! Reproduces the 2026-06-11 earbuds-switch freeze shape and proves the
//! fix: a burst of concurrent `SecretServicePeerStore` calls from an
//! 8-worker runtime (inline try_accept like `on_incoming`, inline `list()`
//! like the heartbeat, `spawn_blocking` nonces like the send path), with a
//! liveness heartbeat on the ambient runtime. With the old
//! `block_in_place(Handle::current().block_on(..))` store this shape could
//! park every worker on zbus futures that had no worker left to run on —
//! a total, permanent freeze. With the dedicated secret-store runtime the
//! burst must complete and the heartbeat must keep ticking.
//!
//! Run (needs an unlocked Secret Service / desktop session):
//!   cargo run --example secret_store_stress
//! Exit 0 = no freeze; exit 1 = deadlock (watchdog tripped).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vortex_l3_daemon::core::storage::peers::PeerStore;
use vortex_l3_daemon::core::storage::peers_secret_service::SecretServicePeerStore;

const FAKE_PEER: [u8; 32] = [0xEE; 32];
const BURST_TASKS: u64 = 32;
const WATCHDOG: Duration = Duration::from_secs(120);

fn main() {
    // Same flavor as the real worker runtime (worker.rs: 8 workers).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime");
    let code = rt.block_on(run());
    std::process::exit(code);
}

async fn run() -> i32 {
    let store = Arc::new(SecretServicePeerStore::new().expect("secret service unavailable"));

    // Ambient-runtime liveness heartbeat — the thing that silently died
    // in the live freeze (LAN heartbeat never fired again).
    let beats = Arc::new(AtomicU64::new(0));
    {
        let beats = beats.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                beats.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..BURST_TASKS {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            // Inline sync store call on a worker thread — the exact shape
            // of the pre-fix `on_incoming` replay check.
            let _ = s.try_accept_audio_in_nonce(&FAKE_PEER, 1_000_000 + i);
            // Inline list() — the heartbeat shape.
            let _ = s.list();
            // spawn_blocking nonce — the send-path (O4) shape.
            let s2 = s.clone();
            let _ = tokio::task::spawn_blocking(move || s2.next_audio_out_nonce(&FAKE_PEER)).await;
        }));
    }

    let all = async {
        for h in handles {
            let _ = h.await;
        }
    };
    let code = match tokio::time::timeout(WATCHDOG, all).await {
        Ok(()) => {
            let elapsed = start.elapsed();
            let expected = (elapsed.as_millis() / 100) as u64;
            let got = beats.load(Ordering::Relaxed);
            println!(
                "OK: {BURST_TASKS} burst tasks completed in {:.1}s; ambient heartbeat {got}/{expected} beats",
                elapsed.as_secs_f32()
            );
            // A wedged-but-recovered runtime shows up as a big beat deficit.
            if expected > 10 && got * 2 < expected {
                println!("WARN: ambient runtime starved (heartbeat lost >50% of beats)");
                2
            } else {
                0
            }
        }
        Err(_) => {
            println!(
                "FROZEN: burst did not complete within {WATCHDOG:?} — deadlock (heartbeat {} beats)",
                beats.load(Ordering::Relaxed)
            );
            1
        }
    };

    // Drop the fake peer's keyring entries (nonce slots etc.).
    let _ = store.forget(&FAKE_PEER);
    println!("keyring cleanup done");
    code
}
