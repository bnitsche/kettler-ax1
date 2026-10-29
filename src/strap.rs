//! Chest straps: their states (link, pulse, battery) in a `watch`, the rule which strap
//! counts for a ride, and the backends that talk to them (off, simulation, BlueZ). The
//! ride manager only reads from the `watch`, so a hanging strap never delays it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::db::Strap;
use crate::hrm::HeartRate;
use crate::monitor::{Monitor, TraceKind};

/// A pulse older than this counts as none.
pub const STALE_AFTER: Duration = Duration::from_secs(5);
/// How long a search for straps runs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SCAN_TIME: Duration = Duration::from_secs(15);
/// How long `StrapHub::shutdown` waits for the backend to let the straps go.
const SHUTDOWN_TIME: Duration = Duration::from_secs(3);

/// `KETTLER_BLUETOOTH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Off,
    Sim,
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Mode::Auto),
            "off" => Ok(Mode::Off),
            "sim" => Ok(Mode::Sim),
            other => Err(format!("KETTLER_BLUETOOTH={other:?}: erlaubt sind auto, off und sim")),
        }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Availability {
    Off,
    Unavailable { reason: String },
    Ready,
    Sim,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Searching,
    Connected,
    OutOfRange,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct StrapState {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub link: Link,
    /// A valid pulse; `None` if missing or older than `STALE_AFTER` (see `StrapHub::states`).
    pub pulse: Option<u16>,
    pub battery: Option<u8>,
    /// Why the last connect failed; cleared once connected.
    pub problem: Option<String>,
    #[serde(skip)]
    pub pulse_at: Option<Instant>,
}

/// A strap seen by a search.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Found {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScanError {
    #[error("Bluetooth nicht verfügbar")]
    Unavailable,
    #[error("Es läuft schon eine Suche")]
    Busy,
    #[error("Suche fehlgeschlagen: {0}")]
    Failed(String),
}

type ScanReply = oneshot::Sender<Result<Vec<Found>, ScanError>>;

/// What a backend task gets: the known straps, search requests and where to report.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Backend {
    pub known: watch::Receiver<Vec<Strap>>,
    pub scans: mpsc::Receiver<ScanReply>,
    pub reporter: Reporter,
    /// Turns `true` once: disconnect every strap and end.
    pub stop: watch::Receiver<bool>,
}

struct Inner {
    availability: watch::Sender<Availability>,
    states: watch::Sender<Vec<StrapState>>,
    known: watch::Sender<Vec<Strap>>,
    /// `None` without a backend (off or unavailable for good).
    scans: Option<mpsc::Sender<ScanReply>>,
    scanning: AtomicBool,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    monitor: Monitor,
}

#[derive(Clone)]
pub struct StrapHub {
    inner: Arc<Inner>,
}

/// A backend's way to report; cheap and never waits.
#[derive(Clone)]
pub struct Reporter {
    inner: Arc<Inner>,
}

impl Reporter {
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub fn set_availability(&self, availability: Availability) {
        self.inner.availability.send_if_modified(|current| {
            let changed = *current != availability;
            *current = availability;
            changed
        });
    }

    /// The new state of a known strap; a heart rate updates the pulse, `battery` only if
    /// given. Link changes are traced for the service page. Unknown ids are ignored.
    pub fn report(&self, id: i64, link: Link, heart_rate: Option<HeartRate>, battery: Option<u8>) {
        self.update(id, |state| {
            state.link = link;
            if let Some(hr) = heart_rate {
                state.pulse = hr.pulse();
                state.pulse_at = Some(Instant::now());
            }
            if link == Link::Connected {
                state.problem = None;
            } else {
                state.pulse = None;
            }
            if battery.is_some() {
                state.battery = battery;
            }
        });
    }

