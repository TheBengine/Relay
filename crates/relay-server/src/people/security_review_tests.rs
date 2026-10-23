use super::*;
use futures_util::StreamExt;
use tokio_tungstenite::{
    connect_async, tungstenite::client::IntoClientRequest, MaybeTlsStream, WebSocketStream,
};

type Watch = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn watch(address: SocketAddr, session: &Session) -> Watch {
    let mut request = format!("ws://{address}/api/events")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", session.token).parse().unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    // The initial heartbeat proves the server subscribed before we mutate state.
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(message) = socket.next().await {
            if message.unwrap().is_ping() {
                return;
            }
        }
        panic!("owned event connection closed before its first heartbeat");
    })
    .await
    .unwrap();
    socket
}

async fn presence_notice(socket: &mut Watch, user: &str) -> Option<Value> {
    tokio::time::timeout(Duration::from_millis(250), async {
        while let Some(message) = socket.next().await {
            let message = message.unwrap();
            if let Ok(text) = message.to_text() {
                if let Ok(event) = serde_json::from_str::<Value>(text) {
                    if event["user_id"] == user {
                        return Some(event);
                    }
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn pending_friend_cannot_monitor_presence_events_but_accepted_friend_can() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Owned presence QA", 0, config()).unwrap();
    let pending = account(&state, "pending");
    let subject = account(&state, "subject");
    let accepted = account(&state, "accepted");
    let outsider = account(&state, "outsider");
    let app = router(state);
    for (sender, recipient) in [
        (&pending, &subject),
        (&accepted, &subject),
        (&subject, &accepted),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("/api/friends/{}", recipient.user.id),
                Some(&sender.token),
                json!({}),
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let served = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    let mut pending_events = watch(address, &pending).await;
    let mut friend_events = watch(address, &accepted).await;
    let mut outsider_events = watch(address, &outsider).await;
    let mut self_events = watch(address, &subject).await;
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/presence",
            Some(&subject.token),
            json!({"user_id":subject.user.id,"status":"away","text":"owned QA","expires_at":null}),
        )
        .await
        .0,
        StatusCode::OK
    );
    let leaked = presence_notice(&mut pending_events, &subject.user.id).await;
    let expected = presence_notice(&mut friend_events, &subject.user.id).await;
    let unrelated = presence_notice(&mut outsider_events, &subject.user.id).await;
    let own = presence_notice(&mut self_events, &subject.user.id).await;
    drop((pending_events, friend_events, outsider_events, self_events));
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        expected.is_some(),
        "accepted friend must receive status updates"
    );
    assert!(
        own.is_some(),
        "other sessions of the same account retain updates"
    );
    assert!(
        unrelated.is_none(),
        "outsider must not receive status updates"
    );
    assert!(
        leaked.is_none(),
        "unaccepted friend request revealed a presence event: {leaked:?}"
    );
}
