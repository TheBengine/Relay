//! Constrain automatic invitation probes and pin DNS results to the checked IPs.
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy)]
pub struct Subnet(pub IpAddr, pub u8);

fn v4(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

fn on_link(ip: IpAddr, subnet: Subnet) -> bool {
    match (v4(ip), v4(subnet.0), subnet.1) {
        (IpAddr::V4(ip), IpAddr::V4(local), bits @ 1..=32) => {
            let mask = u32::MAX << (32 - bits);
            (u32::from(ip) & mask) == (u32::from(local) & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(local), bits @ 1..=128) => {
            let mask = u128::MAX << (128 - bits);
            (u128::from(ip) & mask) == (u128::from(local) & mask)
        }
        _ => false,
    }
}

pub fn allowed_ip(ip: IpAddr, subnets: &[Subnet]) -> bool {
    match v4(ip) {
        IpAddr::V4(ip) => {
            if ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_link_local()
                || ip.octets()[0] == 0
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
            {
                return false;
            }
            if ip.is_loopback() {
                return true;
            }
            !ip.is_private() || subnets.iter().any(|s| on_link(ip.into(), *s))
        }
        IpAddr::V6(ip) => {
            if ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local() {
                return false;
            }
            if ip.is_loopback() {
                return true;
            }
            (ip.segments()[0] & 0xfe00) != 0xfc00 || subnets.iter().any(|s| on_link(ip.into(), *s))
        }
    }
}

pub fn resolve(
    address: &str,
    primary_port: u16,
    subnets: &[Subnet],
) -> Result<Vec<SocketAddr>, String> {
    let url = url::Url::parse(address).map_err(|_| "Invalid invitation address")?;
    let port = url
        .port_or_known_default()
        .ok_or("Invalid invitation port")?;
    // A public HTTPS invitation can also offer the same Relay community on
    // its ordinary API port at home. Keep this exception tied to the parser's
    // exact validated Relay service, not to arbitrary alternative ports.
    let relay_api = port == 4580
        && crate::invitation_routes::valid_direct_address(address).is_ok();
    if !matches!(url.scheme(), "http" | "https")
        || !(port == primary_port || relay_api || url.scheme() == "https" && port == 443)
    {
        return Err("This extra address uses an unrelated port".into());
    }
    let ips: Vec<_> = match url.host().ok_or("Invalid invitation host")? {
        url::Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Domain(name) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "Cannot check this invitation address")?;
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::lookup_host((name, port)),
                )
                .await
                .map_err(|_| "Address lookup timed out")?
                .map(|v| v.collect::<Vec<_>>())
                .map_err(|_| "Address lookup failed")
            })?
        }
    };
    if ips.is_empty() || ips.len() > 64 || ips.iter().any(|v| !allowed_ip(v.ip(), subnets)) {
        return Err("This address is outside your local network or uses a reserved network".into());
    }
    Ok(ips)
}

#[cfg(not(windows))]
pub fn local_subnets() -> Vec<Subnet> {
    Vec::new()
}

