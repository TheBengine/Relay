//! GIPHY requests go directly from the client. Only favorite IDs are persisted.
//! Media and provider URLs live only while their current view is open.
use crate::chat_media::{self, DecodedMedia};
use eframe::egui;
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    io::Read,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

#[derive(Clone)]
pub struct Gif {
    pub id: String,
    pub title: String,
    pub still: String,
    pub animated: String,
    pub preview: String,
}

impl Gif {
    pub fn link(&self) -> String {
        format!("https://giphy.com/gifs/{}", self.id)
    }
}

pub struct Page {
    pub gifs: Vec<Gif>,
    pub more: bool,
}

pub enum Request {
    Browse {
        generation: u64,
        query: String,
        offset: usize,
        favorites: Option<Vec<String>>,
    },
    Preview {
        generation: u64,
        id: String,
        url: String,
    },
    Hover {
        generation: u64,
        id: String,
        url: String,
    },
    Message {
        id: String,
    },
}

pub enum ResultItem {
    Browse(u64, Result<Page, String>),
    Preview(u64, String, Result<DecodedMedia, String>),
    Hover(u64, String, Result<DecodedMedia, String>),
    Message(String, Result<DecodedMedia, String>),
}

pub struct Loader {
    latest: Arc<AtomicU64>,
    browse: mpsc::SyncSender<Request>,
    media: mpsc::SyncSender<Request>,
    pub rx: mpsc::Receiver<ResultItem>,
    #[cfg(test)]
    pub(super) worker_exits: mpsc::Receiver<()>,
}

pub fn configured_key(local: &str) -> String {
    choose_key(
        local,
        std::env::var("RELAY_GIPHY_API_KEY").ok().as_deref(),
        option_env!("RELAY_GIPHY_API_KEY"),
    )
}

