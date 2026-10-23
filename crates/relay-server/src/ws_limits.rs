//! Bounded authenticated socket admission, released when the socket task ends.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(crate) struct ConnectionBudget(Arc<Budget>);

struct Budget {
    global_limit: usize,
    user_limit: usize,
    counts: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    total: usize,
    users: HashMap<String, usize>,
}

pub(crate) struct ConnectionPermit {
    budget: Arc<Budget>,
    user: String,
}

impl ConnectionBudget {
    pub(crate) fn new(global_limit: usize, user_limit: usize) -> Self {
        assert!(global_limit > 0 && user_limit > 0);
        Self(Arc::new(Budget {
            global_limit,
            user_limit,
            counts: Mutex::new(Counts::default()),
        }))
    }

    pub(crate) fn try_acquire(&self, user: &str) -> Option<ConnectionPermit> {
        let mut counts = self.0.counts.lock().unwrap_or_else(|e| e.into_inner());
        if counts.total >= self.0.global_limit
            || counts.users.get(user).copied().unwrap_or(0) >= self.0.user_limit
        {
            return None;
        }
        counts.total += 1;
        *counts.users.entry(user.into()).or_default() += 1;
        Some(ConnectionPermit {
            budget: self.0.clone(),
            user: user.into(),
        })
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut counts = self.budget.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.total -= 1;
        if let Some(count) = counts.users.get_mut(&self.user) {
            *count -= 1;
            if *count == 0 {
                counts.users.remove(&self.user);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_budget_scopes_accounts_and_releases_on_disconnect() {
        let budget = ConnectionBudget::new(3, 2);
        let first = budget.try_acquire("alex").unwrap();
        let second = budget.try_acquire("alex").unwrap();
        assert!(budget.try_acquire("alex").is_none());
        let third = budget.try_acquire("mira").unwrap();
        assert!(budget.try_acquire("jonas").is_none());
        drop(first);
        let replacement = budget.try_acquire("jonas").unwrap();
        drop((second, third, replacement));
        let counts = budget.0.counts.lock().unwrap();
        assert_eq!(counts.total, 0);
        assert!(
            counts.users.is_empty(),
            "Disconnected identities must not accumulate"
        );
    }

    #[test]
    fn concurrent_socket_admission_never_exceeds_limits() {
        let budget = ConnectionBudget::new(4, 2);
        let barrier = Arc::new(std::sync::Barrier::new(17));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let budget = budget.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let permit = budget.try_acquire(if i % 2 == 0 { "alex" } else { "mira" });
                    barrier.wait();
                    barrier.wait();
                    drop(permit);
                })
            })
            .collect();
        barrier.wait();
        {
            let counts = budget.0.counts.lock().unwrap();
            assert_eq!(counts.total, 4);
            assert_eq!(counts.users["alex"], 2);
            assert_eq!(counts.users["mira"], 2);
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(budget.0.counts.lock().unwrap().total, 0);
    }
}
