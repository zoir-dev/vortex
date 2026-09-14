//! Secret Service-backed [`PeerStore`] — the Linux trusted-peer backend.
//!
//! Split out of `peers.rs` so the trait, the `TrustedPeer` record and its wire
//! codec are not hostage to a `use secret_service::...` at the top of that file.
//! A second platform brings its own backend (Windows Credential Manager) and
//! reuses all three unchanged.

use std::collections::HashMap;
use std::sync::Mutex;

use secret_service::{EncryptionType, SecretService};

use super::peers::{PeerStore, TrustedPeer};
use super::{unlocked_default_collection, StorageError, StorageResult};

const SCHEMA: &str = "com.vortex.peer.v1";
const LABEL: &str = "Vortex V1 trusted peer";
const CONTENT_TYPE: &str = "application/x-vortex-peer-v1";

/// Schema for the per-peer monotonic reconnect counter (M6).
const COUNTER_SCHEMA: &str = "com.vortex.peer.counter.v1";
const COUNTER_LABEL: &str = "Vortex V1 reconnect counter";
const COUNTER_CONTENT_TYPE: &str = "application/x-vortex-peer-counter-v1";

/// Schema for the per-peer audio-op nonce (sender / outbound).
/// Persisted so a process restart cannot reset the counter — a
/// rollback would let a network attacker replay a captured
/// `AudioOpFrame` (e.g. an old `Done`) and confuse our state.
const AUDIO_OUT_NONCE_SCHEMA: &str = "com.vortex.peer.audio_out_nonce.v1";
const AUDIO_OUT_NONCE_LABEL: &str = "Vortex V1 audio-op outbound nonce";
const AUDIO_OUT_NONCE_CT: &str = "application/x-vortex-audio-out-nonce-v1";

/// Schema for the per-peer audio-op nonce (receiver / inbound).
/// Highest accepted nonce; we drop any frame with `<= in_nonce`.
const AUDIO_IN_NONCE_SCHEMA: &str = "com.vortex.peer.audio_in_nonce.v1";
const AUDIO_IN_NONCE_LABEL: &str = "Vortex V1 audio-op inbound nonce";
const AUDIO_IN_NONCE_CT: &str = "application/x-vortex-audio-in-nonce-v1";

/// Schema for the per-peer BT-bonded identity address (BD_ADDR string,
/// e.g. "74:15:75:27:AB:2E"). Stored after a successful BT bond during
/// pairing so the persistent reconnect loop can connect DIRECTLY to the
/// stable identity (no BLE scan — scanning conflicts with A2DP on the
/// shared radio). Absent on records that haven't bonded yet.
const BONDED_ADDR_SCHEMA: &str = "com.vortex.peer.bonded_addr.v1";
const BONDED_ADDR_LABEL: &str = "Vortex V1 BT-bonded identity address";
const BONDED_ADDR_CT: &str = "application/x-vortex-bonded-addr-v1";

/// Secret Service-backed trusted-peer store. The `lock` serialises the
/// read-modify-write sequences (counters, nonces) that would otherwise race
/// between the heartbeat and the BLE loop on the same collection path.
pub struct SecretServicePeerStore {
    lock: Mutex<()>,
}

impl SecretServicePeerStore {
    pub fn new() -> StorageResult<Self> {
        Self::block_on(async {
            let s = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            s.get_default_collection()
                .await
                .map_err(|e| StorageError::Backend(format!("collection: {e}")))?;
            Ok::<_, StorageError>(())
        })?;
        Ok(Self { lock: Mutex::new(()) })
    }

    fn block_on<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T> + Send,
        T: Send,
    {
        super::secret_block_on(fut)
    }

    fn attrs_for(peer_pub_hex: &str) -> HashMap<String, String> {
        let mut a = HashMap::new();
        a.insert("schema".to_string(), SCHEMA.to_string());
        a.insert("peer_static_pub".to_string(), peer_pub_hex.to_string());
        a
    }

    fn schema_only_attrs() -> HashMap<&'static str, &'static str> {
        let mut a = HashMap::new();
        a.insert("schema", SCHEMA);
        a
    }

    fn counter_attrs_for(peer_pub_hex: &str) -> HashMap<String, String> {
        let mut a = HashMap::new();
        a.insert("schema".to_string(), COUNTER_SCHEMA.to_string());
        a.insert("peer_static_pub".to_string(), peer_pub_hex.to_string());
        a
    }

