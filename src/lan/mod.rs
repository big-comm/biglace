//! The local network without a server: devices on one network pair once by
//! comparing a six-digit code, then talk over TLS, each proving the device
//! key pinned at pairing. People on a BigScale network keep using the
//! tailnet, which already goes straight across the local network.

pub mod bridge;
pub mod discovery;
pub mod handshake;
pub mod peers;
pub mod pairing;
pub mod tls;

/// Where BigLace listens on the local network (the tailnet agent has 41980).
pub const PORT: u16 = 41981;
