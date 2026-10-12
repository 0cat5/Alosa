//! Shared protocol types for Alosa devices and the Alosa server.
//!
//! The types are generated from `protocol/alosa/device/v1/device.proto`, so the
//! server and every device agree on the wire format by construction.

pub mod noise;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/alosa.device.v1.rs"));
}

/// Protocol version this crate speaks. Sent in `Hello.protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// mDNS service type the server advertises and devices browse for.
pub const MDNS_SERVICE_TYPE: &str = "_alosa._tcp.local.";

/// WebSocket path devices connect to.
pub const DEVICE_WS_PATH: &str = "/device";

/// Default server port.
pub const DEFAULT_PORT: u16 = 8765;