#[cfg(windows)]
pub fn local_subnets() -> Vec<Subnet> {
    use windows::Win32::{
        NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
        Networking::WinSock::*,
    };
    let mut size = 16_384u32;
    for _ in 0..3 {
        if size > 1_048_576 {
            return Vec::new();
        }
        // u64 alignment covers all native adapter/address structures, also on x86.
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        let start = storage.as_mut_ptr() as usize;
        let end = start + storage.len() * 8;
        let contains =
            |p: usize, n: usize| p >= start && p.checked_add(n).is_some_and(|p| p <= end);
        let first = storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the OS writes into our aligned, bounded allocation. Every linked
        // structure and sockaddr is checked inside that allocation before reading.
        unsafe {
            let status = GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(first),
                &mut size,
            );
            if status == 111 {
                continue;
            } // ERROR_BUFFER_OVERFLOW: retry the returned size.
            if status != 0 {
                return Vec::new();
            }
            let mut result = Vec::new();
            let mut adapter = first;
            let mut visited = 0;
            while !adapter.is_null()
                && contains(adapter as usize, size_of::<IP_ADAPTER_ADDRESSES_LH>())
                && visited < 1024
            {
                visited += 1;
                if (*adapter).OperStatus == IfOperStatusUp {
                    let mut address = (*adapter).FirstUnicastAddress;
                    let mut seen = 0;
                    while !address.is_null()
                        && contains(address as usize, size_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>())
                        && seen < 1024
                    {
                        seen += 1;
                        let sock = (*address).Address;
                        let ptr = sock.lpSockaddr;
                        let bits = (*address).OnLinkPrefixLength;
                        if contains(ptr as usize, size_of::<SOCKADDR>()) {
                            if (*ptr).sa_family == AF_INET
                                && sock.iSockaddrLength as usize >= size_of::<SOCKADDR_IN>()
                                && contains(ptr as usize, size_of::<SOCKADDR_IN>())
                                && bits <= 32
                            {
                                let ip = std::net::Ipv4Addr::from(
                                    (*ptr.cast::<SOCKADDR_IN>())
                                        .sin_addr
                                        .S_un
                                        .S_addr
                                        .to_ne_bytes(),
                                );
                                result.push(Subnet(ip.into(), bits));
                            } else if (*ptr).sa_family == AF_INET6
                                && sock.iSockaddrLength as usize >= size_of::<SOCKADDR_IN6>()
                                && contains(ptr as usize, size_of::<SOCKADDR_IN6>())
                                && bits <= 128
                            {
                                let ip = std::net::Ipv6Addr::from(
                                    (*ptr.cast::<SOCKADDR_IN6>()).sin6_addr.u.Byte,
                                );
                                result.push(Subnet(ip.into(), bits));
                            }
                        }
                        address = (*address).Next;
                    }
                }
                adapter = (*adapter).Next;
            }
            return result;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_dns_addresses_are_pinned_for_both_health_requests_without_authentication() {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = 0;
            while requests < 2 && Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut data = [0u8; 4096];
                let n = socket.read(&mut data).unwrap();
                let request = String::from_utf8_lossy(&data[..n]).to_ascii_lowercase();
                assert!(request.starts_with("get /api/health "));
                assert!(!request.contains("authorization:") && !request.contains("cookie:"));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
                requests += 1;
            }
            assert_eq!(requests, 2);
        });
        let result = crate::connection_path::health_probe_pinned(
            &format!("http://rebound.example.invalid:{}", address.port()),
            Some(&[address]),
        );
        server.join().unwrap();
        assert_eq!(result.unwrap().1, br#"{"ok":true}"#);
    }

    #[test]
    fn private_means_actual_subnet_and_mapped_ipv6_cannot_bypass_it() {
        let subnets = [
            Subnet("192.168.111.5".parse().unwrap(), 24),
            Subnet("fd12:3456::1".parse().unwrap(), 64),
        ];
        for ip in ["192.168.111.6", "fd12:3456::99", "1.1.1.1", "127.0.0.1"] {
            assert!(allowed_ip(ip.parse().unwrap(), &subnets), "{ip}");
        }
        for ip in [
            "10.0.0.1",
            "192.168.1.1",
            "::ffff:10.0.0.1",
            "169.254.169.254",
            "100.64.0.5",
            "fd12:3457::1",
            "224.0.0.1",
            "::",
            "0.0.0.0",
            "fe80::1",
        ] {
            assert!(!allowed_ip(ip.parse().unwrap(), &subnets), "{ip}");
        }
        assert!(resolve("https://10.0.0.1:22", 4580, &subnets).is_err());
        assert!(resolve("http://10.0.0.1:4580", 4580, &subnets).is_err());
        assert!(resolve("https://192.168.111.6:443", 8443, &subnets).is_ok());
        assert!(resolve("https://192.168.111.6:4580", 8443, &subnets).is_ok());
        assert!(resolve("http://192.168.111.6:4580", 443, &subnets).is_ok());
        assert!(resolve("https://192.168.111.6:8444", 8443, &subnets).is_err());
        // An advertised Relay port does not authorize other private networks,
        // mapped addresses, CGNAT services or credentials in a probe URL.
        for address in [
            "http://10.0.0.1:4580",
            "https://[::ffff:10.0.0.1]:4580",
            "https://100.64.0.5:4580",
            "http://synthetic:synthetic@192.168.111.6:4580",
            "http://192.168.111.6:4580?token=synthetic",
        ] {
            assert!(resolve(address, 443, &subnets).is_err(), "{address}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_adapter_inventory_is_bounded_and_has_valid_prefixes() {
        let subnets = local_subnets();
        assert!(
            !subnets.is_empty(),
            "Windows should report at least its loopback adapter"
        );
        assert!(subnets
            .iter()
            .all(|s| s.1 <= if s.0.is_ipv4() { 32 } else { 128 }));
    }
}