fn choose_key(local: &str, runtime: Option<&str>, bundled: Option<&str>) -> String {
    [Some(local), runtime, bundled]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|key| !key.is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[derive(Default, PartialEq, Eq, Debug)]
pub enum ConnectionState {
    #[default]
    Idle,
    Checking,
    Ready,
    Failed(String),
}

/// Each test owns its receiver. Changing the effective key drops that receiver,
/// so a late response can never validate a replacement key, even after A/B/A.
#[derive(Default)]
pub struct ConnectionCheck {
    key: String,
    pub state: ConnectionState,
    pending: Option<mpsc::Receiver<Result<(), String>>>,
}

impl ConnectionCheck {
    pub fn poll(&mut self, local: &str) {
        self.poll_key(configured_key(local));
    }

    fn poll_key(&mut self, key: String) {
        if self.key != key {
            self.key = key;
            self.pending = None;
            self.state = ConnectionState::Idle;
        }
        if let Some(pending) = &self.pending {
            let result = match pending.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("The connection test stopped. Try again.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.pending = None;
                self.state = match result {
                    Ok(()) => ConnectionState::Ready,
                    Err(error) => ConnectionState::Failed(error),
                };
            }
        }
    }

    pub fn start(&mut self, local: &str, ctx: &egui::Context) {
        self.poll(local);
        if self.key.is_empty() || self.pending.is_some() {
            return;
        }
        let key = self.key.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending = Some(rx);
        self.state = ConnectionState::Checking;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = client()
                .map_err(|_| "The connection test could not start.".to_owned())
                .and_then(|client| {
                    connection_at(&client, &key, "https://api.giphy.com/v1/gifs/trending")
                });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
}

fn client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn connection_at(client: &Client, key: &str, endpoint: &str) -> Result<(), String> {
    let response = client
        .get(endpoint)
        .query(&[("api_key", key), ("limit", "1"), ("rating", "pg-13")])
        .send()
        .map_err(|_| "Could not reach GIPHY. Check your connection and try again.".to_owned())?;
    let value = json_response(response)?;
    if value["meta"]["status"].as_u64() != Some(200) {
        return Err("GIPHY did not confirm the connection. Try again.".into());
    }
    let page = page(&value)?;
    if page.gifs.len() > 1 {
        return Err("GIPHY returned an unexpected connection response.".into());
    }
    Ok(())
}

impl Loader {
    pub fn new(key: String, ctx: egui::Context) -> Self {
        let latest = Arc::new(AtomicU64::new(0));
        let (browse, browse_rx) = mpsc::sync_channel(1);
        let (media, media_rx) = mpsc::sync_channel(2);
        let (results, rx) = mpsc::sync_channel(4);
        #[cfg(test)]
        let (exits, worker_exits) = mpsc::channel();
        for commands in [browse_rx, media_rx] {
            let results = results.clone();
            let key = key.clone();
            let ctx = ctx.clone();
            let latest = latest.clone();
            #[cfg(test)]
            let exits = exits.clone();
            std::thread::spawn(move || {
                let client = client();
                while let Ok(request) = commands.recv() {
                    if let Request::Browse { generation, .. }
                    | Request::Preview { generation, .. }
                    | Request::Hover { generation, .. } = &request
                    {
                        if *generation != latest.load(Ordering::Relaxed) {
                            continue;
                        }
                    }
                    let result = match &client {
                        Ok(client) => execute(client, &key, request),
                        Err(_) => match request {
                            Request::Browse { generation, .. } => ResultItem::Browse(
                                generation,
                                Err("Online GIFs could not start.".into()),
                            ),
                            Request::Preview { generation, id, .. } => ResultItem::Preview(
                                generation,
                                id,
                                Err("GIF preview could not start.".into()),
                            ),
                            Request::Hover { generation, id, .. } => ResultItem::Hover(
                                generation,
                                id,
                                Err("GIF preview could not start.".into()),
                            ),
                            Request::Message { id } => {
                                ResultItem::Message(id, Err("GIF preview could not start.".into()))
                            }
                        },
                    };
                    if results.send(result).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                #[cfg(test)]
                let _ = exits.send(());
            });
        }
        Self {
            latest,
            browse,
            media,
            rx,
            #[cfg(test)]
            worker_exits,
        }
    }

    pub fn request(&self, request: Request) -> bool {
        if let Request::Browse { generation, .. } = &request {
            self.latest.store(*generation, Ordering::Relaxed);
            self.browse.try_send(request).is_ok()
        } else {
            self.media.try_send(request).is_ok()
        }
    }

    pub fn invalidate(&self, generation: u64) {
        self.latest.store(generation, Ordering::Relaxed);
    }
}

fn execute(client: &Client, key: &str, request: Request) -> ResultItem {
    match request {
        Request::Browse {
            generation,
            query,
            offset,
            favorites,
        } => {
            let result = (|| {
                if let Some(ids) = favorites.as_ref() {
                    if ids.is_empty() {
                        return Ok(Page {
                            gifs: Vec::new(),
                            more: false,
                        });
                    }
                    if ids.iter().any(|id| !valid_id(id)) {
                        return Err("The saved GIF list is invalid.".into());
                    }
                }
                let endpoint = if favorites.is_some() {
                    "https://api.giphy.com/v1/gifs"
                } else if query.is_empty() {
                    "https://api.giphy.com/v1/gifs/trending"
                } else {
                    "https://api.giphy.com/v1/gifs/search"
                };
                let mut params = vec![
                    ("api_key", key.to_owned()),
                    ("limit", "20".into()),
                    ("offset", offset.to_string()),
                    ("rating", "pg-13".into()),
                ];
                if let Some(ids) = favorites {
                    params.push((
                        "ids",
                        ids.into_iter().take(50).collect::<Vec<_>>().join(","),
                    ));
                } else if !query.is_empty() {
                    params.push(("q", query));
                }
                let response = client
                    .get(endpoint)
                    .query(&params)
                    .send()
                    .map_err(|_| "Could not reach GIPHY. Try again.".to_owned())?;
                let value = json_response(response)?;
                page(&value)
            })();
            ResultItem::Browse(generation, result)
        }
        Request::Preview {
            generation,
            id,
            url,
        } => ResultItem::Preview(generation, id, media(client, &url, 2 * 1024 * 1024)),
        Request::Hover {
            generation,
            id,
            url,
        } => {
            let result = (|| {
                if !valid_media(&url) {
                    return Err("GIPHY returned an invalid preview address.".into());
                }
                let response = client
                    .get(url)
                    .send()
                    .map_err(|_| "Could not load this preview.".to_owned())?;
                chat_media::picker_preview(&bytes(response, 2 * 1024 * 1024)?)
            })();
            ResultItem::Hover(generation, id, result)
        }
        Request::Message { id } => {
            let result = (|| {
                if !valid_id(&id) {
                    return Err("Invalid GIF link.".into());
                }
                let response = client
                    .get(format!("https://api.giphy.com/v1/gifs/{id}"))
                    .query(&[("api_key", key)])
                    .send()
                    .map_err(|_| "Could not load this GIF from GIPHY.".to_owned())?;
                let value = json_response(response)?;
                let gif = gif(&value["data"])?;
                media(client, &gif.animated, chat_media::MAX_COMPRESSED_BYTES)
            })();
            ResultItem::Message(id, result)
        }
    }
}

fn bytes(mut response: reqwest::blocking::Response, max: usize) -> Result<Vec<u8>, String> {
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => "Online GIFs need a valid application key.",
            429 => "The GIPHY request limit has been reached. Try again later.",
            404 => "This GIF is no longer available.",
            _ => "GIPHY could not complete this request.",
        }
        .into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > max as u64)
    {
        return Err("GIF response exceeds the preview limit.".into());
    }
    let mut data = Vec::new();
    response
        .by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "The GIF download was interrupted.".to_owned())?;
    if data.len() > max {
        return Err("GIF response exceeds the preview limit.".into());
    }
    Ok(data)
}

