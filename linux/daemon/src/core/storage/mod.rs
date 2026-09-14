//! Local storage backends for V1 secrets and trusted-peer metadata
//! (spec §3.2 and §3.3).

// The trusted-peer TRAIT and record: platform-neutral, like the identity trait
// below it.
pub mod peers;

// The Credential Manager naming scheme. Windows-only in use, compiled
// everywhere so its "a counter is not a peer" rule can be tested here.
pub mod credential_names;

// Secret Service is the LINUX backend for both traits.
#[cfg(target_os = "linux")]
pub mod peers_secret_service;
#[cfg(target_os = "linux")]
pub mod secret_service;

// Windows Credential Manager, implementing the same two.
#[cfg(target_os = "windows")]
pub mod windows_credentials;

use std::sync::{Arc, Mutex};
// Only the Secret Service runtime below needs it, and that is Linux-only.
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use crate::core::identity::{IdentityRecord, IdentityPublicView, Platform};
use crate::core::crypto::x25519::{X25519Sec, X25519SecBytes};

/// Dedicated runtime for ALL Secret Service (zbus) traffic.
///
/// `secret-service` v5 with the `rt-tokio-crypto-rust` feature spawns its
/// zbus I/O tasks onto `Handle::current()` at connection time. The old
/// store pattern — `block_in_place(|| Handle::current().block_on(fut))`
/// from the sync trait methods — therefore made every store call depend
/// on FREE workers of the *ambient* runtime: under a burst of concurrent
/// calls (earbuds switch at call-end: inbound-nonce checks from both
/// transports + outbound nonces + heartbeat `list()` at once) every
/// worker parked inside `block_on` while the zbus futures they waited on
/// had no worker left to be polled on — a permanent, total runtime
/// freeze (live-hit 2026-06-11 during the call-end buds handoff).
///
/// Running the future on THIS runtime breaks the cycle: `block_on`
/// enters the dedicated runtime's context, so the zbus tasks land on its
/// own worker thread and a store call always completes no matter how
/// starved the ambient runtime is.
/// Linux-only, like the Secret Service backend it exists for: the deadlock it
/// avoids is a zbus one.
#[cfg(target_os = "linux")]
static SECRET_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

#[cfg(target_os = "linux")]
fn secret_rt() -> &'static tokio::runtime::Runtime {
    SECRET_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("vortex-secret-store")
            .enable_all()
            .build()
            .expect("secret-store runtime build")
    })
}

/// Drive a Secret Service future to completion from sync code, pinned to
/// the dedicated [`SECRET_RT`] runtime. Safe to call from any thread:
/// ambient multi-thread workers hand their core off via `block_in_place`,
/// current-thread runtimes (e.g. `#[tokio::test]`) hop to a scoped thread
/// (blocking them in place would panic), plain threads just block.
#[cfg(target_os = "linux")]
pub(crate) fn secret_block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| secret_rt().block_on(fut))
        }
        Ok(_) => std::thread::scope(|s| {
            s.spawn(|| secret_rt().block_on(fut))
                .join()
                .expect("secret-store scoped thread panicked")
        }),
        Err(_) => secret_rt().block_on(fut),
    }
}

