use super::*;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
enum Response {
    Bytes(Vec<u8>),
    Redirect(String),
    OversizedChunks,
    Hold,
}

struct Server {
    base: String,
    certificate: Vec<u8>,
    objects: Arc<Mutex<BTreeMap<String, Response>>>,
    requested: Arc<Mutex<Vec<String>>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}
impl Server {
    async fn new(fixture: &Fixture) -> Self {
        initialize_tls_provider();
        let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let certificate = identity.cert.der().to_vec();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    identity.signing_key.serialize_der(),
                )),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("https://{}/", listener.local_addr().unwrap());
        let objects = Arc::new(Mutex::new(
            fixture
                .memory
                .objects
                .lock()
                .unwrap()
                .iter()
                .map(|(p, b)| (p.clone(), Response::Bytes(b.clone())))
                .collect::<BTreeMap<_, _>>(),
        ));
        let requested = Arc::new(Mutex::new(Vec::new()));
        let (stop, mut stopping) = tokio::sync::oneshot::channel();
        let shared = objects.clone();
        let requests = requested.clone();
        let handle = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _=&mut stopping=>break,
                    connection=listener.accept()=>{
                        let Ok((socket,_))=connection else {break;};
                        let tls=acceptor.clone();let data=shared.clone();let log=requests.clone();
                        connections.spawn(async move {
                            let Ok(mut socket)=tls.accept(socket).await else{return;};
                            let mut request=Vec::new();let mut chunk=[0u8;1024];
                            while !request.windows(4).any(|s|s==b"\r\n\r\n") {
                                let Ok(count)=socket.read(&mut chunk).await else{return;};
                                if count==0||request.len()+count>8192{return;}
                                request.extend_from_slice(&chunk[..count]);
                            }
                            let request=String::from_utf8_lossy(&request);
                            let path=request.lines().next().and_then(|s|s.split_whitespace().nth(1)).unwrap_or("").to_string();
                            log.lock().unwrap().push(path.clone());
                            if path=="/socket" {
                                let key=request.lines().filter_map(|l|l.split_once(':')).find(|(k,_)|k.eq_ignore_ascii_case("sec-websocket-key")).map(|(_,v)|v.trim());
                                let Some(key)=key else{return;};
                                let accept=tungstenite::handshake::derive_accept_key(key.as_bytes());
                                let headers=format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
                                if socket.write_all(headers.as_bytes()).await.is_err(){return;}
                                let mut ws=tokio_tungstenite::WebSocketStream::from_raw_socket(socket,tokio_tungstenite::tungstenite::protocol::Role::Server,None).await;
                                if let Some(Ok(message))=ws.next().await {let _=ws.send(message).await;}
                                let _=ws.close(None).await;return;
                            }
                            let response=data.lock().unwrap().get(&path).cloned();
                            match response {
                                Some(Response::Bytes(bytes))=>{
                                    let headers=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len());
                                    if socket.write_all(headers.as_bytes()).await.is_ok(){let _=socket.write_all(&bytes).await;}
                                },
                                Some(Response::Redirect(location))=>{let _=socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;},
                                Some(Response::OversizedChunks)=>{
                                    if socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.is_err(){return;}
                                    let bytes=vec![b' ';64*1024];
                                    for _ in 0..34 {
                                        if socket.write_all(b"10000\r\n").await.is_err()||socket.write_all(&bytes).await.is_err()||socket.write_all(b"\r\n").await.is_err(){return;}
                                    }
                                    let _=socket.write_all(b"0\r\n\r\n").await;
                                },
                                Some(Response::Hold)=>std::future::pending::<()>().await,
                                None=>{let _=socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;},
                            }
                        });
                    },
                    _=connections.join_next(),if !connections.is_empty()=>{},
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            base,
            certificate,
            objects,
            requested,
            stop: Some(stop),
            handle: Some(handle),
        }
    }
    fn publisher(&self, fixture: &Fixture) -> Publisher {
        Publisher::from_bundled_root(
            fixture.root.clone(),
            &(self.base.clone() + "metadata/"),
            &(self.base.clone() + "targets/"),
            "stable",
        )
        .unwrap()
    }
    fn transport(&self, fixture: &Fixture) -> HttpTransport {
        HttpTransport::with_fixture_certificate(&self.publisher(fixture), &self.certificate)
            .unwrap()
    }
    fn engine(&self, fixture: &Fixture, dir: &Path) -> Engine {
        Engine::new(
            self.publisher(fixture),
            InstalledRelease {
                version: "0.10.0".into(),
                sequence: "20261009034055".into(),
            },
            dir.into(),
        )
        .unwrap()
    }
    async fn close(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.handle.take().unwrap().await.unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[tokio::test]
async fn signed_repository_and_payload_pass_over_actual_tls_and_reject_changed_payload() {
    let fixture = Fixture::new();
    let server = Server::new(&fixture).await;
    let dir = directory();
    let engine = server.engine(&fixture, dir.path());
    assert_eq!(
        engine.check_with(server.transport(&fixture)).await.unwrap(),
        Check::Available(Box::new(fixture.release.clone()))
    );
    let ready = engine
        .download_with(&fixture.release, server.transport(&fixture))
        .await
        .unwrap();
    ready.revalidate().unwrap();
    assert_eq!(std::fs::read(ready.path()).unwrap(), fixture.installer);
    let second_dir = directory();
    let engine = server.engine(&fixture, second_dir.path());
    server.objects.lock().unwrap().insert(
        format!("/targets/{}", fixture.release.installer_target),
        Response::Bytes(vec![b'X'; fixture.installer.len()]),
    );
    assert!(engine
        .download_with(&fixture.release, server.transport(&fixture))
        .await
        .is_err());
    assert_eq!(
        std::fs::read_dir(second_dir.path().join("downloads"))
            .unwrap()
            .count(),
        0
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(second_dir.path().join("head.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved["highest"]["installer_sha256"],
        fixture.release.installer_sha256
    );
    // The valid signed metadata persists; altered payload never becomes an installer.
    assert_eq!(generations(second_dir.path()), 1);
    server.close().await;
}

#[tokio::test]
async fn https_redirects_and_chunked_overflow_are_refused_without_following_redirect() {
    let fixture = Fixture::new();
    let server = Server::new(&fixture).await;
    let path = "/metadata/timestamp.json";
    let url = url::Url::parse(&(server.base.clone() + "metadata/timestamp.json")).unwrap();
    server.objects.lock().unwrap().insert(
        path.into(),
        Response::Redirect(server.base.clone() + "targets/redirect-destination"),
    );
    let transport = server.transport(&fixture);
    assert_eq!(
        transport.fetch(url.clone()).await.err().unwrap().kind(),
        TransportErrorKind::Other
    );
    assert_eq!(*server.requested.lock().unwrap(), vec![path]);
    server
        .objects
        .lock()
        .unwrap()
        .insert(path.into(), Response::OversizedChunks);
    let mut stream = transport.fetch(url).await.unwrap();
    let mut count = 0;
    let error = loop {
        match stream.try_next().await {
            Ok(Some(b)) => count += b.len(),
            Ok(None) => panic!("Oversized chunked response accepted"),
            Err(e) => break e,
        }
    };
    assert!(count as u64 <= MAX_METADATA);
    assert_eq!(error.kind(), TransportErrorKind::Other);
    server.close().await;
}

#[tokio::test]
async fn untrusted_tls_certificate_is_not_accepted_by_production_transport() {
    let fixture = Fixture::new();
    let server = Server::new(&fixture).await;
    let production = HttpTransport::new(&server.publisher(&fixture)).unwrap();
    let url = url::Url::parse(&(server.base.clone() + "metadata/timestamp.json")).unwrap();
    assert!(production.fetch(url).await.is_err());
    assert!(server.requested.lock().unwrap().is_empty());
    server.close().await;
}

#[tokio::test]
async fn cancelling_actual_https_metadata_wait_preserves_checkpoint_and_releases_lock() {
    let fixture = Fixture::new();
    let server = Server::new(&fixture).await;
    let dir = directory();
    let engine = Arc::new(server.engine(&fixture, dir.path()));
    engine.check_with(server.transport(&fixture)).await.unwrap();
    let head = std::fs::read(dir.path().join("head.json")).unwrap();
    server.requested.lock().unwrap().clear();
    server
        .objects
        .lock()
        .unwrap()
        .insert("/metadata/timestamp.json".into(), Response::Hold);
    let mut worker = Worker::new(server.engine(&fixture, dir.path()));
    let operation = engine.clone();
    let transport = server.transport(&fixture);
    worker
        .start(
            move || async move { operation.check_with(transport).await.map(Outcome::Checked) },
            || {},
        )
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !server
        .requested
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == "/metadata/timestamp.json")
    {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    worker.cancel();
    loop {
        match worker.poll() {
            Poll::Finished(result) => {
                assert!(matches!(result.unwrap(), Outcome::Cancelled));
                break;
            }
            Poll::Running | Poll::Completing => {}
            Poll::Idle => panic!("Lost operation"),
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(!worker.running());
    assert_eq!(std::fs::read(dir.path().join("head.json")).unwrap(), head);
    assert_eq!(generations(dir.path()), 1);
    drop(Store::open(dir.path(), &server.publisher(&fixture).identity).unwrap());
    server.close().await;
}

#[tokio::test]
async fn explicit_provider_supports_real_https_and_websocket_tls_with_both_features_enabled() {
    let fixture = Fixture::new();
    let server = Server::new(&fixture).await;
    let certificate = server.certificate.clone();
    let address = server.base.replacen("https://", "wss://", 1) + "socket";
    let client = tokio::task::spawn_blocking(move || {
        initialize_tls_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(certificate)).unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let url = url::Url::parse(&address).unwrap();
        let destination: std::net::SocketAddr =
            format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
                .parse()
                .unwrap();
        let stream =
            std::net::TcpStream::connect_timeout(&destination, Duration::from_secs(3)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (mut socket, _) = tungstenite::client_tls_with_config(
            address,
            stream,
            None,
            Some(tungstenite::Connector::Rustls(Arc::new(config))),
        )
        .unwrap();
        socket
            .send(tungstenite::Message::Text(
                "owned TLS websocket fixture".into(),
            ))
            .unwrap();
        assert_eq!(
            socket.read().unwrap().into_text().unwrap(),
            "owned TLS websocket fixture"
        );
        let _ = socket.close(None);
    });
    tokio::time::timeout(Duration::from_secs(8), client)
        .await
        .unwrap()
        .unwrap();
    assert!(server
        .requested
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == "/socket"));
    server.close().await;
}
