use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use serde_json::{json, Value};

struct Fixture {
    _data: tempfile::TempDir,
    state: AppState,
    app: Router,
    caller: Session,
    recipient: Session,
    call: PrivateCall,
    other_room: String,
}

impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "private-call-busy", 0).unwrap();
        let app = router(state.clone());
        let caller = signup(&app, "Caller", "").await;
        let recipient = signup(&app, "Recipient", &new_invite(&app, &caller).await).await;
        for (session, peer) in [(&caller, &recipient), (&recipient, &caller)] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &format!("/api/friends/{}", peer.user.id),
                    Some(&session.token),
                    Value::Null,
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        let other_room = create_test_channel(&app, &caller, "voice", false).await;
        let (status, value) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&caller.token),
            json!({"peer_id":recipient.user.id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        Self {
            _data: data,
            state,
            app,
            caller,
            recipient,
            call: serde_json::from_value(value).unwrap(),
            other_room,
        }
    }

    async fn join(&self, session: &Session, channel: &str) -> StatusCode {
        request(
            &self.app,
            "POST",
            "/api/voice/join",
            Some(&session.token),
            json!({"channel_id":channel}),
        )
        .await
        .0
    }

    async fn accept(&self) -> (StatusCode, Value) {
        request(
            &self.app,
            "POST",
            &format!("/api/private-calls/{}", self.call.id),
            Some(&self.recipient.token),
            json!({"action":"accept"}),
        )
        .await
    }

    async fn visible_call(&self, session: &Session) -> Value {
        let (status, social) = request(
            &self.app,
            "GET",
            "/api/social",
            Some(&session.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        social["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|call| call["id"] == self.call.id)
            .unwrap()
            .clone()
    }
}

#[tokio::test]
async fn caller_joining_another_room_cancels_outgoing_ring_for_both_peers() {
    let f = Fixture::new().await;
    assert_eq!(f.join(&f.caller, &f.other_room).await, StatusCode::OK);
    for session in [&f.caller, &f.recipient] {
        assert_eq!(f.visible_call(session).await["state"], "cancelled");
    }
    let (status, error) = f.accept().await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error.to_string().contains("Leave your current call"));
}

#[tokio::test]
async fn recipient_busy_error_does_not_cancel_an_incoming_ring() {
    let f = Fixture::new().await;
    assert_eq!(f.join(&f.recipient, &f.other_room).await, StatusCode::OK);
    let (status, error) = f.accept().await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("Leave your current call"));
    assert_eq!(f.visible_call(&f.caller).await["state"], "ringing");
}

#[tokio::test]
async fn acceptance_during_join_notification_gap_cancels_with_caller_specific_error() {
    let f = Fixture::new().await;
    assert_eq!(f.join(&f.caller, &f.other_room).await, StatusCode::OK);
    // Reproduce the state between committed admission and the post-commit
    // notification helper, without adding a production scheduling hook.
    lock(&f.state.0.private_calls).insert(f.call.id.clone(), f.call.clone());
    let (status, error) = f.accept().await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("The caller joined another call"));
    for session in [&f.caller, &f.recipient] {
        assert_eq!(f.visible_call(session).await["state"], "cancelled");
    }
}

#[tokio::test]
async fn admission_cancellation_ignores_failed_join_same_room_and_accepted_calls() {
    let f = Fixture::new().await;
    assert_eq!(
        f.join(&f.caller, "missing-voice-room").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.visible_call(&f.recipient).await["state"], "ringing");
    assert_eq!(f.join(&f.caller, &f.call.channel_id).await, StatusCode::OK);
    assert_eq!(f.visible_call(&f.recipient).await["state"], "ringing");
    {
        let mut calls = lock(&f.state.0.private_calls);
        calls.get_mut(&f.call.id).unwrap().state = "accepted".into();
        let mut incoming = f.call.clone();
        incoming.id = "incoming-fixture".into();
        std::mem::swap(&mut incoming.caller_id, &mut incoming.recipient_id);
        calls.insert(incoming.id.clone(), incoming);
    }
    // Exercise the B3 notification helper in isolation: durable accepted call
    // teardown on a real room replacement is covered by recovery tests.
    joined_other_room(&f.state, &f.caller.user.id, &f.other_room);
    let calls = lock(&f.state.0.private_calls);
    assert_eq!(calls[&f.call.id].state, "accepted");
    assert_eq!(calls["incoming-fixture"].state, "ringing");
}