    fn audio_out_attrs_for(peer_pub_hex: &str) -> HashMap<String, String> {
        let mut a = HashMap::new();
        a.insert("schema".to_string(), AUDIO_OUT_NONCE_SCHEMA.to_string());
        a.insert("peer_static_pub".to_string(), peer_pub_hex.to_string());
        a
    }

    fn audio_in_attrs_for(peer_pub_hex: &str) -> HashMap<String, String> {
        let mut a = HashMap::new();
        a.insert("schema".to_string(), AUDIO_IN_NONCE_SCHEMA.to_string());
        a.insert("peer_static_pub".to_string(), peer_pub_hex.to_string());
        a
    }

    fn bonded_addr_attrs_for(peer_pub_hex: &str) -> HashMap<String, String> {
        let mut a = HashMap::new();
        a.insert("schema".to_string(), BONDED_ADDR_SCHEMA.to_string());
        a.insert("peer_static_pub".to_string(), peer_pub_hex.to_string());
        a
    }

    /// Read-modify-write helper for a single 8-byte u64 secret slot.
    /// Used by both the counter and the audio-nonce paths. `op`
    /// receives the current value (or 0 if absent) and returns the
    /// new value to store.
    fn rmw_u64_slot(
        &self,
        attrs_owned: HashMap<String, String>,
        label: &'static str,
        content_type: &'static str,
        op: impl FnOnce(u64) -> u64 + Send,
    ) -> StorageResult<u64> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let coll = unlocked_default_collection(&svc).await?;
            let mut search = svc
                .search_items(attrs_ref.clone())
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let current: u64 = if let Some(item) =
                search.unlocked.pop().or_else(|| search.locked.pop())
            {
                if item.is_locked().await.unwrap_or(false) {
                    item.unlock()
                        .await
                        .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
                }
                let bytes = item
                    .get_secret()
                    .await
                    .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
                if bytes.len() == 8 {
                    u64::from_be_bytes(bytes[..8].try_into().unwrap())
                } else {
                    0
                }
            } else {
                0
            };
            let next = op(current);
            // Remove old item(s) so create_item produces exactly one
            // record (Secret Service has no atomic upsert).
            let mut existing = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            for item in existing.unlocked.drain(..).chain(existing.locked.drain(..)) {
                let _ = item.delete().await;
            }
            coll.create_item(
                label,
                attrs_owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
                &next.to_be_bytes(),
                /*replace=*/ false,
                content_type,
            )
            .await
            .map_err(|e| StorageError::Backend(format!("create_item: {e}")))?;
            Ok::<u64, StorageError>(next)
        })
    }
}

