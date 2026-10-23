//! The existing event subscriber owns this bounded call/Friends-message observer. It stays
//! live when the UI update channel is full or the tray window has stopped paint.
use super::*;
use zeroize::Zeroize;

/// Preserve cursor presence at the authenticated response boundary: serde's
/// legacy u64 default cannot distinguish an empty feed from an absent cursor.
#[derive(Debug)]
pub(super) struct BootstrapReply {
    pub bootstrap: Bootstrap,
    pub cursor: Option<u64>,
}
impl<'de> serde::Deserialize<'de> for BootstrapReply {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        let cursor = value
            .get("latest_change_sequence")
            .and_then(serde_json::Value::as_u64);
        let bootstrap = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { bootstrap, cursor })
    }
}

pub(super) struct Watch {
    alerts: crate::call_alerts::Alerts,
    origin: CommandOrigin,
    generation: u64,
    token: zeroize::Zeroizing<String>,
    http: Client,
    last_poll: Instant,
    stopped: bool,
    dirty: bool,
    failures: u32,
    retry_at: Option<Instant>,
    shutdown: Arc<AtomicBool>,
}
impl Worker {
    #[cfg(test)]
    pub(super) fn record_call_bootstrap(
        &self,
        origin: &CommandOrigin,
        bootstrap: &Bootstrap,
        received: Instant,
    ) {
        self.record_alert_bootstrap(
            origin,
            bootstrap,
            received,
            Some(bootstrap.latest_change_sequence),
        );
    }
    pub(super) fn record_alert_bootstrap(
        &self,
        origin: &CommandOrigin,
        bootstrap: &Bootstrap,
        received: Instant,
        cursor: Option<u64>,
    ) {
        let Some(alerts) = &self.call_alerts else {
            return;
        };
        if validate_bootstrap_identity(&origin.server_id, &origin.user_id, bootstrap).is_err() {
            return;
        }
        if let Some(generation) = alerts.generation(origin) {
            alerts.observe_received_cursor(
                origin,
                generation,
                bootstrap,
                received,
                Some(crate::call_alerts::MessageCursor::baseline(cursor)),
            );
        }
    }
    #[cfg(test)]
    pub(super) fn call_alert_watch(
        &self,
        bootstrap: &Bootstrap,
        received: Instant,
    ) -> Option<Watch> {
        self.alert_watch(bootstrap, received, Some(bootstrap.latest_change_sequence))
    }
    pub(super) fn alert_watch(
        &self,
        bootstrap: &Bootstrap,
        received: Instant,
        cursor: Option<u64>,
    ) -> Option<Watch> {
        if !bootstrap
            .capabilities
            .iter()
            .any(|c| c == "private-calls-v1" || c == "people-v1")
        {
            return None;
        }
        let connection = self.connection.as_ref()?;
        let origin = CommandOrigin::from(connection);
        validate_bootstrap_identity(&origin.server_id, &origin.user_id, bootstrap).ok()?;
        let alerts = self.call_alerts.as_ref()?.clone();
        let generation = alerts.bind(origin.clone())?;
        alerts.observe_received_cursor(
            &origin,
            generation,
            bootstrap,
            received,
            Some(crate::call_alerts::MessageCursor::baseline(cursor)),
        );
        Some(Watch {
            alerts,
            origin,
            generation,
            token: zeroize::Zeroizing::new(connection.session.token.clone()),
            http: self.http.clone(),
            last_poll: Instant::now(),
            stopped: false,
            dirty: false,
            failures: 0,
            retry_at: None,
            shutdown: self.live.shutdown.clone(),
        })
    }
}
impl Watch {
    pub(super) fn tick(&mut self, changed: bool) {
        if self.stopped {
            return;
        }
        if self.shutdown.load(Ordering::Acquire)
            || !self.alerts.is_current(&self.origin, self.generation)
        {
            self.stop();
            return;
        }
        self.dirty |= changed;
        if self.retry_at.is_none_or(|at| Instant::now() >= at)
            && ((self.dirty && self.last_poll.elapsed() >= Duration::from_millis(200))
                || self.last_poll.elapsed()
                    >= self.alerts.poll_interval(&self.origin, self.generation))
        {
            self.last_poll = Instant::now();
            self.dirty = false;
            // No discovery, login, redirects, E2EE mutation or caller-provided
            // endpoint: only this already authenticated exact service account.
            let response = self
                .http
                .get(crate::embedded_network::endpoint(&format!(
                    "{}/api/bootstrap",
                    self.origin.url
                )))
                .bearer_auth(self.token.as_str())
                .timeout(Duration::from_secs(2))
                .send();
            let mut refreshed = false;
            match response {
                Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
                    self.stop();
                    return;
                }
                Ok(response) => {
                    if let Ok(BootstrapReply { bootstrap, cursor }) =
                        parse::<BootstrapReply>(response)
                    {
                        let received = Instant::now();
                        if validate_bootstrap_identity(
                            &self.origin.server_id,
                            &self.origin.user_id,
                            &bootstrap,
                        )
                        .is_err()
                        {
                            self.stop();
                            return;
                        }
                        if self.shutdown.load(Ordering::Acquire)
                            || !self.alerts.is_current(&self.origin, self.generation)
                        {
                            self.stop();
                            return;
                        }
                        self.alerts.observe_received_cursor(
                            &self.origin,
                            self.generation,
                            &bootstrap,
                            received,
                            Some(crate::call_alerts::MessageCursor::polled(
                                cursor,
                                self.last_poll,
                            )),
                        );
                        refreshed = self.refresh_messages();
                        if self.stopped {
                            return;
                        }
                    }
                }
                Err(_) => {} // Keep only the original bounded ringing deadline.
            }
            if refreshed {
                self.failures = 0;
                self.retry_at = None;
            } else {
                // Retain cursor/claims, but let an authenticated painted UI
                // take over while this subscriber backs off after an error.
                self.alerts
                    .release_message_feed(&self.origin, self.generation);
                self.failures = self.failures.saturating_add(1).min(5);
                self.retry_at =
                    Some(Instant::now() + Duration::from_secs((1u64 << self.failures).min(30)));
            }
        }
        self.alerts.tick();
    }
    fn refresh_messages(&mut self) -> bool {
        let Some(after) = self.alerts.message_cursor(&self.origin, self.generation) else {
            return true;
        };
        // One bounded page per scheduled refresh. Bootstrap supplies current
        // friend/block/DnD metadata; ciphertext is neither opened nor previewed.
        let response = self
            .http
            .get(crate::embedded_network::endpoint(&format!(
                "{}/api/changes",
                self.origin.url
            )))
            .bearer_auth(self.token.as_str())
            .query(&[("after", after), ("limit", 200)])
            .timeout(Duration::from_secs(2))
            .send();
        let response = match response {
            Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
                self.stop();
                return false;
            }
            Ok(response) => response,
            Err(_) => return false,
        };
        let Ok(page) = parse::<ChangePage>(response) else {
            return false;
        };
        if self.shutdown.load(Ordering::Acquire)
            || !self.alerts.is_current(&self.origin, self.generation)
        {
            self.stop();
            return false;
        }
        let valid = self
            .alerts
            .observe_message_page(&self.origin, self.generation, after, &page);
        self.dirty |= valid && page.has_more;
        valid
    }
    fn stop(&mut self) {
        self.stopped = true;
        self.token.zeroize();
        self.alerts.remove(&self.origin, self.generation);
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}
