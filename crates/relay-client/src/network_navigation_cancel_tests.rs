use super::*;

#[test]
fn cancelling_late_navigation_parks_without_logout_or_forgetting_saved_access() {
    let server = MockHttp::new(|_| panic!("Parking navigation must not make an HTTP request"));
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    let connection = test.worker.connection.as_mut().unwrap();
    connection.voice_channel = None;
    connection.server_id = "4be44aef-8c98-4db6-a9d5-81c2eb311330".into();
    connection.remembered = true;
    let origin = test.command_origin();
    #[cfg(windows)]
    {
        crate::credential_vault::save(
            &server.url,
            &origin.server_id,
            &test.worker.connection.as_ref().unwrap().session,
        )
        .unwrap();
    }
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = crate::credential_vault::forget(&self.0);
        }
    }
    let _cleanup = Cleanup(server.url.clone());
    test.worker
        .handle(Command::CancelNavigation {
            cancelled: origin,
            restore: None,
        })
        .unwrap();
    assert!(test.worker.connection.is_none());
    let parked = &test.worker.saved_connections[&server.url].connection;
    assert!(parked.remembered);
    assert_eq!(parked.session.token, "fixture-token");
    #[cfg(windows)]
    assert_eq!(
        crate::credential_vault::load(&server.url)
            .unwrap()
            .unwrap()
            .session
            .token,
        "fixture-token"
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn late_navigation_cannot_park_a_replacement_account_or_a_current_call() {
    for changed in ["user", "server", "call"] {
        let mut test = TestWorker::new();
        test.connection("https://owned.invalid".into());
        test.worker.connection.as_mut().unwrap().voice_channel = None;
        let origin = test.command_origin();
        match changed {
            "user" => {
                test.worker.connection.as_mut().unwrap().session.user.id = "replacement-user".into()
            }
            "server" => {
                test.worker.connection.as_mut().unwrap().server_id = "replacement-server".into()
            }
            _ => {
                test.worker.connection.as_mut().unwrap().voice_channel = Some("current-call".into())
            }
        }
        let before = test.command_origin();
        let epoch = test.worker.live.epoch.load(Ordering::SeqCst);
        test.worker
            .handle(Command::CancelNavigation {
                cancelled: origin,
                restore: None,
            })
            .unwrap();
        assert_eq!(test.command_origin(), before);
        assert_eq!(test.worker.live.epoch.load(Ordering::SeqCst), epoch);
        assert!(test.worker.saved_connections.is_empty());
    }
}

#[test]
fn cancelling_late_friends_navigation_restores_the_requested_parked_community() {
    let late = MockHttp::new(|_| panic!("Cancelled Friends must not be logged out"));
    let desired = MockHttp::new(|path| {
        assert_eq!(path, "/api/bootstrap");
        Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                server_id: "owned-destination".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
                protocol_version: API_PROTOCOL_VERSION,
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut test = TestWorker::new();
    test.connection(desired.url.clone());
    let mut desired_connection = test.worker.connection.take().unwrap();
    desired_connection.voice_channel = None;
    desired_connection.server_id = "owned-destination".into();
    test.worker.saved_connections.insert(
        desired.url.clone(),
        SavedConnection {
            connection: desired_connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    test.connection(late.url.clone());
    test.worker.connection.as_mut().unwrap().voice_channel = None;
    let origin = test.command_origin();
    test.worker
        .handle(Command::CancelNavigation {
            cancelled: origin,
            restore: Some(desired.url.clone()),
        })
        .unwrap();
    assert_eq!(test.worker.connection.as_ref().unwrap().url, desired.url);
    assert!(test.worker.saved_connections.contains_key(&late.url));
    assert!(late.requests.lock().unwrap().is_empty());
    assert!(test
        .drain()
        .iter()
        .any(|update| matches!(update, Update::Connected { url, .. } if url == &desired.url)));
}

#[test]
fn failed_navigation_restore_marks_previous_ui_destination_reconnecting_without_logout() {
    let late = MockHttp::new(|_| panic!("Cancelled navigation must not log out"));
    let desired = MockHttp::new(|path| {
        assert_eq!(path, "/api/bootstrap");
        Reply::json(503, json!({"error":"owned destination offline"}))
    });
    let mut test = TestWorker::new();
    test.connection(desired.url.clone());
    let mut parked = test.worker.connection.take().unwrap();
    parked.voice_channel = None;
    test.worker.saved_connections.insert(
        desired.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    test.connection(late.url.clone());
    test.worker.connection.as_mut().unwrap().voice_channel = None;
    let origin = test.command_origin();
    assert!(test
        .worker
        .handle(Command::CancelNavigation {
            cancelled: origin,
            restore: Some(desired.url.clone())
        })
        .is_err());
    assert!(test.worker.connection.is_none());
    assert!(
        test.worker.saved_connections.contains_key(&desired.url)
            && test.worker.saved_connections.contains_key(&late.url)
    );
    assert!(late.requests.lock().unwrap().is_empty());
    assert!(test.drain().iter().any(|update| matches!(update,
        Update::ConnectionState { url, status: ConnectionStatus::Reconnecting } if url == &desired.url)));
}