fn json_response(response: reqwest::blocking::Response) -> Result<Value, String> {
    serde_json::from_slice(&bytes(response, 2 * 1024 * 1024)?)
        .map_err(|_| "GIPHY returned an unreadable response.".into())
}

fn media(client: &Client, url: &str, max: usize) -> Result<DecodedMedia, String> {
    if !valid_media(url) {
        return Err("GIPHY returned an invalid preview address.".into());
    }
    let response = client
        .get(url)
        .send()
        .map_err(|_| "Could not load the GIF preview.".to_owned())?;
    chat_media::inline_preview(&bytes(response, max)?)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|ch| ch.is_ascii_alphanumeric())
}

fn valid_media(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".giphy.com"))
    })
}

pub fn message_id(content: &str) -> Option<String> {
    let url = url::Url::parse(content.trim()).ok()?;
    if url.scheme() != "https"
        || url.host_str()? != "giphy.com"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let path = url.path().strip_prefix("/gifs/")?;
    if path.contains('/') {
        return None;
    }
    let id = path.rsplit('-').next()?;
    valid_id(id).then(|| id.to_owned())
}

fn gif(value: &Value) -> Result<Gif, String> {
    let id = value["id"]
        .as_str()
        .ok_or("GIPHY returned an incomplete GIF.")?;
    let still = value["images"]["fixed_width_small_still"]["url"]
        .as_str()
        .ok_or("GIPHY returned an incomplete preview.")?;
    let animated = value["images"]["fixed_width"]["url"]
        .as_str()
        .ok_or("GIPHY returned an incomplete animation.")?;
    let preview = value["images"]["fixed_width_small"]["url"]
        .as_str()
        .unwrap_or(animated);
    if !valid_id(id) || !valid_media(still) || !valid_media(animated) || !valid_media(preview) {
        return Err("GIPHY returned invalid GIF data.".into());
    }
    Ok(Gif {
        id: id.into(),
        title: value["title"]
            .as_str()
            .unwrap_or("GIF")
            .chars()
            .take(200)
            .collect(),
        still: still.into(),
        animated: animated.into(),
        preview: preview.into(),
    })
}

