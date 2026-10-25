//! Release preflight. Offline by default; explicit repository verification can
//! download a trusted target into an explicit cache but never executes it.
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--repository") {
        return match verify_repository(&arguments) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Repository verification failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let require = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--require-configured" => true,
        _ => {
            eprintln!("Usage: relay-publisher-check [--require-configured]\n       relay-publisher-check --repository CACHE --installed-sequence YYYYMMDDhhmmss [--download]");
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

fn verify_repository(args: &[String]) -> Result<(), String> {
    use relay_updater::{bundled_publisher, Check, Engine, InstalledRelease};
    let (cache, sequence, download) = match args {
        [mode, cache, flag, sequence]
            if mode == "--repository" && flag == "--installed-sequence" =>
        {
            (cache, sequence, false)
        }
        [mode, cache, flag, sequence, download]
            if mode == "--repository"
                && flag == "--installed-sequence"
                && download == "--download" =>
        {
            (cache, sequence, true)
        }
        _ => {
            return Err(
                "Use --repository CACHE --installed-sequence YYYYMMDDhhmmss [--download].".into(),
            )
        }
    };
    let cache = std::path::PathBuf::from(cache);
    if !cache.is_absolute() {
        return Err("The verification cache must be an explicit absolute path.".into());
    }
    let publisher = bundled_publisher()?.ok_or("No publisher is configured.")?;
    let identity = publisher.policy_identity().to_owned();
    let engine = Engine::new(
        publisher,
        InstalledRelease {
            version: env!("CARGO_PKG_VERSION").into(),
            sequence: sequence.clone(),
        },
        cache,
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        match engine.check().await? {
            Check::Current => println!("{}", serde_json::json!({"configured":true,"policy_identity":identity,"repository_verified":true,"status":"current","downloaded":false,"executed":false})),
            Check::Available(release) => {
                let ready = if download { Some(engine.download(&release).await?) } else { None };
                if let Some(ready) = &ready { ready.revalidate()?; }
                println!("{}", serde_json::json!({
                    "configured":true,"policy_identity":identity,"repository_verified":true,
                    "status":"available","version":release.version,"sequence":release.sequence,
                    "handoff":release.handoff,"installer_sha256":release.installer_sha256,
                    "downloaded":ready.is_some(),"download":ready.as_ref().map(|r| r.path()),
                    "executed":false,"authenticode_verified":false,
                }));
            }
        }
        Ok(())
    })
}
