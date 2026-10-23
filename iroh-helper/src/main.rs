//! relay-iroh: Relay's account-free route, run by Relay as a small helper.
//!
//! Usage: `relay-iroh --state-dir <absolute folder>` with the control secret in
//! `RELAY_IROH_CONTROL_TOKEN`. It prints one line of JSON naming its loopback
//! control address, then waits for Relay.
use relay_iroh::{control, Network};
use std::{path::PathBuf, time::Duration};
use tokio::net::TcpListener;

fn state_dir(args: &[String]) -> Result<PathBuf, &'static str> {
    let at = args
        .iter()
        .position(|a| a == "--state-dir")
        .ok_or("--state-dir is required")?;
    let dir = args
        .get(at + 1)
        .filter(|v| !v.is_empty() && !v.starts_with("--"))
        .map(PathBuf::from)
        .ok_or("--state-dir needs a folder")?;
    if !dir.is_absolute() {
        return Err("--state-dir must be an absolute folder");
    }
    Ok(dir)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(secret) = std::env::var("RELAY_IROH_CONTROL_TOKEN")
        .ok()
        .filter(|s| (32..=256).contains(&s.len()))
    else {
        eprintln!("Private app control token is required");
        std::process::exit(2);
    };
    let dir = match state_dir(&args) {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("The state folder could not be created");
        std::process::exit(2);
    }
    // A test build may run without relays; a release build never does.
    let network = if cfg!(debug_assertions)
        && std::env::var("RELAY_IROH_NETWORK").as_deref() == Ok("loopback")
    {
        Network::Loopback
    } else {
        Network::Internet
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the async runtime starts");
    runtime.block_on(async move {
        let Ok(control) = control::Control::new(&secret, dir, network) else {
            std::process::exit(2);
        };
        let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
            eprintln!("Private app control listener unavailable");
            std::process::exit(2);
        };
        let address = listener.local_addr().expect("a bound listener has an address");
        // Startup output holds no secret.
        println!(
            "{}",
            serde_json::json!({"version": 1, "pid": std::process::id(), "control_url": format!("http://{address}"), "phase": "idle"})
        );
        let server = tokio::spawn(control::serve(listener, control.clone()));
        tokio::select! {
            _ = control.stop.notified() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        // Let the shutdown answer leave before the connection goes away.
        tokio::time::sleep(Duration::from_millis(100)).await;
        control.shutdown().await;
        server.abort();
    });
}