    /// Why a connect failed. A new reason is logged and traced; `true` if it was new.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub fn fail(&self, id: i64, reason: &str) -> bool {
        let mut name = None;
        self.update(id, |state| {
            if state.problem.as_deref() != Some(reason) {
                state.problem = Some(reason.to_string());
                name = Some(state.name.clone());
            }
        });
        let Some(name) = name else { return false };
        tracing::info!(strap = name, "strap not reached: {reason}");
        self.inner.monitor.trace(TraceKind::Link, format!("Gurt {name} nicht erreicht: {reason}"));
        true
    }

    fn update(&self, id: i64, f: impl FnOnce(&mut StrapState)) {
        let mut trace = None;
        self.inner.states.send_if_modified(|states| {
            let Some(state) = states.iter_mut().find(|s| s.id == id) else { return false };
            let before = state.clone();
            f(state);
            if before.link != state.link {
                if state.link == Link::Connected {
                    trace = Some(format!("Gurt {} verbunden", state.name));
                } else if before.link == Link::Connected {
                    trace = Some(format!("Gurt {} getrennt", state.name));
                }
            }
            *state != before
        });
        if let Some(text) = trace {
            self.inner.monitor.trace(TraceKind::Link, text);
        }
    }
}

impl StrapHub {
    fn new(availability: Availability, monitor: Monitor, backend: bool) -> (StrapHub, Option<Backend>) {
        let (known, known_rx) = watch::channel(Vec::new());
        let (scans, scans_rx) = mpsc::channel(1);
        let (stop, stop_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            availability: watch::channel(availability).0,
            states: watch::channel(Vec::new()).0,
            known,
            scans: backend.then_some(scans),
            scanning: AtomicBool::new(false),
            stop,
            task: Mutex::new(None),
            monitor,
        });
        let backend = backend.then(|| Backend {
            known: known_rx,
            scans: scans_rx,
            reporter: Reporter { inner: inner.clone() },
            stop: stop_rx,
        });
        (StrapHub { inner }, backend)
    }

    /// `Auto` uses BlueZ on Linux and is `Off` elsewhere.
    pub fn spawn(mode: Mode, monitor: Monitor) -> StrapHub {
        match mode {
            Mode::Off => Self::new(Availability::Off, monitor, false).0,
            Mode::Sim => {
                let (hub, backend) = Self::new(Availability::Sim, monitor, true);
                hub.run(run_sim(backend.expect("sim backend")));
                hub
            }
            #[cfg(target_os = "linux")]
            Mode::Auto => {
                let starting = Availability::Unavailable { reason: "wird gestartet".into() };
                let (hub, backend) = Self::new(starting, monitor, true);
                hub.run(crate::strap_bluez::run(backend.expect("bluez backend")));
                hub
            }
            #[cfg(not(target_os = "linux"))]
            Mode::Auto => Self::new(Availability::Off, monitor, false).0,
        }
    }

    fn run(&self, backend: impl Future<Output = ()> + Send + 'static) {
        *self.inner.task.lock().unwrap() = Some(tokio::spawn(backend));
    }

    /// Disconnects every strap, so a phone can have it, and ends the backend; waits at
    /// most `SHUTDOWN_TIME`. Without a backend there is nothing to do.
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        let task = self.inner.task.lock().unwrap().take();
        if let Some(task) = task
            && tokio::time::timeout(SHUTDOWN_TIME, task).await.is_err()
        {
            tracing::warn!("chest straps not released at shutdown");
        }
    }

    pub fn availability(&self) -> Availability {
        self.inner.availability.borrow().clone()
    }

    /// The registered straps; the backend connects to exactly these. Known straps keep
    /// their state, new ones start searching. Without a backend there are no states.
    pub fn set_known(&self, straps: Vec<Strap>) {
        if self.inner.scans.is_some() {
            self.inner.states.send_modify(|states| {
                let old = std::mem::take(states);
                *states = straps
                    .iter()
                    .map(|strap| match old.iter().find(|s| s.id == strap.id) {
                        Some(s) => StrapState { name: strap.name.clone(), address: strap.address.clone(), ..s.clone() },
                        None => StrapState {
                            id: strap.id,
                            name: strap.name.clone(),
                            address: strap.address.clone(),
                            link: Link::Searching,
                            pulse: None,
                            battery: None,
                            problem: None,
                            pulse_at: None,
                        },
                    })
                    .collect();
            });
        }
        self.inner.known.send_replace(straps);
    }

    /// With a stale pulse already removed.
    pub fn states(&self) -> Vec<StrapState> {
        let mut states = self.inner.states.borrow().clone();
        for s in &mut states {
            if !s.pulse_at.is_some_and(|at| at.elapsed() <= STALE_AFTER) {
                s.pulse = None;
            }
        }
        states
    }

    /// Changes with every report.
    pub fn watch(&self) -> watch::Receiver<Vec<StrapState>> {
        self.inner.states.subscribe()
    }

    pub async fn scan(&self) -> Result<Vec<Found>, ScanError> {
        let scans = self.inner.scans.as_ref().ok_or(ScanError::Unavailable)?;
        if self.inner.scanning.swap(true, Ordering::AcqRel) {
            return Err(ScanError::Busy);
        }
        // Frees the flag also if the request is dropped half way.
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _done = Done(&self.inner.scanning);
        let (reply, answer) = oneshot::channel();
        scans.send(reply).await.map_err(|_| ScanError::Unavailable)?;
        answer.await.map_err(|_| ScanError::Failed("Suche abgebrochen".into()))?
    }

    /// A hub with a test backend that tests drive directly.
    #[cfg(test)]
    pub fn test() -> (StrapHub, TestStraps) {
        let (hub, backend) = Self::new(Availability::Ready, Monitor::new(), true);
        let Backend { known, mut scans, reporter, .. } = backend.expect("test backend");
        let scan_result = Arc::new(Mutex::new(Ok(Vec::new())));
        let scan_delay = Arc::new(Mutex::new(Duration::ZERO));
        let (result, delay) = (scan_result.clone(), scan_delay.clone());
        tokio::spawn(async move {
            while let Some(reply) = scans.recv().await {
                let wait = *delay.lock().unwrap();
                tokio::time::sleep(wait).await;
                let _ = reply.send(result.lock().unwrap().clone());
            }
        });
        (hub, TestStraps { reporter, known, scan_result, scan_delay })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Strap(i64),
    None,
    Ambiguous,
}

