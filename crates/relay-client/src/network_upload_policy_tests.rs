use super::*;
use sha2::{Digest, Sha256};

fn open_uploads(test: &mut TestWorker, limit: u64) {
    let bootstrap = Bootstrap {
        protection: CommunityProtection {
            level: ProtectionLevel::Open,
            revision: 1,
            ..Default::default()
        },
        upload_limits: UploadLimits {
            max_file_bytes: MAX_SUPPORTED_UPLOAD_BYTES as u64,
            effective_file_bytes: limit,
            pending_bytes: MAX_PENDING_UPLOAD_BYTES,
        },
        ..Default::default()
    };
    let connection = test.worker.connection.as_mut().unwrap();
    connection.chat.update_bootstrap(&bootstrap);
    connection.chat.set_open_consent(Some(1));
}
fn wait_upload(test: &mut TestWorker) -> Result<Attachment, String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        test.worker.poll_transfers();
        for update in test.drain() {
            match update {
                Update::UploadFinished { attachment, .. } => return Ok(attachment),
                Update::UploadFailed { error, .. } => return Err(error),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "Owned upload timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn reviewed_upload_rejects_same_size_file_replacement_before_any_file_post() {
    let server = MockHttp::new(|_| panic!("Changed reviewed bytes must not be uploaded"));
    let path = test_work_root().join(format!("reviewed-change-{}", operation_id()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"replaced").unwrap();
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    open_uploads(&mut test, MAX_SUPPORTED_UPLOAD_BYTES as u64);
    test.worker.command(Command::StartReviewedUpload {
        origin: test.command_origin(),
        target: Target::Channel("general".into()),
        path: path.clone(),
        operation_id: operation_id(),
        review: UploadReview {
            bytes: 8,
            sha256: Sha256::digest(b"original").into(),
        },
    });
    assert!(wait_upload(&mut test)
        .unwrap_err()
        .contains("changed after review"));
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(fs::read(&path).unwrap(), b"replaced");
    fs::remove_file(path).unwrap();
}

#[test]
fn reviewed_upload_above_legacy_limit_uses_advertised_allowance_and_original_bytes() {
    let size = MAX_UPLOAD_BYTES + 1024;
    let server = MockHttp::new(move |path| {
        assert!(path.starts_with("/api/uploads?"));
        Reply::json(
            200,
            json!({"id":"large-reviewed-file","name":"opaque","mime":"application/octet-stream","size":size,"url":"/api/uploads/large-reviewed-file"}),
        )
    });
    let path = test_work_root().join(format!("reviewed-large-{}", operation_id()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = vec![7; size];
    let digest = Sha256::digest(&bytes).into();
    fs::write(&path, &bytes).unwrap();
    drop(bytes);
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    open_uploads(&mut test, MAX_SUPPORTED_UPLOAD_BYTES as u64);
    test.worker.command(Command::StartReviewedUpload {
        origin: test.command_origin(),
        target: Target::Channel("general".into()),
        path: path.clone(),
        operation_id: operation_id(),
        review: UploadReview {
            bytes: size as u64,
            sha256: digest,
        },
    });
    assert_eq!(wait_upload(&mut test).unwrap().size, size as u64);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body.len(), size);
    assert!(requests[0].body.iter().all(|byte| *byte == 7));
    drop(requests);
    fs::remove_file(path).unwrap();
}

#[test]
fn reduced_or_disabled_upload_allowance_rejects_before_reading_or_posting_file() {
    let server = MockHttp::new(|_| panic!("Policy-rejected file must not be posted"));
    let path = test_work_root().join(format!("reviewed-policy-{}", operation_id()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"reviewed").unwrap();
    for limit in [0, 7] {
        let mut test = TestWorker::new();
        test.connection(server.url.clone());
        open_uploads(&mut test, limit);
        test.worker.command(Command::StartReviewedUpload {
            origin: test.command_origin(),
            target: Target::Channel("general".into()),
            path: path.clone(),
            operation_id: operation_id(),
            review: UploadReview {
                bytes: 8,
                sha256: Sha256::digest(b"reviewed").into(),
            },
        });
        assert!(wait_upload(&mut test).is_err());
    }
    assert!(server.requests.lock().unwrap().is_empty());
    fs::remove_file(path).unwrap();
}

#[test]
fn previously_uploaded_file_above_ten_mib_downloads_after_owner_disables_new_uploads() {
    let size = MAX_UPLOAD_BYTES + 1024;
    let server = MockHttp::new(move |path| {
        assert_eq!(path, "/api/uploads/historical-file");
        Reply {
            status: 200,
            body: vec![3; size],
            headers: vec![],
        }
    });
    let path = test_work_root().join(format!("historical-download-{}", operation_id()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    open_uploads(&mut test, 0);
    test.worker.command(Command::StartDownload {
        origin: test.command_origin(),
        attachment: Attachment {
            id: "historical-file".into(),
            name: "historical.bin".into(),
            mime: "application/octet-stream".into(),
            size: size as u64,
            url: "/api/uploads/historical-file".into(),
        },
        path: path.clone(),
        operation_id: operation_id(),
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut complete = false;
    while !complete {
        test.worker.poll_transfers();
        for update in test.drain() {
            match update {
                Update::DownloadFinished { .. } => complete = true,
                Update::DownloadFailed { error, .. } => {
                    panic!("Owned historical download failed: {error}")
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), size as u64);
    fs::remove_file(path).unwrap();
}

#[test]
fn upload_plaintext_allowance_subtracts_encryption_and_clamps_untrusted_limits() {
    let limits = UploadLimits {
        max_file_bytes: u64::MAX,
        effective_file_bytes: u64::MAX,
        pending_bytes: u64::MAX,
    };
    assert_eq!(
        upload_plain_limit(&limits, true),
        MAX_SUPPORTED_UPLOAD_BYTES as u64
    );
    assert_eq!(
        upload_plain_limit(&limits, false),
        (MAX_SUPPORTED_UPLOAD_BYTES - relay_crypto::files::FILE_OVERHEAD) as u64
    );
    assert_eq!(
        upload_plain_limit(
            &UploadLimits {
                effective_file_bytes: 0,
                ..Default::default()
            },
            false
        ),
        0
    );
}
