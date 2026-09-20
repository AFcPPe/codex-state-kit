use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountTraffic {
    pub concurrent_requests: usize,
    pub rpm: usize,
}

#[derive(Default)]
struct AccountRequests {
    active: usize,
    starts: VecDeque<Instant>,
}

#[derive(Clone, Default)]
pub(crate) struct TrafficTracker(Arc<Mutex<HashMap<String, AccountRequests>>>);

impl TrafficTracker {
    pub fn begin(&self, account: &str, now: Instant) -> RequestActivity {
        let mut accounts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut accounts, now);
        let requests = accounts.entry(account.to_owned()).or_default();
        requests.active += 1;
        requests.starts.push_back(now);
        RequestActivity {
            tracker: self.clone(),
            account: account.to_owned(),
        }
    }

    pub fn view(&self, account: Option<&str>, now: Instant) -> AccountTraffic {
        let mut accounts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut accounts, now);
        account
            .and_then(|id| accounts.get(id))
            .map(|requests| AccountTraffic {
                concurrent_requests: requests.active,
                rpm: requests.starts.len(),
            })
            .unwrap_or_default()
    }
}

fn prune(accounts: &mut HashMap<String, AccountRequests>, now: Instant) {
    accounts.retain(|_, requests| {
        while requests.starts.front().is_some_and(|started| {
            now.saturating_duration_since(*started) >= Duration::from_secs(60)
        }) {
            requests.starts.pop_front();
        }
        requests.active > 0 || !requests.starts.is_empty()
    });
}

/// Owned by the pending upstream request, then by its response body. Dropping
/// either releases concurrency synchronously, including task cancellation.
pub(crate) struct RequestActivity {
    tracker: TrafficTracker,
    account: String,
}

impl Drop for RequestActivity {
    fn drop(&mut self) {
        let mut accounts = self
            .tracker
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(requests) = accounts.get_mut(&self.account) {
            requests.active = requests.active.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_expires_starts_without_losing_long_requests() {
        let tracker = TrafficTracker::default();
        let now = Instant::now();
        let first = tracker.begin("a", now);
        let second = tracker.begin("a", now + Duration::from_secs(30));
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(59)),
            AccountTraffic {
                concurrent_requests: 2,
                rpm: 2
            }
        );
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(60)),
            AccountTraffic {
                concurrent_requests: 2,
                rpm: 1
            }
        );
        drop(first);
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(90)),
            AccountTraffic {
                concurrent_requests: 1,
                rpm: 0
            }
        );
        drop(second);
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(91)),
            AccountTraffic::default()
        );
        assert!(tracker.0.lock().unwrap().is_empty());
    }

    #[test]
    fn switching_accounts_and_finishing_old_requests_keeps_counts_separate() {
        let tracker = TrafficTracker::default();
        let now = Instant::now();
        let old = tracker.begin("old", now);
        let new = tracker.begin("new", now);
        drop(old);
        assert_eq!(
            tracker.view(Some("old"), now),
            AccountTraffic {
                concurrent_requests: 0,
                rpm: 1
            }
        );
        assert_eq!(
            tracker.view(Some("new"), now),
            AccountTraffic {
                concurrent_requests: 1,
                rpm: 1
            }
        );
        assert_eq!(tracker.view(None, now), AccountTraffic::default());
        drop(new);
    }
}