/// Only the rider's own strap; without one, the one connected strap nobody else has.
pub fn choose_strap(own: Option<i64>, assigned_to_others: &[i64], states: &[StrapState]) -> Choice {
    let connected = |s: &&StrapState| s.link == Link::Connected;
    if let Some(own) = own {
        return match states.iter().filter(connected).any(|s| s.id == own) {
            true => Choice::Strap(own),
            false => Choice::None,
        };
    }
    let mut free = states.iter().filter(connected).filter(|s| !assigned_to_others.contains(&s.id));
    match (free.next(), free.next()) {
        (Some(s), None) => Choice::Strap(s.id),
        (None, _) => Choice::None,
        (Some(_), Some(_)) => Choice::Ambiguous,
    }
}

pub const SIM_ADDRESS: &str = "00:00:00:00:00:01";

/// Simulated straps: a search finds one, known straps connect after a second and send a
/// pulse that wanders between 70 and 150.
async fn run_sim(mut backend: Backend) {
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Straps that were known at the last tick connect at this one.
    let mut seen: Vec<i64> = Vec::new();
    loop {
        tokio::select! {
            request = backend.scans.recv() => match request {
                Some(reply) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let found = Found { address: SIM_ADDRESS.into(), name: Some("Sim-Gurt".into()), rssi: Some(-50) };
                    let _ = reply.send(Ok(vec![found]));
                }
                None => return,
            },
            _ = backend.stop.changed() => {
                for strap in backend.known.borrow().iter() {
                    backend.reporter.report(strap.id, Link::OutOfRange, None, None);
                }
                return;
            }
            _ = tick.tick() => {
                let t = started.elapsed().as_secs_f64();
                let bpm = (110.0 + 30.0 * (t / 20.0).sin() + 8.0 * (t / 3.0).sin()).round() as u16;
                let known: Vec<i64> = backend.known.borrow().iter().map(|s| s.id).collect();
                for &id in &known {
                    if seen.contains(&id) {
                        let hr = HeartRate { bpm, contact: Some(true) };
                        backend.reporter.report(id, Link::Connected, Some(hr), Some(80));
                    }
                }
                seen = known;
            }
        }
    }
}

