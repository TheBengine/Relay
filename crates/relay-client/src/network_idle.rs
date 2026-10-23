//! Coalesced worker wakeups. A generation prevents notifications arriving
//! between queue inspection and sleep from being lost.
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub(super) struct Wake(Arc<(Mutex<u64>, Condvar)>);

impl Wake {
    pub(super) fn generation(&self) -> u64 {
        *self.0 .0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(super) fn notify(&self) {
        let mut generation = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        *generation = generation.wrapping_add(1);
        self.0 .1.notify_all();
    }
    pub(super) fn wait(&self, observed: u64, timeout: Option<Duration>) {
        let generation = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(timeout) = timeout {
            drop(
                self.0
                     .1
                    .wait_timeout_while(generation, timeout, |g| *g == observed),
            );
        } else {
            drop(self.0 .1.wait_while(generation, |g| *g == observed));
        }
    }
}

pub struct Sender<T> {
    sender: mpsc::SyncSender<T>,
    wake: Wake,
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: self.wake.clone(),
        }
    }
}
impl<T> Sender<T> {
    pub(super) fn new(sender: mpsc::SyncSender<T>, wake: Wake) -> Self {
        Self { sender, wake }
    }
    pub fn send(&self, item: T) -> Result<(), mpsc::SendError<T>> {
        // Wake before a potentially blocking bounded send, and again after
        // enqueueing in case the worker drained the queue between these steps.
        self.wake.notify();
        let result = self.sender.send(item);
        self.wake.notify();
        result
    }
    pub fn try_send(&self, item: T) -> Result<(), mpsc::TrySendError<T>> {
        let result = self.sender.try_send(item);
        self.wake.notify();
        result
    }
}

impl super::Worker {
    pub(super) fn idle_delay(&self, refreshed: std::time::Instant) -> Option<Duration> {
        let now = std::time::Instant::now();
        let mut next = None;
        let mut schedule = |at: std::time::Instant| {
            next = Some(next.map_or(at, |old: std::time::Instant| old.min(at)));
        };
        if self.connection.is_some() {
            schedule(refreshed + Duration::from_secs(5));
            if self
                .refresh_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                schedule(refreshed + Duration::from_millis(400));
            }
            if !self.socket_stop.load(std::sync::atomic::Ordering::Relaxed)
                && self.status != Some(super::ConnectionStatus::SignInRequired)
            {
                if let Some(at) = self
                    .connection
                    .as_ref()
                    .filter(|c| c.current.is_some())
                    .and_then(|c| c.chat.idle_deadline(now))
                {
                    schedule(at);
                }
            }
        }
        if !self.background_pending.is_empty() {
            schedule(now);
        }
        if let Some(at) = self.legacy_reactions.next_deadline() {
            schedule(at);
        }
        for (url, saved) in &self.saved_connections {
            if self.background_jobs.len() < 4
                && saved.dirty.load(std::sync::atomic::Ordering::Relaxed)
                && !self.background_jobs.contains_key(url)
            {
                let retry = self.background_retry.get(url).map_or(now, |(_, at)| *at);
                let due = (self.background_at + Duration::from_millis(750)).max(retry);
                // Two HTTP permits may still be held briefly after completion.
                // A bounded retry avoids both a hot loop and a missed permit release.
                let due = if self
                    .background_active
                    .load(std::sync::atomic::Ordering::Relaxed)
                    >= 2
                {
                    due.max(now + Duration::from_millis(100))
                } else {
                    due
                };
                schedule(due);
            }
        }
        next.map(|at| at.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Instant};
    #[test]
    fn notification_between_inspection_and_sleep_is_not_lost() {
        let wake = Wake::default();
        let observed = wake.generation();
        wake.notify();
        let at = Instant::now();
        wake.wait(observed, Some(Duration::from_secs(2)));
        assert!(at.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn completion_and_shutdown_wake_a_blocked_worker() {
        let wake = Wake::default();
        let (tx, rx) = mpsc::sync_channel(1);
        let tx = Sender::new(tx, wake.clone());
        let observed = wake.generation();
        let task = thread::spawn(move || {
            tx.send(7).unwrap();
        });
        wake.wait(observed, Some(Duration::from_secs(2)));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 7);
        task.join().unwrap();
    }
    #[test]
    fn a_full_completion_queue_wakes_before_blocking() {
        let wake = Wake::default();
        let (tx, rx) = mpsc::sync_channel(1);
        let tx = Sender::new(tx, wake.clone());
        tx.send(1).unwrap();
        let observed = wake.generation();
        let task = thread::spawn(move || {
            tx.send(2).unwrap();
        });
        wake.wait(observed, Some(Duration::from_secs(2)));
        assert_ne!(wake.generation(), observed);
        assert_eq!(rx.recv().unwrap(), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 2);
        task.join().unwrap();
    }
}