fn page(value: &Value) -> Result<Page, String> {
    let rows = value["data"]
        .as_array()
        .ok_or("GIPHY returned an incomplete result.")?;
    if rows.len() > 50 {
        return Err("GIPHY returned too many results.".into());
    }
    let gifs = rows.iter().map(gif).collect::<Result<Vec<_>, _>>()?;
    let offset = value["pagination"]["offset"].as_u64().unwrap_or(0);
    let total = value["pagination"]["total_count"].as_u64().unwrap_or(0);
    Ok(Page {
        more: offset + (gifs.len() as u64) < total,
        gifs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread::JoinHandle, time::Instant};

    // No production key or provider request: one owned local HTTP fixture.
    fn http_fixture(status: &str, body: String, delay: Duration) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/gifs/trending", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("owned HTTP fixture did not receive a request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() < 16 * 1024);
            }
            std::thread::sleep(delay);
            let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            // The timeout case intentionally closes the client before the reply.
            let _ = stream.write_all(reply.as_bytes());
            String::from_utf8(request).unwrap()
        });
        (endpoint, handle)
    }

    fn connected_response() -> String {
        serde_json::json!({
            "meta": {"status": 200},
            "data": [{"id":"test123", "images":{
                "fixed_width_small_still":{"url":"https://media.giphy.com/still.png?cid=keep"},
                "fixed_width":{"url":"https://media.giphy.com/animated.gif?rid=keep"}
            }}], "pagination":{"offset":0,"total_count":1}
        })
        .to_string()
    }

    #[test]
    fn empty_runtime_key_falls_through_to_bundled_and_local_override_wins() {
        assert_eq!(choose_key(" ", Some("\n\t"), Some(" bundled ")), "bundled");
        assert_eq!(
            choose_key(" local ", Some("runtime"), Some("bundled")),
            "local"
        );
        assert_eq!(
            choose_key("", Some(" runtime "), Some("bundled")),
            "runtime"
        );
        assert!(choose_key(" ", None, Some(" ")).is_empty());
    }

    #[test]
    fn connection_test_encodes_key_and_requests_only_one_gif() {
        let (endpoint, server) = http_fixture("200 OK", connected_response(), Duration::ZERO);
        assert!(connection_at(&client().unwrap(), "fixture&+key", &endpoint).is_ok());
        let request = server.join().unwrap();
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let parsed = url::Url::parse(&format!("http://fixture{target}")).unwrap();
        let params = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(params.get("api_key").unwrap(), "fixture&+key");
        assert_eq!(params.get("limit").unwrap(), "1");
        assert_eq!(params.get("rating").unwrap(), "pg-13");
        assert_eq!(
            params.len(),
            3,
            "no account ID, token or message content in the provider request"
        );
        assert!(!target.contains("fixture&+key"));
    }

    #[test]
    fn connection_test_reports_status_without_exposing_key_or_response_body() {
        for (status, expected) in [
            ("401 Unauthorized", "valid application key"),
            ("403 Forbidden", "valid application key"),
            ("429 Too Many Requests", "request limit"),
            ("500 Server Error", "could not complete"),
            ("302 Found", "could not complete"),
        ] {
            let (endpoint, server) = http_fixture(
                status,
                "secret-test-key and private server body".into(),
                Duration::ZERO,
            );
            let error =
                connection_at(&client().unwrap(), "secret-test-key", &endpoint).unwrap_err();
            server.join().unwrap();
            assert!(error.contains(expected));
            assert!(!error.contains("secret-test-key"));
            assert!(!error.contains("private server body"));
            assert!(!error.contains(&endpoint));
        }
    }

    #[test]
    fn connection_test_rejects_unconfirmed_or_malformed_success() {
        for body in [
            "not json".into(),
            "{\"meta\":{\"status\":403},\"data\":[]}".into(),
            "{\"meta\":{\"status\":200}}".into(),
            "{\"meta\":{\"status\":200},\"data\":[{\"id\":\"missing-images\"}]}".into(),
        ] {
            let (endpoint, server) = http_fixture("200 OK", body, Duration::ZERO);
            assert!(connection_at(&client().unwrap(), "fixture-key", &endpoint).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn connection_test_timeout_is_bounded_and_retryable() {
        let (endpoint, server) =
            http_fixture("200 OK", connected_response(), Duration::from_millis(350));
        let short = Client::builder()
            .timeout(Duration::from_millis(100))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        assert_eq!(
            connection_at(&short, "fixture-key", &endpoint).unwrap_err(),
            "Could not reach GIPHY. Check your connection and try again."
        );
        server.join().unwrap();
        let (endpoint, server) = http_fixture("200 OK", connected_response(), Duration::ZERO);
        assert!(connection_at(&short, "fixture-key", &endpoint).is_ok());
        server.join().unwrap();
    }

    #[test]
    fn key_changes_drop_pending_result_even_after_returning_to_the_same_key() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut check = ConnectionCheck {
            key: "first".into(),
            state: ConnectionState::Checking,
            pending: Some(rx),
        };
        check.poll_key("replacement".into());
        assert_eq!(check.state, ConnectionState::Idle);
        assert!(tx.send(Ok(())).is_err());
        check.poll_key("first".into());
        assert_eq!(check.state, ConnectionState::Idle);
        let (tx, rx) = mpsc::sync_channel(1);
        check.pending = Some(rx);
        check.state = ConnectionState::Checking;
        tx.send(Err("new test failed".into())).unwrap();
        check.poll_key("first".into());
        assert_eq!(
            check.state,
            ConnectionState::Failed("new test failed".into())
        );
    }

    #[test]
    fn stopped_connection_test_does_not_remain_checking() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut check = ConnectionCheck {
            key: "fixture".into(),
            state: ConnectionState::Checking,
            pending: Some(rx),
        };
        drop(tx);
        check.poll_key("fixture".into());
        assert!(matches!(check.state, ConnectionState::Failed(_)));
        assert!(check.pending.is_none());
    }

    #[test]
    fn only_provider_links_and_ids_are_accepted() {
        assert_eq!(
            message_id("https://giphy.com/gifs/crying-abc123"),
            Some("abc123".into())
        );
        for url in [
            "http://giphy.com/gifs/a",
            "https://giphy.com.evil.test/gifs/a",
            "https://user@giphy.com/gifs/a",
            "https://giphy.com/gifs/a/b",
            "text https://giphy.com/gifs/a",
        ] {
            assert!(message_id(url).is_none());
        }
        assert!(valid_media(
            "https://media2.giphy.com/media/a/giphy.gif?cid=keep&rid=giphy.gif"
        ));
        for url in [
            "http://media.giphy.com/a",
            "https://media.giphy.com.evil.test/a",
            "https://localhost/a",
            "https://user@media.giphy.com/a",
        ] {
            assert!(!valid_media(url));
        }
    }
    #[test]
    fn search_keeps_provider_order_and_original_urls() {
        let row = |id: &str| serde_json::json!({"id":id,"title":"Test","images":{"fixed_width_small_still":{"url":"https://media.giphy.com/still.png?cid=preserve"},"fixed_width":{"url":"https://media.giphy.com/animated.gif?rid=preserve"}}});
        let value = serde_json::json!({"data":[row("z"),row("a")],"pagination":{"offset":20,"total_count":50}});
        let result = page(&value).unwrap();
        assert_eq!(
            result
                .gifs
                .iter()
                .map(|gif| gif.id.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert!(result.more);
        assert!(result.gifs[0].still.ends_with("?cid=preserve"));
        assert!(result.gifs[0].animated.ends_with("?rid=preserve"));
        assert_eq!(result.gifs[0].preview, result.gifs[0].animated);
        let mut small = row("small");
        small["images"]["fixed_width_small"]["url"] =
            "https://media.giphy.com/small.gif?cid=original".into();
        assert_eq!(
            gif(&small).unwrap().preview,
            "https://media.giphy.com/small.gif?cid=original"
        );
        small["images"]["fixed_width_small"]["url"] = "https://untrusted.example/small.gif".into();
        assert!(gif(&small).is_err());
        let invalid = serde_json::json!({"data":[row("valid"),row("../invalid")]});
        assert!(
            page(&invalid).is_err(),
            "never silently filter or reorder a provider response"
        );
    }
}
