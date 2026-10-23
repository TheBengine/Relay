use sha2::{Digest, Sha256};
use std::{env, fs, io::Read, path::Path, path::PathBuf, process::Command};
#[path = "build_support/build_stamp.rs"]
mod build_stamp;
#[path = "build_support/source_identity.rs"]
mod source_identity;

fn emit_build_metadata() {
    println!("cargo:rerun-if-env-changed=RELAY_BUILD_SEQUENCE");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .expect("Relay workspace root");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    source_identity::validate_build_output(root, &output).expect("Unsafe Relay target directory");
    let sources =
        source_identity::source_identity(root).expect("Cannot fingerprint Relay source inputs");
    for path in &sources.watched {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // Git describes the checkout at this compilation, not the user's later
    // checkout. Git-only activity must not invalidate an unchanged client.
    let empty_config = output.join("relay-provenance-empty.config");
    source_identity::reject_redirects(&empty_config).expect("Unsafe Git config scratch path");
    fs::write(&empty_config, []).expect("Create owned empty Git config");
    let revision = source_identity::git_identity(root, &empty_config);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Build clock precedes the Unix epoch")
        .as_secs();
    let sequence = env::var("RELAY_BUILD_SEQUENCE").ok();
    let epoch = env::var("SOURCE_DATE_EPOCH").ok();
    let stamp = build_stamp::BuildStamp::resolve(sequence.as_deref(), epoch.as_deref(), now)
        .expect("Invalid Relay build timestamp");
    println!("cargo:rustc-env=RELAY_BUILD_SEQUENCE={}", stamp.sequence);
    println!("cargo:rustc-env=RELAY_BUILD_DATE={}", stamp.date);
    println!("cargo:rustc-env=RELAY_BUILD_UTC={}", stamp.utc);
    println!("cargo:rustc-env=RELAY_SOURCE_SHA256={}", sources.sha256);
    println!(
        "cargo:rustc-env=RELAY_SOURCE_SCOPE={}",
        source_identity::SOURCE_SCOPE
    );
    println!("cargo:rustc-env=RELAY_GIT_SHA={}", revision.revision);
    println!(
        "cargo:rustc-env=RELAY_GIT_DIRTY={}",
        match revision.dirty {
            Some(true) => "true",
            Some(false) => "false",
            None => "unknown",
        }
    );
}

fn digest(path: &Path) -> String {
    let mut file = fs::File::open(path).expect("Native cryptor build input missing");
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = file.read(&mut buffer).expect("Native SDK read failed");
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    format!("{:x}", hash.finalize())
}

fn verify_native_cryptor() {
    if env::var_os("CARGO_FEATURE_NATIVE_RTC").is_none() {
        return;
    }
    println!("cargo:rerun-if-env-changed=LK_CUSTOM_WEBRTC");
    let sdk = PathBuf::from(env::var_os("LK_CUSTOM_WEBRTC").expect(
        "Prepare Relay's patched SDK with scripts/prepare-webrtc.ps1, then set LK_CUSTOM_WEBRTC",
    ));
    for path in sdk.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(path).expect("Native SDK path missing");
        assert!(
            !metadata.file_type().is_symlink(),
            "SDK symbolic link refused"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(
                metadata.file_attributes() & 0x400,
                0,
                "SDK reparse point refused"
            );
        }
    }
    let marker = sdk.join("relay-frame-crypto.json");
    println!("cargo:rerun-if-changed={}", marker.display());
    let metadata =
        fs::metadata(&marker).expect("Unpatched native SDK: run scripts/prepare-webrtc.ps1");
    assert!(metadata.len() <= 8 * 1024 * 1024, "Oversized SDK marker");
    let mut bytes = fs::read(&marker).expect("Native SDK marker read failed");
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).expect("Invalid native SDK marker");
    assert_eq!(manifest["schema"].as_u64(), Some(1));
    assert_eq!(manifest["patch"].as_str(), Some("relay-frame-iv-v1"));
    assert_eq!(
        manifest["base_archive_sha256"].as_str(),
        Some("5c2349c960bae4f06f71c102f58552d0d09c912a142811acfcadfb7c883cbf58")
    );
    let patch = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../vendor/relay-frame-crypto/frame_crypto_transformer.cc");
    println!("cargo:rerun-if-changed={}", patch.display());
    let expected_patch = "79813011a8cfd41a0ed872a1c30fc4ac64f2fd1baae4e61022e7822a98c3590a";
    assert_eq!(
        digest(&patch),
        expected_patch,
        "Native patch source changed"
    );
    assert_eq!(manifest["patch_sha256"].as_str(), Some(expected_patch));
    let recipe = patch
        .parent()
        .unwrap()
        .join("../../scripts/prepare-webrtc.ps1");
    println!("cargo:rerun-if-changed={}", recipe.display());
    assert_eq!(
        manifest["recipe_sha256"].as_str(),
        Some(digest(&recipe).as_str())
    );
    let expected = manifest["files"]
        .as_object()
        .expect("Missing SDK file inventory");
    let mut directories = vec![sdk.clone()];
    let mut seen = 0;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).expect("SDK directory read failed") {
            let entry = entry.expect("SDK entry read failed");
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("SDK metadata read failed");
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                assert_eq!(
                    metadata.file_attributes() & 0x400,
                    0,
                    "SDK reparse point refused"
                );
            }
            assert!(
                !metadata.file_type().is_symlink(),
                "SDK symbolic link refused"
            );
            if metadata.is_dir() {
                directories.push(path);
                continue;
            }
            assert!(metadata.is_file(), "Nonregular SDK entry refused");
            if path == marker {
                continue;
            }
            let relative = path
                .strip_prefix(&sdk)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let hash = expected
                .get(&relative)
                .and_then(serde_json::Value::as_str)
                .expect("Unverified native SDK file");
            println!("cargo:rerun-if-changed={}", path.display());
            assert_eq!(
                digest(&path),
                hash,
                "Native SDK integrity failure: {relative}"
            );
            seen += 1;
        }
    }
    assert_eq!(seen, expected.len(), "Incomplete native SDK");
    assert_eq!(
        manifest["library_sha256"].as_str(),
        expected["lib/webrtc.lib"].as_str()
    );
}

fn main() {
    emit_build_metadata();
    println!("cargo:rerun-if-changed=assets/relay.ico");
    println!("cargo:rerun-if-changed=assets/relay.rc");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        Ok("msvc"),
        "Relay's Windows package uses the MSVC toolchain"
    );
    verify_native_cryptor();
    let compiler = env::var_os("RC").map(PathBuf::from).unwrap_or_else(|| {
        let sdk = env::var_os("WindowsSdkDir").map(PathBuf::from).or_else(|| {
            env::var_os("ProgramFiles(x86)").map(|root| PathBuf::from(root).join("Windows Kits/10"))
        });
        let mut bins = sdk
            .and_then(|sdk| fs::read_dir(sdk.join("bin")).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64/rc.exe"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        bins.sort();
        bins.pop().unwrap_or_else(|| PathBuf::from("rc.exe"))
    });
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .join("relay-icon.res");
    let status = Command::new(compiler)
        .args(["/nologo", "/fo"])
        .arg(&output)
        .arg("assets/relay.rc")
        .status()
        .expect("Install the Windows SDK resource compiler, or set RC to its rc.exe path");
    assert!(status.success(), "Windows icon resource compilation failed");
    println!("cargo:rustc-link-arg-bin=relay={}", output.display());
}
