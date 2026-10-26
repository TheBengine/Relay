use super::*;

#[test]
fn failed_read_receipt_has_scoped_result_and_same_sequence_can_succeed_on_retry() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/api/read-state");
        let input: MarkRead = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(input.channel_id.as_deref(), Some("room"));
        assert_eq!(input.peer_id, None);
        assert_eq!(input.last_read_sequence, 42);
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(503, json!({"error":"temporary read failure"}))
        } else {
            Reply::json(
                200,
                json!({"channel_id":"room", "peer_id":null,
                "last_read_sequence":42,"unread_count":0,"mention_count":0}),
            )
        }
    });
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    let origin = test.command_origin();
    let target = Target::Channel("room".into());
    for succeeds in [false, true] {
        test.worker.command(Command::MarkRead {
            origin: origin.clone(),
            target: target.clone(),
            last_read_sequence: 42,
        });
        let updates = test.drain();
        assert_eq!(
            updates.len(),
            1,
            "receipts must not publish global busy/errors"
        );
        match &updates[0] {
            Update::ReadMarked {
                origin: actual,
                target: actual_target,
                requested_sequence,
                result,
            } => {
                assert_eq!(actual, &origin);
                assert_eq!(actual_target, &target);
                assert_eq!(*requested_sequence, 42);
                assert_eq!(result.is_ok(), succeeds);
                if let Ok(state) = result {
                    assert_eq!(state.last_read_sequence, 42);
                }
            }
            _ => panic!("missing scoped read result"),
        }
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn read_receipts_reject_account_switches_before_http_and_mismatched_response_targets() {
    let server = MockHttp::new(|_| {
        Reply::json(
            200,
            json!({
                "channel_id":"another-room","peer_id":null,"last_read_sequence":42,
                "unread_count":0,"mention_count":0
            }),
        )
    });
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    let origin = test.command_origin();
    test.worker.connection.as_mut().unwrap().session.user.id = "new-account".into();
    test.worker.command(Command::MarkRead {
        origin: origin.clone(),
        target: Target::Channel("room".into()),
        last_read_sequence: 42,
    });
    assert!(
        matches!(&test.drain()[0], Update::ReadMarked {result:Err(error), ..}
        if error.contains("another account or community"))
    );
    assert!(server.requests.lock().unwrap().is_empty());

    test.connection(server.url.clone());
    test.worker.command(Command::MarkRead {
        origin,
        target: Target::Channel("room".into()),
        last_read_sequence: u64::MAX,
    });
    assert!(
        matches!(&test.drain()[0], Update::ReadMarked {result:Err(error), ..}
        if error.contains("another conversation"))
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
