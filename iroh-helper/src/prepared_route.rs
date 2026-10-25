//! Short-lived preflight routes, independent of every normal client route.
//! Each lease owns a fresh client/endpoint and its own loopback sockets. Neither
//! opening nor closing it can replace an existing admission or touch live calls.
//!
//! The same bounded pool also holds independent voice-gateway upstream routes. A
//! gateway lease is a distinct internal purpose: preflight leases never renew, a gateway
//! lease renews only through `renew_gateway`, and neither purpose can read, renew or
//! close the other's id. Renewal moves one expiry value; it never touches the entry's
//! client, connection, admission or sockets.
use crate::{
    client::{Client, RouteInfo, RouteSpec},
    protocol, token, Network,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore},
    time::{timeout, timeout_at, Duration, Instant},
};

pub const LIFETIME: Duration = Duration::from_secs(120);
pub const MAX_OPEN: usize = 2;
const OPEN_TIMEOUT: Duration = Duration::from_secs(22);
const CLEANUP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Busy,
    Failed,
    Missing,
}

/// What a lease is for. It is never supplied by a caller of the control API: each
/// endpoint fixes it, and a lease of one purpose is `Missing` to the other's calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Preflight,
    VoiceGateway,
}
impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Preflight => "preflight",
            Purpose::VoiceGateway => "voice_gateway",
        }
    }
}

