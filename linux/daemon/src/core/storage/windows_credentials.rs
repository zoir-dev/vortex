//! Windows Credential Manager backend for [`IdentityStore`] and [`PeerStore`].
//!
//! The counterpart to Secret Service on Linux: generic credentials, one per
//! record, keyed by target name. `CredWriteW` blobs are DPAPI-encrypted under
//! the user's profile, so the trust model matches — any process running as this
//! user can read them, exactly as any process can read an unlocked Secret
//! Service collection. That is the platform's answer, and it is the same answer
//! the phone side gets from the Android Keystore's non-hardware tier.
//!
//! # Layout
//!
//! Target names are hierarchical so `CredEnumerateW` can list peers with a
//! single wildcard, and so a stray credential is obviously ours:
//!
//! ```text
//! Vortex/identity                        the 90-byte identity record
//! Vortex/peer/<hex>                      the trusted-peer record
//! Vortex/peer/<hex>/counter              reconnect counter (u64 BE)
//! Vortex/peer/<hex>/audio-out-nonce      audio-op send nonce (u64 BE)
//! Vortex/peer/<hex>/audio-in-nonce       highest accepted recv nonce
//! Vortex/peer/<hex>/bonded-addr          BD_ADDR string
//! ```
//!
//! `<hex>` is the peer's static public key, which is the same identity the
//! Linux backend keys on — so the two stores hold the same records under
//! different names, and neither can be confused about which peer is which.
//!
//! # Persistence scope
//!
//! `CRED_PERSIST_LOCAL_MACHINE`, deliberately not `CRED_PERSIST_ENTERPRISE`.
//! Enterprise credentials roam with a domain profile, and a *device* identity
//! key that follows the user to another machine is a different key than the one
//! the phone paired with — the peer would see the same static public key from
//! two devices, which is precisely what the pairing model assumes cannot
//! happen.
//!
//! # Untested
//!
//! Never run. Type-checked against the Win32 metadata only. The pure parts —
//! target-name construction and the counter encoding — live in
//! [`super::credential_names`] and are tested there.

use std::ffi::c_void;

use windows::core::PWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

use super::credential_names::{
    decode_u64, encode_u64, peer_from_target, peer_target, IDENTITY_TARGET, PEER_PREFIX,
    SUB_AUDIO_IN, SUB_AUDIO_OUT, SUB_BONDED, SUB_COUNTER,
};
use super::peers::{PeerStore, TrustedPeer};
use super::{IdentityStore, StorageError, StorageResult};
use crate::core::identity::IdentityRecord;