impl PeerStore for SecretServicePeerStore {
    fn save(&self, peer: &TrustedPeer) -> StorageResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let payload = peer.encode().to_vec();
        let pub_hex = hex::encode(peer.peer_static_pub);
        let attrs_owned = Self::attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let coll = unlocked_default_collection(&svc).await?;
            // Dedupe: explicitly remove any prior items for this
            // peer_static_pub before creating. The `replace=true` flag
            // on create_item alone is unreliable across Secret Service
            // implementations (gnome-keyring vs kwallet handle the
            // "match by attributes" semantics differently); explicit
            // search + delete is portable.
            let prior = svc
                .search_items(attrs_ref.clone())
                .await
                .map_err(|e| StorageError::Backend(format!("dedupe search: {e}")))?;
            for item in prior.unlocked.iter().chain(prior.locked.iter()) {
                item.delete()
                    .await
                    .map_err(|e| StorageError::Backend(format!("dedupe delete: {e}")))?;
            }
            coll.create_item(LABEL, attrs_ref, &payload, true, CONTENT_TYPE)
                .await
                .map_err(|e| StorageError::Backend(format!("create_item: {e}")))?;
            Ok(())
        })
    }

    fn load(&self, peer_static_pub: &[u8; 32]) -> StorageResult<TrustedPeer> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let mut search = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let item = search.unlocked.pop().or_else(|| search.locked.pop());
            let item = match item {
                Some(i) => i,
                None => return Err(StorageError::NotFound),
            };
            if item.is_locked().await.unwrap_or(false) {
                item.unlock()
                    .await
                    .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
            }
            let bytes = item
                .get_secret()
                .await
                .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
            TrustedPeer::decode(&bytes)
        })
    }

    fn list(&self) -> StorageResult<Vec<TrustedPeer>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let search = svc
                .search_items(Self::schema_only_attrs())
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let mut peers = Vec::new();
            for item in search.unlocked.iter().chain(search.locked.iter()) {
                if item.is_locked().await.unwrap_or(false) {
                    item.unlock()
                        .await
                        .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
                }
                let bytes = item
                    .get_secret()
                    .await
                    .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
                if let Ok(peer) = TrustedPeer::decode(&bytes) {
                    peers.push(peer);
                }
            }
            Ok(peers)
        })
    }

    fn forget(&self, peer_static_pub: &[u8; 32]) -> StorageResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let search = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let trust_count = search.unlocked.len() + search.locked.len();
            for item in search.unlocked.iter().chain(search.locked.iter()) {
                item.delete()
                    .await
                    .map_err(|e| StorageError::Backend(format!("delete: {e}")))?;
            }
            // Cascade: drop every other schema we hold for this peer
            // (counter + audio_out_nonce + audio_in_nonce). A partial
            // forget would leave stale nonces that survive into a
            // future re-pair and cause replay-protection mismatches.
            let cascades = [
                ("counter", Self::counter_attrs_for(&pub_hex)),
                ("audio_out_nonce", Self::audio_out_attrs_for(&pub_hex)),
                ("audio_in_nonce", Self::audio_in_attrs_for(&pub_hex)),
                ("bonded_addr", Self::bonded_addr_attrs_for(&pub_hex)),
            ];
            let mut cascade_counts: Vec<(&str, usize)> = Vec::new();
            for (label, attrs_owned) in &cascades {
                let attrs_ref: HashMap<&str, &str> = attrs_owned
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                if let Ok(cs) = svc.search_items(attrs_ref).await {
                    let n = cs.unlocked.len() + cs.locked.len();
                    cascade_counts.push((*label, n));
                    for item in cs.unlocked.iter().chain(cs.locked.iter()) {
                        let _ = item.delete().await;
                    }
                }
            }
            tracing::info!(
                trust = trust_count,
                cascades = ?cascade_counts,
                "peer_store.forget cleared {} entries",
                &pub_hex[..16],
            );
            Ok(())
        })
    }

    fn load_counter(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::counter_attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let mut search = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let item = match search.unlocked.pop().or_else(|| search.locked.pop()) {
                Some(i) => i,
                None => return Ok(0u64),
            };
            if item.is_locked().await.unwrap_or(false) {
                item.unlock()
                    .await
                    .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
            }
            let bytes = item
                .get_secret()
                .await
                .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
            if bytes.len() != 8 {
                return Ok(0u64);
            }
            Ok(u64::from_be_bytes(bytes[..8].try_into().unwrap()))
        })
    }

    fn load_bonded_addr(&self, peer_static_pub: &[u8; 32]) -> StorageResult<Option<String>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::bonded_addr_attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let mut search = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let Some(item) = search.unlocked.pop().or_else(|| search.locked.pop()) else {
                return Ok(None);
            };
            if item.is_locked().await.unwrap_or(false) {
                item.unlock()
                    .await
                    .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
            }
            let bytes = item
                .get_secret()
                .await
                .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
            Ok(String::from_utf8(bytes).ok().filter(|s| !s.is_empty()))
        })
    }

    fn save_bonded_addr(&self, peer_static_pub: &[u8; 32], addr: &str) -> StorageResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::bonded_addr_attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let addr_owned = addr.to_string();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let coll = unlocked_default_collection(&svc).await?;
            // Single-record upsert: drop any existing then create one.
            let mut existing = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            for item in existing.unlocked.drain(..).chain(existing.locked.drain(..)) {
                let _ = item.delete().await;
            }
            coll.create_item(
                BONDED_ADDR_LABEL,
                attrs_owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
                addr_owned.as_bytes(),
                /*replace=*/ false,
                BONDED_ADDR_CT,
            )
            .await
            .map_err(|e| StorageError::Backend(format!("create_item: {e}")))?;
            Ok(())
        })
    }

    fn bump_counter(&self, peer_static_pub: &[u8; 32], peer_seen: u64) -> StorageResult<u64> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::counter_attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let coll = unlocked_default_collection(&svc).await?;
            // Read current.
            let mut search = svc
                .search_items(attrs_ref.clone())
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let current: u64 = if let Some(item) =
                search.unlocked.pop().or_else(|| search.locked.pop())
            {
                if item.is_locked().await.unwrap_or(false) {
                    item.unlock()
                        .await
                        .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
                }
                let bytes = item
                    .get_secret()
                    .await
                    .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
                let _ = item.delete().await;
                if bytes.len() == 8 {
                    u64::from_be_bytes(bytes[..8].try_into().unwrap())
                } else {
                    0
                }
            } else {
                0
            };
            let next = current.max(peer_seen).saturating_add(1);
            let payload = next.to_be_bytes().to_vec();
            coll.create_item(
                COUNTER_LABEL,
                attrs_ref,
                &payload,
                true,
                COUNTER_CONTENT_TYPE,
            )
            .await
            .map_err(|e| StorageError::Backend(format!("create counter: {e}")))?;
            Ok(next)
        })
    }

    fn next_audio_out_nonce(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let attrs = Self::audio_out_attrs_for(&hex::encode(peer_static_pub));
        self.rmw_u64_slot(
            attrs,
            AUDIO_OUT_NONCE_LABEL,
            AUDIO_OUT_NONCE_CT,
            |current| current.saturating_add(1),
        )
    }

    fn load_audio_in_nonce(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| StorageError::Backend("peer store mutex poisoned".into()))?;
        let pub_hex = hex::encode(peer_static_pub);
        let attrs_owned = Self::audio_in_attrs_for(&pub_hex);
        let attrs_ref: HashMap<&str, &str> = attrs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Self::block_on(async {
            let svc = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| StorageError::Backend(format!("connect: {e}")))?;
            let mut search = svc
                .search_items(attrs_ref)
                .await
                .map_err(|e| StorageError::Backend(format!("search: {e}")))?;
            let item = match search.unlocked.pop().or_else(|| search.locked.pop()) {
                Some(i) => i,
                None => return Ok(0u64),
            };
            if item.is_locked().await.unwrap_or(false) {
                item.unlock()
                    .await
                    .map_err(|e| StorageError::Backend(format!("unlock: {e}")))?;
            }
            let bytes = item
                .get_secret()
                .await
                .map_err(|e| StorageError::Backend(format!("get_secret: {e}")))?;
            if bytes.len() != 8 {
                return Ok(0u64);
            }
            Ok(u64::from_be_bytes(bytes[..8].try_into().unwrap()))
        })
    }

    fn commit_audio_in_nonce(&self, peer_static_pub: &[u8; 32], nonce: u64) -> StorageResult<()> {
        let attrs = Self::audio_in_attrs_for(&hex::encode(peer_static_pub));
        // max() against current so a duplicate frame delivered via both
        // BLE and LAN can't write a smaller nonce.
        let _ = self.rmw_u64_slot(
            attrs,
            AUDIO_IN_NONCE_LABEL,
            AUDIO_IN_NONCE_CT,
            |current| current.max(nonce),
        )?;
        Ok(())
    }

    /// Atomic accept-if-fresh: rmw_u64_slot holds the per-store mutex
    /// plus the secret-service round-trip lock across the read, compare,
    /// and (conditional) commit. Two concurrent callers therefore
    /// linearise — the first to take the lock decides; the second
    /// sees the freshly-committed nonce and returns false. Fixes the
    /// BLE+LAN duplicate-delivery race where both transports could
    /// drive the same frame into the orchestrator.
    fn try_accept_audio_in_nonce(
        &self,
        peer_static_pub: &[u8; 32],
        nonce: u64,
    ) -> StorageResult<bool> {
        let attrs = Self::audio_in_attrs_for(&hex::encode(peer_static_pub));
        // `rmw_u64_slot` runs the closure with the slot's current
        // value under the mutex; whatever the closure returns is
        // what gets persisted. We use an AtomicBool to ferry "did we
        // actually advance" back out — the closure can't return a
        // tuple because the slot is u64-typed (and it must be Send
        // for the dedicated-runtime hop, which rules out Cell).
        use std::sync::atomic::{AtomicBool, Ordering};
        let accepted = AtomicBool::new(false);
        self.rmw_u64_slot(
            attrs,
            AUDIO_IN_NONCE_LABEL,
            AUDIO_IN_NONCE_CT,
            |current| {
                if nonce > current {
                    accepted.store(true, Ordering::Relaxed);
                    nonce
                } else {
                    current
                }
            },
        )?;
        Ok(accepted.load(Ordering::Relaxed))
    }
}