#[cfg(test)]
pub struct TestStraps {
    reporter: Reporter,
    known: watch::Receiver<Vec<Strap>>,
    scan_result: Arc<Mutex<Result<Vec<Found>, ScanError>>>,
    scan_delay: Arc<Mutex<Duration>>,
}

#[cfg(test)]
impl TestStraps {
    pub fn known(&self) -> Vec<Strap> {
        self.known.borrow().clone()
    }

    /// `pulse` measured `age` ago.
    pub fn set(&self, id: i64, link: Link, pulse: Option<u16>, age: Duration) {
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.reporter.update(id, |state| {
            state.link = link;
            state.pulse = pulse;
            state.pulse_at = pulse.map(|_| at);
        });
    }

    pub fn set_availability(&self, availability: Availability) {
        self.reporter.set_availability(availability);
    }

    pub fn report(&self, id: i64, link: Link) {
        self.reporter.report(id, link, None, None);
    }

    pub fn fail(&self, id: i64, reason: &str) -> bool {
        self.reporter.fail(id, reason)
    }

    pub fn set_scan(&self, result: Result<Vec<Found>, ScanError>, delay: Duration) {
        *self.scan_result.lock().unwrap() = result;
        *self.scan_delay.lock().unwrap() = delay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: i64, link: Link) -> StrapState {
        StrapState {
            id,
            name: format!("Gurt {id}"),
            address: format!("00:00:00:00:00:0{id}"),
            link,
            pulse: Some(100),
            battery: None,
            problem: None,
            pulse_at: None,
        }
    }

    fn strap(id: i64) -> Strap {
        Strap { id, address: format!("00:00:00:00:00:0{id}"), name: format!("Gurt {id}") }
    }

    #[test]
    fn mode_parses() {
        assert_eq!("".parse(), Ok(Mode::Auto));
        assert_eq!(" Auto ".parse(), Ok(Mode::Auto));
        assert_eq!("off".parse(), Ok(Mode::Off));
        assert_eq!("sim".parse(), Ok(Mode::Sim));
        assert!("bluetooth".parse::<Mode>().is_err());
    }

    #[test]
    fn choose_follows_spec() {
        use Link::*;
        let both = [state(1, Connected), state(2, Connected)];
        assert_eq!(choose_strap(Some(1), &[2], &both), Choice::Strap(1));
        // The own strap is not connected: no other strap stands in.
        assert_eq!(choose_strap(Some(1), &[], &[state(1, OutOfRange), state(2, Connected)]), Choice::None);
        assert_eq!(choose_strap(None, &[], &[state(1, Connected), state(2, Searching)]), Choice::Strap(1));
        assert_eq!(choose_strap(None, &[2], &both), Choice::Strap(1));
        assert_eq!(choose_strap(None, &[], &both), Choice::Ambiguous);
        assert_eq!(choose_strap(None, &[1, 2], &both), Choice::None);
        assert_eq!(choose_strap(None, &[], &[]), Choice::None);
    }

    #[tokio::test]
    async fn stale_pulse_is_dropped() {
        let (hub, test) = StrapHub::test();
        hub.set_known(vec![strap(1)]);
        test.set(1, Link::Connected, Some(90), Duration::from_secs(1));
        assert_eq!(hub.states()[0].pulse, Some(90));
        test.set(1, Link::Connected, Some(90), Duration::from_secs(6));
        assert_eq!(hub.states()[0].pulse, None);
        assert_eq!(hub.states()[0].link, Link::Connected);
    }

    #[test]
    fn off_hub_is_off() {
        let hub = StrapHub::spawn(Mode::Off, Monitor::new());
        assert_eq!(hub.availability(), Availability::Off);
        hub.set_known(vec![strap(1)]);
        assert!(hub.states().is_empty());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert_eq!(rt.block_on(hub.scan()), Err(ScanError::Unavailable));
    }

