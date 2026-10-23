//! Offline release preflight. It never generates keys or contacts a publisher.
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let require = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--require-configured" => true,
        _ => {
            eprintln!("Usage: relay-publisher-check [--require-configured]");
            return ExitCode::FAILURE;
        }
    };
    match relay_updater::bundled_publisher() {
        Ok(Some(publisher)) => {
            println!(
                "{}",
                serde_json::json!({
                    "configured": true,
                    "policy_identity": publisher.policy_identity(),
                    "scope": "bundled public policy and root signatures only",
                    "authenticode_verified": false,
                    "repository_or_installer_tested": false,
                })
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("{}", serde_json::json!({"configured": false}));
            if require {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("Publisher configuration rejected: {error}");
            ExitCode::FAILURE
        }
    }
}
