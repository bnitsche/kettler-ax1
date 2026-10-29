//! What only the service page needs: the raw traffic of the device link (ring buffer plus
//! broadcast), the device's identity and connection state, and the raw modem lines.
//! Every call is cheap and never waits, so the device task can call it in its poll loop.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{broadcast, watch};

use crate::keypad::ModemLines;

/// Entries kept for a service page that opens later.
pub const TRACE_CAPACITY: usize = 200;
/// A slower subscriber lags behind and gets the history again.
const BROADCAST_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Sent,
    Received,
    /// A line `wait` did not take.
    Discarded,
    Timeout,
    Key,
    /// Connected or disconnected.
    Link,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TraceEntry {
    pub at_ms: i64,
    pub kind: TraceKind,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Disconnect {
    pub at_ms: i64,
    pub reason: String,
}

/// `id` and `firmware` stay at their last known values while disconnected.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub connected: bool,
    /// Since when connected, or disconnected.
    pub since_ms: Option<i64>,
    pub id: Option<String>,
    pub firmware: Option<String>,
    pub last_disconnect: Option<Disconnect>,
}

#[derive(Clone)]
pub struct Monitor {
    history: Arc<Mutex<VecDeque<TraceEntry>>>,
    traces: broadcast::Sender<TraceEntry>,
    info: Arc<watch::Sender<DeviceInfo>>,
    modem: Arc<watch::Sender<Option<ModemLines>>>,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            history: Arc::new(Mutex::new(VecDeque::with_capacity(TRACE_CAPACITY))),
            traces: broadcast::channel(BROADCAST_CAPACITY).0,
            info: Arc::new(watch::channel(DeviceInfo::default()).0),
            modem: Arc::new(watch::channel(None).0),
        }
    }

    pub fn trace(&self, kind: TraceKind, text: impl Into<String>) {
        let entry = TraceEntry { at_ms: now_ms(), kind, text: text.into() };
        {
            let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            if history.len() == TRACE_CAPACITY {
                history.pop_front();
            }
            history.push_back(entry.clone());
        }
        let _ = self.traces.send(entry);
    }

    /// Oldest first.
    pub fn history(&self) -> Vec<TraceEntry> {
        self.history.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TraceEntry> {
        self.traces.subscribe()
    }

    pub fn info(&self) -> DeviceInfo {
        self.info.borrow().clone()
    }

    #[cfg(test)]
    pub fn watch_info(&self) -> watch::Receiver<DeviceInfo> {
        self.info.subscribe()
    }

    pub fn update_info(&self, f: impl FnOnce(&mut DeviceInfo)) {
        self.info.send_modify(f);
    }

    /// Published only when the lines changed.
    pub fn set_modem(&self, lines: Option<ModemLines>) {
        self.modem.send_if_modified(|current| {
            let changed = *current != lines;
            *current = lines;
            changed
        });
    }

    pub fn modem(&self) -> Option<ModemLines> {
        *self.modem.borrow()
    }

    pub fn watch_modem(&self) -> watch::Receiver<Option<ModemLines>> {
        self.modem.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    #[test]
    fn history_keeps_the_last_200_in_order() {
        let m = Monitor::new();
        for i in 0..250 {
            m.trace(TraceKind::Sent, i.to_string());
        }
        let h = m.history();
        assert_eq!(h.len(), TRACE_CAPACITY);
        assert_eq!(h[0].text, "50");
        assert_eq!(h[199].text, "249");
    }

    #[test]
    fn subscribers_get_new_entries() {
        let m = Monitor::new();
        let mut rx = m.subscribe();
        m.trace(TraceKind::Received, "ACK");
        let entry = rx.try_recv().unwrap();
        assert_eq!(entry.kind, TraceKind::Received);
        assert_eq!(entry.text, "ACK");
        assert!(entry.at_ms > 0);
    }

    #[test]
    fn trace_never_blocks_without_or_with_slow_subscribers() {
        let m = Monitor::new();
        for _ in 0..1000 {
            m.trace(TraceKind::Sent, "ST");
        }
        let mut rx = m.subscribe();
        for _ in 0..1000 {
            m.trace(TraceKind::Sent, "ST");
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(_))));
    }

    #[test]
    fn set_modem_publishes_only_changes() {
        let m = Monitor::new();
        let mut rx = m.watch_modem();
        let lines = ModemLines { cts: true, ..ModemLines::default() };
        m.set_modem(Some(lines));
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Some(lines));
        m.set_modem(Some(lines));
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn update_info_changes_the_info() {
        let m = Monitor::new();
        m.update_info(|i| {
            i.connected = true;
            i.firmware = Some("018".into());
        });
        assert!(m.info().connected);
        assert_eq!(m.watch_info().borrow().firmware.as_deref(), Some("018"));
    }
}