    #[tokio::test]
    async fn sim_scans_and_connects() {
        let monitor = Monitor::new();
        let hub = StrapHub::spawn(Mode::Sim, monitor.clone());
        assert_eq!(hub.availability(), Availability::Sim);
        let found = hub.scan().await.unwrap();
        assert_eq!(found[0].name.as_deref(), Some("Sim-Gurt"));
        hub.set_known(vec![Strap { id: 7, address: found[0].address.clone(), name: "Sim-Gurt".into() }]);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(s) = hub.states().first()
                    && s.link == Link::Connected
                    && s.pulse.is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("sim strap connects");
        let pulse = hub.states()[0].pulse.unwrap();
        assert!((70..=150).contains(&pulse), "{pulse}");
        assert!(monitor.history().iter().any(|e| e.kind == TraceKind::Link && e.text == "Gurt Sim-Gurt verbunden"));
    }

    #[tokio::test]
    async fn shutdown_lets_the_straps_go() {
        let hub = StrapHub::spawn(Mode::Sim, Monitor::new());
        hub.set_known(vec![strap(1)]);
        tokio::time::timeout(Duration::from_secs(3), async {
            while hub.states().first().is_none_or(|s| s.link != Link::Connected) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("sim strap connects");
        tokio::time::timeout(Duration::from_secs(1), hub.shutdown()).await.expect("shutdown returns");
        assert_eq!(hub.states()[0].link, Link::OutOfRange);
        // The backend is gone: nothing reconnects.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(hub.states()[0].link, Link::OutOfRange);
        // Off has no backend; shutting it down is a no-op.
        StrapHub::spawn(Mode::Off, Monitor::new()).shutdown().await;
    }

    #[tokio::test]
    async fn scan_busy() {
        let (hub, test) = StrapHub::test();
        test.set_scan(Ok(vec![]), Duration::from_millis(300));
        let first = tokio::spawn({
            let hub = hub.clone();
            async move { hub.scan().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.scan().await, Err(ScanError::Busy));
        assert_eq!(first.await.unwrap(), Ok(vec![]));
        // Free again afterwards.
        test.set_scan(Ok(vec![]), Duration::ZERO);
        assert_eq!(hub.scan().await, Ok(vec![]));
    }

    #[tokio::test]
    async fn known_list_reaches_backend_and_states() {
        let (hub, test) = StrapHub::test();
        hub.set_known(vec![strap(1), strap(2)]);
        assert_eq!(test.known().len(), 2);
        let states = hub.states();
        assert_eq!(states.iter().map(|s| (s.id, s.link)).collect::<Vec<_>>(), [(1, Link::Searching), (2, Link::Searching)]);
        test.set(2, Link::Connected, Some(80), Duration::ZERO);
        hub.set_known(vec![strap(2)]);
        assert_eq!(hub.states().iter().map(|s| (s.id, s.link, s.pulse)).collect::<Vec<_>>(), [(2, Link::Connected, Some(80))]);
    }

    #[tokio::test]
    async fn failure_stays_until_connected() {
        let (hub, test) = StrapHub::test();
        hub.set_known(vec![strap(1)]);
        assert_eq!(hub.states()[0].problem, None);
        assert!(test.fail(1, "le-connection-abort-by-local"));
        // The same reason again is no news.
        assert!(!test.fail(1, "le-connection-abort-by-local"));
        test.report(1, Link::Searching);
        assert_eq!(hub.states()[0].problem.as_deref(), Some("le-connection-abort-by-local"));
        assert!(test.fail(1, "Zeitüberschreitung beim Verbinden"));
        assert_eq!(hub.states()[0].problem.as_deref(), Some("Zeitüberschreitung beim Verbinden"));
        test.report(1, Link::Connected);
        assert_eq!(hub.states()[0].problem, None);
        // Unknown straps are ignored.
        assert!(!test.fail(9, "egal"));
    }
}