/// No admission token; the random lease identifies only this private helper's resource.
pub struct Lease {
    pub id: String,
    pub purpose: Purpose,
    pub route: RouteInfo,
    pub expires_in_secs: u64,
}
struct Entry {
    purpose: Purpose,
    route: RouteInfo,
    expires: Instant,
    /// Wakes the entry's single timer task when a gateway lease is renewed.
    expiry: watch::Sender<Instant>,
    stop: oneshot::Sender<()>,
    done: oneshot::Receiver<()>,
}
type Book = Mutex<HashMap<String, Entry>>;
pub struct Store {
    book: Arc<Book>,
    slots: Arc<Semaphore>,
    stop: watch::Sender<bool>,
    lifetime: Duration,
}
impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
impl Store {
    pub fn new() -> Self {
        Self::with_lifetime(LIFETIME)
    }
    /// Shorter lifetime for device-free fixtures; not an input of the control API.
    #[doc(hidden)]
    pub fn with_lifetime(lifetime: Duration) -> Self {
        assert!(!lifetime.is_zero() && lifetime <= LIFETIME);
        Self {
            book: Arc::default(),
            slots: Arc::new(Semaphore::new(MAX_OPEN)),
            stop: watch::channel(false).0,
            lifetime,
        }
    }
    pub async fn open(&self, network: &Network, spec: RouteSpec) -> Result<Lease, Error> {
        self.open_as(Purpose::Preflight, network, spec).await
    }
    /// Opens an independent upstream route for a voice gateway. It shares the pool and
    /// its two slots with preflight leases and expires like them unless renewed.
    pub async fn open_gateway(&self, network: &Network, spec: RouteSpec) -> Result<Lease, Error> {
        self.open_as(Purpose::VoiceGateway, network, spec).await
    }
    async fn open_as(
        &self,
        purpose: Purpose,
        network: &Network,
        spec: RouteSpec,
    ) -> Result<Lease, Error> {
        if !protocol::valid_token(&spec.token) {
            return Err(Error::Invalid);
        }
        let mut shutdown = self.stop.subscribe();
        if *shutdown.borrow() {
            return Err(Error::Failed);
        }
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let mut own = Own {
            client: None,
            permit: Some(permit),
        };
        let deadline = Instant::now() + OPEN_TIMEOUT;
        let opened = tokio::select! {
            result = timeout_at(deadline, async {
                own.client = Some(Arc::new(Client::new(network.clone()).await?));
                own.client.as_ref().unwrap().route(spec).await
            }) => result.ok().and_then(Result::ok),
            _ = shutdown.changed() => None,
        };
        let Some(route) = opened else {
            own.finish().await;
            return Err(Error::Failed);
        };
        if *shutdown.borrow() {
            own.finish().await;
            return Err(Error::Failed);
        }
        let id = match token::new_token() {
            Ok(id) => id,
            Err(_) => {
                own.finish().await;
                return Err(Error::Failed);
            }
        };
        let expires = Instant::now() + self.lifetime;
        let (stop, mut stopped) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        let (expiry, mut expiry_changes) = watch::channel(expires);
        // Unique names are never replaced: release can only close its exact lease.
        let inserted = {
            let mut book = self.book.lock().unwrap_or_else(|e| e.into_inner());
            if book.contains_key(&id) {
                false
            } else {
                book.insert(
                    id.clone(),
                    Entry {
                        purpose,
                        route,
                        expires,
                        expiry,
                        stop,
                        done: finished,
                    },
                );
                true
            }
        };
        if !inserted {
            own.finish().await;
            return Err(Error::Failed);
        }
        let weak = Arc::downgrade(&self.book);
        let key = id.clone();
        // One task per lease for its whole life, however often it is renewed.
        tokio::spawn(async move {
            // A dropped Store also drops the watch sender and ends its resources.
            loop {
                let at = *expiry_changes.borrow_and_update();
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {
                        // A renewal that raced the timer wins: the book decides, and the
                        // removal happens under the same lock so a renewal cannot be
                        // acknowledged and then lose its entry.
                        let due = match weak.upgrade() {
                            Some(book) => {
                                let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
                                if book.get(&key).is_some_and(|e| e.expires > Instant::now()) {
                                    false
                                } else {
                                    book.remove(&key);
                                    true
                                }
                            }
                            None => true,
                        };
                        if due {
                            break;
                        }
                    },
                    changed = expiry_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    },
                    _ = &mut stopped => break,
                    _ = shutdown.changed() => break,
                }
            }
            // Only this lease's own key: a replacement has a different random id.
            if let Some(book) = weak.upgrade() {
                book.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            }
            own.finish().await;
            let _ = done.send(());
        });
        Ok(Lease {
            id,
            purpose,
            route,
            expires_in_secs: remaining(expires),
        })
    }
    pub fn status(&self, id: &str) -> Result<Lease, Error> {
        self.status_as(Purpose::Preflight, id)
    }
    pub fn status_gateway(&self, id: &str) -> Result<Lease, Error> {
        self.status_as(Purpose::VoiceGateway, id)
    }
    fn status_as(&self, purpose: Purpose, id: &str) -> Result<Lease, Error> {
        if !valid_id(id) {
            return Err(Error::Invalid);
        }
        let book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let entry = book
            .get(id)
            .filter(|e| e.purpose == purpose && e.expires > Instant::now())
            .ok_or(Error::Missing)?;
        Ok(Lease {
            id: id.into(),
            purpose,
            route: entry.route,
            expires_in_secs: remaining(entry.expires),
        })
    }
    /// Resets a LIVE gateway lease to the fixed lifetime from now. Nothing is supplied by
    /// the caller but the id; the same client, connection, admission and sockets stay.
    /// An expired, closed, unknown or preflight id is `Missing`: renewal never reopens.
    pub fn renew_gateway(&self, id: &str) -> Result<Lease, Error> {
        if !valid_id(id) {
            return Err(Error::Invalid);
        }
        let mut book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = book
            .get_mut(id)
            .filter(|e| e.purpose == Purpose::VoiceGateway && e.expires > now)
            .ok_or(Error::Missing)?;
        entry.expires = now + self.lifetime;
        let _ = entry.expiry.send_replace(entry.expires);
        Ok(Lease {
            id: id.into(),
            purpose: Purpose::VoiceGateway,
            route: entry.route,
            expires_in_secs: remaining(entry.expires),
        })
    }
    pub async fn close(&self, id: &str) -> Result<(), Error> {
        self.close_as(Purpose::Preflight, id).await
    }
    pub async fn close_gateway(&self, id: &str) -> Result<(), Error> {
        self.close_as(Purpose::VoiceGateway, id).await
    }
    async fn close_as(&self, purpose: Purpose, id: &str) -> Result<(), Error> {
        if !valid_id(id) {
            return Err(Error::Invalid);
        }
        // The purpose is checked under the same lock as the removal: the other
        // purpose's lease is never taken out of the book by a wrong-purpose call.
        let entry = {
            let mut book = self.book.lock().unwrap_or_else(|e| e.into_inner());
            if book.get(id).is_some_and(|e| e.purpose == purpose) {
                book.remove(id)
            } else {
                None
            }
        }
        .ok_or(Error::Missing)?;
        let expired = entry.expires <= Instant::now();
        let _ = entry.stop.send(());
        let _ = timeout(CLEANUP + Duration::from_millis(100), entry.done).await;
        if expired { Err(Error::Missing) } else { Ok(()) }
    }
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        let entries: Vec<_> = self
            .book
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        let mut pending = Vec::new();
        for entry in entries {
            let _ = entry.stop.send(());
            pending.push(entry.done);
        }
        let _ = timeout(CLEANUP + Duration::from_millis(100), async {
            for done in pending {
                let _ = done.await;
            }
        })
        .await;
    }
}
fn remaining(expires: Instant) -> u64 {
    expires
        .saturating_duration_since(Instant::now())
        .as_millis()
        .div_ceil(1000) as u64
}
fn valid_id(id: &str) -> bool {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    id.len() == 43
        && URL_SAFE_NO_PAD
            .decode(id)
            .is_ok_and(|v| v.len() == 32 && URL_SAFE_NO_PAD.encode(v) == id)
}
/// Cancellation before an HTTP reply or while opening a route closes only this client.
struct Own {
    client: Option<Arc<Client>>,
    permit: Option<OwnedSemaphorePermit>,
}
impl Own {
    async fn finish(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = timeout(CLEANUP, client.close()).await;
        }
        self.permit.take();
    }
}
impl Drop for Own {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let permit = self.permit.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _permit = permit;
                    let _ = timeout(CLEANUP, client.close()).await;
                });
            }
        }
    }
}
