//! Relay's account-free route.
//!
//! A friend is reached by a key instead of an address, so the invitation keeps
//! working when the host's network changes. This crate carries chat (HTTP and
//! WebSocket) and voice (datagrams) between a friend's Relay and a community on
//! the host's PC. Admitted media streams also carry native video and screen sharing
//! to one fixed SFU port; voice keeps its separate datagram path.
//!
//! It runs as its own small program, like `relay-network`, so its dependencies
//! stay out of the main application.
pub mod client;
pub mod control;
pub mod host;
mod media;
pub mod path;
pub mod policy;
pub mod protocol;
pub mod token;

use anyhow::Result;
use iroh::{
    endpoint::{presets, PortmapperConfig},
    Endpoint, RelayMode, SecretKey,
};

/// Where an endpoint finds the other side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// Public relays and n0's address directory. What a real host and friend use.
    Internet,
    /// One loopback address, no relay and no directory. Tests and local checks.
    Loopback,
}

/// Binds one endpoint. A friend binds one with a fresh key; a host with its own.
pub(crate) async fn bind(
    network: &Network,
    key: Option<SecretKey>,
    host: bool,
) -> Result<Endpoint> {
    let alpns = if host {
        vec![protocol::ALPN.to_vec()]
    } else {
        Vec::new()
    };
    let mut builder = match network {
        Network::Internet => Endpoint::builder(presets::N0),
        Network::Loopback => Endpoint::builder(presets::Minimal)
            .clear_address_lookup()
            .portmapper_config(PortmapperConfig::Disabled)
            .clear_ip_transports()
            .relay_mode(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")?,
    }
    .alpns(alpns)
    .transport_config(host::transport());
    if let Some(key) = key {
        builder = builder.secret_key(key);
    }
    Ok(builder.bind().await?)
}

pub(crate) const VOICE_SEND_BUFFER_BYTES: usize = 12_000;
pub(crate) const VOICE_RECEIVE_BUFFER_BYTES: usize = 64 * 1024;
#[derive(Default)]
pub(crate) struct DatagramDrops {
    total: u64,
    reported: u64,
    last: Option<std::time::Instant>,
}
impl DatagramDrops {
    fn record(&mut self, failed: bool) {
        self.record_at(failed, std::time::Instant::now());
    }
    fn record_at(&mut self, failed: bool, now: std::time::Instant) {
        self.total = self.total.saturating_add(u64::from(failed));
        if self.total > self.reported
            && self.last.is_none_or(|at| {
                now.saturating_duration_since(at) >= std::time::Duration::from_secs(10)
            })
        {
            eprintln!(
                "iroh voice datagram send failures: {} ({} new)",
                self.total,
                self.total - self.reported
            );
            self.reported = self.total;
            self.last = Some(now);
        }
    }
}
#[cfg(test)]
mod voice_buffer_tests {
    #[test]
    fn bounded_transport_and_rate_limited_drop_accounting() {
        // The pinned iroh type exposes its inner limits through Debug, not getters.
        let transport = format!("{:?}", super::host::transport());
        assert!(
            transport.contains("datagram_send_buffer_size: 12000"),
            "{transport}"
        );
        assert!(
            transport.contains("datagram_receive_buffer_size: Some(65536)"),
            "{transport}"
        );
        let now = std::time::Instant::now();
        let mut drops = super::DatagramDrops::default();
        drops.record_at(true, now);
        for _ in 0..100 {
            drops.record_at(true, now + std::time::Duration::from_secs(1));
        }
        assert_eq!((drops.total, drops.reported), (101, 1));
        drops.record_at(false, now + std::time::Duration::from_secs(10));
        assert_eq!(drops.reported, 101);
    }
}