/// Write (or replace) one generic credential.
fn cred_write(target: &str, blob: &[u8]) -> StorageResult<()> {
    let mut target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut blob = blob.to_vec();
    let mut cred = CREDENTIALW {
        Flags: Default::default(),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        Comment: PWSTR::null(),
        LastWritten: FILETIME::default(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: PWSTR::null(),
        UserName: PWSTR::null(),
    };
    // SAFETY: `cred` borrows two buffers that outlive the call, and CredWriteW
    // copies what it needs before returning.
    unsafe { CredWriteW(&mut cred, 0) }
        .map_err(|e| StorageError::Backend(format!("CredWriteW {target}: {e}")))
}

/// Read one generic credential's blob.
fn cred_read(target: &str) -> StorageResult<Vec<u8>> {
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `out` is ours and freed on every path below.
    let r = unsafe {
        CredReadW(
            windows::core::PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut out,
        )
    };
    if r.is_err() || out.is_null() {
        // A missing credential is NotFound rather than a backend failure: the
        // first run of a fresh install takes that path, and `load_or_generate`
        // distinguishes the two.
        return Err(StorageError::NotFound);
    }
    let blob = unsafe {
        let c = &*out;
        std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
    };
    unsafe { CredFree(out as *mut c_void) };
    Ok(blob)
}

/// Delete one credential. A credential that is already gone is success — this
/// is used by `forget`, which is idempotent by contract.
fn cred_delete(target: &str) -> StorageResult<()> {
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the string outlives the call; no out-params.
    let _ = unsafe {
        CredDeleteW(
            windows::core::PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
        )
    };
    Ok(())
}

/// Every target name matching `Vortex/peer/*`.
fn cred_enumerate_peers() -> StorageResult<Vec<String>> {
    let filter: Vec<u16> = format!("{PEER_PREFIX}*")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut count: u32 = 0;
    let mut creds: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: out-params are ours; the array is freed once, below, and the
    // strings we keep are copied out first.
    let r = unsafe {
        CredEnumerateW(
            windows::core::PCWSTR(filter.as_ptr()),
            None, // no CRED_ENUMERATE_ALL_CREDENTIALS: the filter is the point
            &mut count,
            &mut creds,
        )
    };
    if r.is_err() || creds.is_null() {
        // Nothing stored yet is an empty list, not an error: an unpaired
        // install must not look like a broken credential store.
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for i in 0..count as isize {
        let c = unsafe { &**creds.offset(i) };
        if !c.TargetName.is_null() {
            if let Ok(name) = unsafe { c.TargetName.to_string() } {
                out.push(name);
            }
        }
    }
    unsafe { CredFree(creds as *mut c_void) };
    Ok(out)
}

/// Credential Manager-backed identity store.
pub struct WindowsIdentityStore;

impl IdentityStore for WindowsIdentityStore {
    fn save(&self, record: &IdentityRecord) -> StorageResult<()> {
        cred_write(IDENTITY_TARGET, &record.encode())
    }

    fn load(&self) -> StorageResult<IdentityRecord> {
        let blob = cred_read(IDENTITY_TARGET)?;
        IdentityRecord::decode(&blob).map_err(StorageError::Backend)
    }

    fn forget(&self) -> StorageResult<()> {
        cred_delete(IDENTITY_TARGET)
    }
}

/// Credential Manager-backed trusted-peer store.
pub struct WindowsPeerStore;

impl PeerStore for WindowsPeerStore {
    fn save(&self, peer: &TrustedPeer) -> StorageResult<()> {
        cred_write(&peer_target(&peer.peer_static_pub), &peer.encode())
    }

    fn load(&self, peer_static_pub: &[u8; 32]) -> StorageResult<TrustedPeer> {
        let blob = cred_read(&peer_target(peer_static_pub))?;
        TrustedPeer::decode(&blob)
    }

    /// Every stored peer.
    ///
    /// A record that fails to decode is SKIPPED rather than failing the list:
    /// one corrupt credential must not make the app look unpaired from a peer
    /// that is still fine.
    fn list(&self) -> StorageResult<Vec<TrustedPeer>> {
        let mut out = Vec::new();
        for target in cred_enumerate_peers()? {
            if peer_from_target(&target).is_none() {
                continue; // a /counter or /nonce child, not a peer record
            }
            if let Ok(blob) = cred_read(&target) {
                match TrustedPeer::decode(&blob) {
                    Ok(p) => out.push(p),
                    Err(e) => tracing::warn!("skipping undecodable peer {target}: {e}"),
                }
            }
        }
        Ok(out)
    }

    /// Forget a peer AND everything hung off it.
    ///
    /// The children matter: leaving a stale reconnect counter behind means a
    /// re-pair with the same phone inherits the old peer's counter, and the
    /// first reconnect looks like a rollback.
    fn forget(&self, peer_static_pub: &[u8; 32]) -> StorageResult<()> {
        let base = peer_target(peer_static_pub);
        for sub in ["", SUB_COUNTER, SUB_AUDIO_OUT, SUB_AUDIO_IN, SUB_BONDED] {
            cred_delete(&format!("{base}{sub}"))?;
        }
        Ok(())
    }

    fn load_counter(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let target = format!("{}{SUB_COUNTER}", peer_target(peer_static_pub));
        match cred_read(&target) {
            Ok(blob) => decode_u64(&blob),
            // Never reconnected yet: zero is the documented starting value.
            Err(StorageError::NotFound) => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn bump_counter(&self, peer_static_pub: &[u8; 32], peer_seen: u64) -> StorageResult<u64> {
        let local = self.load_counter(peer_static_pub)?;
        let next = local.max(peer_seen).saturating_add(1);
        let target = format!("{}{SUB_COUNTER}", peer_target(peer_static_pub));
        cred_write(&target, &encode_u64(next))?;
        Ok(next)
    }

    fn load_bonded_addr(&self, peer_static_pub: &[u8; 32]) -> StorageResult<Option<String>> {
        let target = format!("{}{SUB_BONDED}", peer_target(peer_static_pub));
        match cred_read(&target) {
            Ok(blob) => Ok(String::from_utf8(blob).ok().filter(|s| !s.is_empty())),
            Err(StorageError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save_bonded_addr(&self, peer_static_pub: &[u8; 32], addr: &str) -> StorageResult<()> {
        let target = format!("{}{SUB_BONDED}", peer_target(peer_static_pub));
        cred_write(&target, addr.as_bytes())
    }

    // The audio-op nonces are REPLAY DEFENCES, and the trait's defaults answer
    // `Ok(0)` forever — which would hand out nonce 0 for every outbound frame
    // and accept every inbound one. They are unreachable on Windows today
    // (no audio backend, so AUDIO_OP frames are dropped before they get here),
    // but a default that is silently wrong is a trap for whoever wires audio up
    // later, so they are implemented rather than left to it.

    fn next_audio_out_nonce(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let target = format!("{}{SUB_AUDIO_OUT}", peer_target(peer_static_pub));
        let current = match cred_read(&target) {
            Ok(blob) => decode_u64(&blob)?,
            Err(StorageError::NotFound) => 0,
            Err(e) => return Err(e),
        };
        let next = current.saturating_add(1);
        // Persist BEFORE returning: a crash between handing out a nonce and
        // storing it must lose the nonce, never reuse it.
        cred_write(&target, &encode_u64(next))?;
        Ok(next)
    }

    fn load_audio_in_nonce(&self, peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        let target = format!("{}{SUB_AUDIO_IN}", peer_target(peer_static_pub));
        match cred_read(&target) {
            Ok(blob) => decode_u64(&blob),
            Err(StorageError::NotFound) => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn commit_audio_in_nonce(&self, peer_static_pub: &[u8; 32], nonce: u64) -> StorageResult<()> {
        let target = format!("{}{SUB_AUDIO_IN}", peer_target(peer_static_pub));
        cred_write(&target, &encode_u64(nonce))
    }
}
