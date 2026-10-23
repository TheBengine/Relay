use serde::Deserialize;
use std::{
    io::Read,
    net::Ipv4Addr,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Status {
    pub address: Option<String>,
    pub message: String,
}

pub fn valid_address(value: &str) -> bool {
    value.parse::<Ipv4Addr>().is_ok_and(|ip| {
        let b = ip.octets();
        b[0] == 100 && (64..=127).contains(&b[1])
    })
}

pub fn requires_private_connection(address: &str) -> bool {
    url::Url::parse(address)
        .ok()
        .is_some_and(|url| url.scheme() == "http" && url.host_str().is_some_and(valid_address))
}

pub fn ensure_private_connection(address: &str) -> Result<(), String> {
    if crate::embedded_network::ensure_route(address)? {
        return Ok(());
    }
    if requires_private_connection(address) && check()?.address.is_none() {
        return Err("Connect Tailscale and accept the host's network sharing before signing in to this private address.".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct CliStatus {
    #[serde(rename = "BackendState")]
    state: String,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Vec<String>,
}

fn parse(bytes: &[u8]) -> Result<Status, String> {
    let value: CliStatus = serde_json::from_slice(bytes)
        .map_err(|_| "Cannot read Tailscale status. Open Tailscale and try again.")?;
    if value.state != "Running" {
        return Ok(Status {
            address: None,
            message: "Open Tailscale, sign in with your own account, then check again.".into(),
        });
    }
    let address = value
        .addresses
        .into_iter()
        .find(|value| valid_address(value));
    let message = if address.is_some() {
        "Tailscale is connected. Friends still need permission to reach this host."
    } else {
        "Tailscale has no usable IPv4 address yet. Open Tailscale and check its connection."
    };
    Ok(Status {
        address,
        message: message.into(),
    })
}

#[cfg(windows)]
pub fn check() -> Result<Status, String> {
    use std::os::windows::process::CommandExt;
    let path = PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe");
    if !path.is_file() {
        return Ok(Status {
            address: None,
            message: "Install Tailscale on both PCs, then sign in with your own accounts.".into(),
        });
    }
    let mut child = Command::new(path)
        .args(["status", "--json", "--peers=false"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|_| "Cannot check Tailscale. Open it and try again.")?;
    let output = child.stdout.take().ok_or("Cannot read Tailscale status.")?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output.take(65_537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut bytes = None;
    loop {
        if bytes.is_none() {
            if let Ok(result) = receiver.try_recv() {
                match result {
                    Ok(value) if value.len() <= 65_536 => bytes = Some(value),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("Tailscale returned an invalid status.".into());
                    }
                }
            }
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                if !exit.success() {
                    return Err("Tailscale is not available. Open it and try again.".into());
                }
                let bytes = match bytes {
                    Some(bytes) => bytes,
                    None => receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .map_err(|_| "Tailscale did not return a status.")?
                        .map_err(|_| "Cannot read Tailscale status.")?,
                };
                if bytes.len() > 65_536 {
                    return Err("Tailscale returned an invalid status.".into());
                }
                return parse(&bytes);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Cannot check Tailscale.".into());
            }
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Tailscale did not respond. Open it and try again.".into());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(not(windows))]
pub fn check() -> Result<Status, String> {
    Err("This Tailscale setup currently requires the Windows app.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_uses_only_running_own_node_addresses() {
        let value = parse(br#"{"BackendState":"Running","TailscaleIPs":["192.168.1.1","100.70.1.2"],"Peer":{"other":{"TailscaleIPs":["100.70.9.9"]}}}"#).unwrap();
        assert_eq!(value.address.as_deref(), Some("100.70.1.2"));
        assert!(
            parse(br#"{"BackendState":"NeedsLogin","TailscaleIPs":["100.70.1.2"]}"#)
                .unwrap()
                .address
                .is_none()
        );
        assert!(parse(
            br#"{"BackendState":"Running","Peer":{"other":{"TailscaleIPs":["100.70.9.9"]}}}"#
        )
        .unwrap()
        .address
        .is_none());
    }
    #[test]
    fn address_cannot_be_a_url_or_foreign_interface() {
        for value in [
            "127.0.0.1",
            "192.168.1.2",
            "100.128.0.1",
            "https://100.70.1.2",
            "100.70.1.2 -flag",
        ] {
            assert!(!valid_address(value));
        }
        assert!(valid_address("100.64.0.1"));
        assert!(valid_address("100.127.255.254"));
    }
    #[test]
    fn private_http_signin_requires_network_while_local_and_public_tls_do_not() {
        assert!(requires_private_connection("http://100.70.1.2:4580"));
        assert!(!requires_private_connection("http://127.0.0.1:4580"));
        assert!(!requires_private_connection("https://relay.example"));
        assert!(!requires_private_connection("http://192.168.1.2:4580"));
    }
}
