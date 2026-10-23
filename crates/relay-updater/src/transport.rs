use super::{Publisher, Result, MAX_INSTALLER, MAX_METADATA};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tough::{Transport, TransportError, TransportErrorKind, TransportStream};
use url::Url;

pub(super) fn base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|e| e.to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err("Updates require a bundled HTTPS publisher address.".into());
    }
    Ok(url)
}
fn allowed(url: &Url, base: &Url) -> bool {
    url.scheme() == "https"
        && url.origin() == base.origin()
        && url.path().starts_with(base.path())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

#[derive(Clone, Debug)]
pub(super) struct HttpTransport {
    client: reqwest::Client,
    metadata: Url,
    targets: Url,
}
impl HttpTransport {
    pub fn new(publisher: &Publisher) -> Result<Self> {
        super::initialize_tls_provider();
        let client = Self::builder().build().map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            metadata: publisher.metadata.clone(),
            targets: publisher.targets.clone(),
        })
    }
    fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("Relay-Updater/", env!("CARGO_PKG_VERSION")))
    }
    #[cfg(test)]
    pub(super) fn with_fixture_certificate(publisher: &Publisher, der: &[u8]) -> Result<Self> {
        let mut transport = Self::new(publisher)?;
        transport.client = Self::builder()
            .add_root_certificate(reqwest::Certificate::from_der(der).map_err(|e| e.to_string())?)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(transport)
    }
}
#[async_trait]
impl Transport for HttpTransport {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        let max = if allowed(&url, &self.metadata) {
            MAX_METADATA
        } else if allowed(&url, &self.targets) {
            MAX_INSTALLER
        } else {
            return Err(TransportError::new(
                TransportErrorKind::UnsupportedUrlScheme,
                &url,
            ));
        };
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| TransportError::new_with_cause(TransportErrorKind::Other, &url, e))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(TransportError::new(TransportErrorKind::FileNotFound, &url));
        }
        if response.status() != reqwest::StatusCode::OK
            || response.content_length().is_some_and(|length| length > max)
        {
            return Err(TransportError::new(TransportErrorKind::Other, &url));
        }
        let mut count = 0u64;
        let stream = response.bytes_stream().map(move |result| {
            let bytes = result
                .map_err(|e| TransportError::new_with_cause(TransportErrorKind::Other, &url, e))?;
            count = count.saturating_add(bytes.len() as u64);
            if count > max {
                return Err(TransportError::new(TransportErrorKind::Other, &url));
            }
            Ok(bytes)
        });
        Ok(Box::pin(stream))
    }
}

#[derive(Debug, Clone)]
pub(super) struct RootRecorder<T> {
    inner: T,
    base: Url,
    roots: Arc<Mutex<BTreeMap<u64, Vec<u8>>>>,
}
impl<T> RootRecorder<T> {
    pub fn new(inner: T, base: Url, roots: Arc<Mutex<BTreeMap<u64, Vec<u8>>>>) -> Self {
        Self { inner, base, roots }
    }
}
#[async_trait]
impl<T: Transport + Clone + 'static> Transport for RootRecorder<T> {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        let mut stream = self.inner.fetch(url.clone()).await?;
        let name = url.path().strip_prefix(self.base.path()).unwrap_or("");
        let version = name
            .strip_suffix(".root.json")
            .and_then(|s| s.parse::<u64>().ok());
        if !allowed(&url, &self.base) || version.is_none() {
            return Ok(stream);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.try_next().await? {
            if bytes.len() as u64 + chunk.len() as u64 > MAX_METADATA {
                return Err(TransportError::new(TransportErrorKind::Other, &url));
            }
            bytes.extend_from_slice(&chunk);
        }
        self.roots
            .lock()
            .map_err(|_| TransportError::new(TransportErrorKind::Other, &url))?
            .insert(version.unwrap(), bytes.clone());
        Ok(Box::pin(futures_util::stream::iter([Ok(bytes.into())])))
    }
}
