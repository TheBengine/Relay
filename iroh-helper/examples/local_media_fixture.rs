//! Offline acceptance driver, never included in the distributed helper.
//! Uses the real control API and QUIC implementation with loopback discovery.
use relay_iroh::{control, Network};
use std::{path::PathBuf, time::Duration};
use tokio::net::TcpListener;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let secret = std::env::var("RELAY_IROH_CONTROL_TOKEN")?;
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("Owned state directory required"))?,
    );
    anyhow::ensure!(directory.is_absolute(), "Absolute owned directory required");
    std::fs::create_dir_all(&directory)?;
    let service = control::Control::new(&secret, directory, Network::Loopback)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    println!(
        "{}",
        serde_json::json!({"control_url":format!("http://{}",listener.local_addr()?),"pid":std::process::id()})
    );
    let task = tokio::spawn(control::serve(listener, service.clone()));
    // An abandoned acceptance process cannot remain alive indefinitely.
    tokio::select! {
        _ = service.stop.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(600)) => {},
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    service.shutdown().await;
    task.abort();
    Ok(())
}
