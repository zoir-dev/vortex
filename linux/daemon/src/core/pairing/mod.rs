//! Pairing orchestration per spec §6.

pub mod backoff;

// The XX pairing and IK reconnect handshakes. Platform-neutral: they take a
// `&dyn core::platform::GattLink`, so the same Noise state machine runs over
// BlueZ, over WinRT, and over a test fake with no radio at all. Only the
// transport differs, which is the whole point of the seam.
//
// (The LAN side is separate either way: `lan::tcp_client` runs its own IK over
// TCP.)
pub mod handshake;
pub mod reconnect;
