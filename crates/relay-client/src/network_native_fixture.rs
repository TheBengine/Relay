//! Explicit owned-loopback native-media fixture. No physical devices or MLS actor.
use super::*;

pub(crate) fn open_context(
    server: &str,
    session: Session,
    channel: &str,
    revision: u64,
) -> Result<(Arc<crate::e2ee::CallKeys>, RtcTicket), String> {
    let address = url::Url::parse(server).map_err(|_| "Invalid owned server URL")?;
    if address.scheme() != "http"
        || address.host_str() != Some("127.0.0.1")
        || address.port().is_none()
        || !address.username().is_empty()
        || address.password().is_some()
        || address.query().is_some()
        || address.fragment().is_some()
        || address.path() != "/"
    {
        return Err("Native acceptance uses an explicit owned loopback server only".into());
    }
    let server = server.trim_end_matches('/');
    let (tx, _updates) = mpsc::sync_channel(256);
    let mut worker = Worker::new(
        tx,
        eframe::egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .map_err(|_| "Owned worker HTTP client failed")?;
    let bootstrap: Bootstrap = parse(
        worker
            .http
            .get(format!("{server}/api/bootstrap"))
            .bearer_auth(&session.token)
            .send()
            .map_err(|_| "Owned server bootstrap failed")?,
    )?;
    if bootstrap.user.id != session.user.id
        || bootstrap.protection.level != ProtectionLevel::Open
        || bootstrap.protection.revision != revision
    {
        return Err("Owned Open fixture account or revision changed".into());
    }
    let mut chat = extended::ChatSession::new(&bootstrap);
    // Only this explicitly opted-in synthetic fixture grants local acceptance.
    chat.set_open_consent(Some(revision));
    worker.connection = Some(Connection {
        url: server.into(),
        session,
        current: None,
        voice_channel: None,
        voice_attempt: None,
        voice_admission: None,
        voice_recovery: None,
        frame_ids: HashMap::new(),
        sharing: false,
        camera: false,
        server_id: bootstrap.server_id,
        name: bootstrap.server_name,
        change_cursor: bootstrap.latest_change_sequence,
        protocol_version: bootstrap.protocol_version,
        remembered: false,
        chat,
        crypto: None,
        crypto_registered: false,
        crypto_call_epoch: None,
        open_call_context: None,
        crypto_pending_join: None,
    });
    // Admission was performed against the real server by the owned orchestrator.
    // Use production key validation/conversion and the real server's RTC grant.
    let keys = worker.protected_call_keys(channel)?;
    let ticket = worker.post(
        "/api/rtc/token",
        &VoiceJoin {
            channel_id: channel.into(),
            device_id: Some(keys.local_device_id.clone()),
            open_revision: Some(revision),
        },
    )?;
    assert!(worker.connection.as_ref().unwrap().crypto.is_none());
    Ok((keys, ticket))
}
