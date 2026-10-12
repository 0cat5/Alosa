//! Advertises the server on the local network so devices can find it
//! without any configuration.

use alosa_protocol::{DEVICE_WS_PATH, MDNS_SERVICE_TYPE};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::info;

/// Starts advertising `_alosa._tcp`. Keep the returned daemon alive for as
/// long as the advertisement should stay up.
pub fn advertise(port: u16) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let host = hostname();
    let properties = [
        ("version", env!("CARGO_PKG_VERSION")),
        ("path", DEVICE_WS_PATH),
    ];
    let service = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        "Alosa Server",
        &format!("{host}.local."),
        "",
        port,
        &properties[..],
    )?
    .enable_addr_auto();
    daemon.register(service)?;
    info!(
        service = MDNS_SERVICE_TYPE,
        host, port, "advertising via mDNS"
    );
    Ok(daemon)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "alosa-server".to_string())
}