/// Errors surfaced by storage backends.
#[derive(Debug)]
pub enum StorageError {
    NotFound,
    Backend(String),
    /// The desktop keyring is locked and the unlock was refused, dismissed,
    /// or impossible (no prompter on the session bus). Distinct from
    /// [`Self::Backend`] because it is the one storage failure the *user*
    /// can fix, so callers surface it instead of dying silently.
    Locked(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::Backend(msg) => write!(f, "backend error: {msg}"),
            Self::Locked(msg) => write!(f, "keyring locked: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

pub type StorageResult<T> = Result<T, StorageError>;

/// Fetch the default collection, unlocking it first when the session's
/// keyring has it locked.
///
/// **Why this is not just `get_default_collection()`:** that call succeeds
/// on a *locked* collection — only the write that follows fails, with
/// `org.freedesktop.Secret.Error.IsLocked`. So the availability probe in
/// each store's `new()` passes and the process dies later, at first save,
/// with an error nobody attributes to the keyring.
///
/// That is the *normal* state on any session whose `default` alias points
/// at a keyring PAM does not unlock at login. Live-hit on Manjaro/KDE
/// 2026-08-25: gnome-keyring owned `org.freedesktop.secrets` (its D-Bus
/// service file wins over KDE's ksecretd), its `login` keyring was
/// unlocked, but the `default` alias pointed at a second, password-
/// protected keyring. Identity init failed, the Tauri worker returned
/// before it ever opened the BLE adapter, and the pairing radar sat
/// silently empty — the phone was advertising correctly the whole time.
///
/// [`secret_service::Collection::unlock`] drives the Secret Service
/// Unlock + Prompt handshake, so the desktop's own prompter asks for the
/// password (gcr on GNOME, ksecretd/KWallet on KDE). Two constraints
/// follow from the D-Bus API and both are satisfied by construction here:
///
/// 1. The prompt object is owned by the **calling connection** and is
///    destroyed with it, so the unlock must share one connection with the
///    write it enables — i.e. live inside the same [`secret_block_on`]
///    future. (Splitting them across two `busctl` calls fails with
///    "no such object", which is how this was first confirmed.)
/// 2. It costs one extra round trip, not a prompt, once the collection is
///    unlocked: the service returns the path in `unlocked` and
///    `lock_or_unlock` skips the prompt entirely. Safe to call before
///    every write rather than tracking unlock state ourselves.
// `::secret_service` — the absolute crate path is required here: the sibling
// module `storage::secret_service` shadows the crate name inside this module.
//
// Linux-gated with the backends it serves: the `secret_service` crate is a
// Linux-only dependency, so naming its types off Linux does not compile.
#[cfg(target_os = "linux")]
pub(crate) async fn unlocked_default_collection<'a>(
    service: &'a ::secret_service::SecretService<'a>,
) -> StorageResult<::secret_service::Collection<'a>> {
    let collection = service
        .get_default_collection()
        .await
        .map_err(|e| StorageError::Backend(format!("default collection: {e}")))?;
    if collection.is_locked().await.unwrap_or(false) {
        collection
            .unlock()
            .await
            .map_err(|e| StorageError::Locked(format!("unlock refused or unavailable: {e}")))?;
    }
    Ok(collection)
}

/// Storage backend for local Vortex identity (private + public material).
///
/// `static_priv` MUST be persisted via a platform secure-storage backend
/// (Secret Service on Linux, Keystore on Android — see §3.2). The other
/// fields MAY be plaintext local storage.
pub trait IdentityStore: Send + Sync {
    fn save(&self, record: &IdentityRecord) -> StorageResult<()>;
    fn load(&self) -> StorageResult<IdentityRecord>;
    fn forget(&self) -> StorageResult<()>;
    fn exists(&self) -> bool {
        self.load().is_ok()
    }
}

/// Public view + helper for any store that conforms to [`IdentityStore`].
pub fn load_or_generate<S: IdentityStore + ?Sized>(
    store: &S,
    platform: Platform,
) -> StorageResult<IdentityRecord> {
    match store.load() {
        Ok(rec) => Ok(rec),
        Err(StorageError::NotFound) => {
            let rec = IdentityRecord::generate(platform);
            store.save(&rec)?;
            Ok(rec)
        }
        Err(err) => Err(err),
    }
}

/// In-memory backend. Useful for tests and for early development before
/// Secret Service / Keystore wiring is in place.
#[derive(Default, Clone)]
pub struct InMemoryIdentityStore {
    inner: Arc<Mutex<Option<StoredIdentity>>>,
}

#[derive(Clone)]
struct StoredIdentity {
    public: IdentityPublicView,
    static_priv: X25519SecBytes,
}

impl InMemoryIdentityStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdentityStore for InMemoryIdentityStore {
    fn save(&self, record: &IdentityRecord) -> StorageResult<()> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(StoredIdentity {
            public: record.into(),
            static_priv: record.static_priv.0,
        });
        Ok(())
    }

    fn load(&self) -> StorageResult<IdentityRecord> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stored = g.as_ref().ok_or(StorageError::NotFound)?;

        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&stored.public.device_id);
        let platform = stored.public.platform;
        Ok(IdentityRecord::from_private(
            platform,
            device_id,
            stored.static_priv,
            stored.public.created_at,
        ))
    }

    fn forget(&self) -> StorageResult<()> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_in_memory() {
        let store = InMemoryIdentityStore::new();
        assert!(!store.exists());

        let record = IdentityRecord::generate(Platform::Linux);
        store.save(&record).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.device_id, record.device_id);
        assert_eq!(loaded.static_priv.0, record.static_priv.0);
        assert_eq!(loaded.static_pub.0, record.static_pub.0);
        assert_eq!(loaded.platform, record.platform);
    }

    #[test]
    fn forget_clears_store() {
        let store = InMemoryIdentityStore::new();
        store.save(&IdentityRecord::generate(Platform::Linux)).unwrap();
        assert!(store.exists());
        store.forget().unwrap();
        assert!(!store.exists());
    }

    #[test]
    fn load_or_generate_creates_then_reuses() {
        let store = InMemoryIdentityStore::new();
        let first = load_or_generate(&store, Platform::Linux).unwrap();
        let second = load_or_generate(&store, Platform::Linux).unwrap();
        assert_eq!(first.device_id, second.device_id);
        assert_eq!(first.static_pub.0, second.static_pub.0);
    }
}

// Suppress dead-code warning for X25519Sec (used in production via record).
#[allow(dead_code)]
fn _keep_x25519_sec_in_use(_: X25519Sec) {}
