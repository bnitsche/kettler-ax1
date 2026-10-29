//! RideManager: one event task that connects the device, the training session
//! and the storage, and publishes a `LiveView` for the web UI. Storage writes
//! go through a separate writer task so the event loop never waits on the DB
//! (except for `start_ride`, whose id `start` returns).
//!
//! Wiring: `RideManager::with_modem_keys(spawn_device(url, poll), db, modem_keys)`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::spawn_blocking;
use tokio::time::{Instant, MissedTickBehavior, sleep_until, timeout_at};

use crate::db::{BackupInfo, Db, Recovery, RestoreError, RideSummary};
use crate::device::{DeviceEvent, DeviceHandle};
use crate::elevation::{self, ElevationParams, ElevationPreview, ElevationTrack};
use crate::key_actions::{KeyAction, KeyActions, KeyInput};
use crate::program::{FlatSegment, Program};
use crate::protocol::{DeviceLimits, Status};
use crate::recovery::{RecoveryResult, RecoveryTracker, RecoveryView, STRAP_RECOVERY, StrapRecovery};
use crate::session::{Input, Output, Phase, Sample, Session, SessionView};
use crate::strap::{Choice, StrapHub, StrapState, choose_strap};

/// How long a finished session keeps enforcing the minimum load at most.
const TRAIL_LIMIT: Duration = Duration::from_secs(30);
/// A recovery measurement this soon after a ride's end still belongs to that ride.
const RECOVERY_AFTER_RIDE: Duration = Duration::from_secs(600);
/// Interval of `Input::Tick`.
const TICK: Duration = Duration::from_secs(1);
/// How long `shutdown` waits for storage and the device at most.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(3);
/// How long the result of a strap recovery measurement stays in the live view.
const STRAP_RESULT_SHOWN: Duration = Duration::from_secs(30);

#[derive(Serialize, Clone, Debug)]
pub struct LiveView {
    pub connected: bool,
    pub device_id: Option<String>,
    pub last: Option<Status>,
    /// Set while the cockpit runs its recovery measurement.
    pub recovery: Option<RecoveryView>,
    pub ride: Option<RideLive>,
    /// Set when the last storage write failed, cleared by the next successful one.
    pub storage_error: Option<String>,
    /// Shared by all clients; kept across rides.
    pub chart_view: ChartView,
    /// LEFT is held long enough that the ride stops if it stays held.
    pub stop_hold: bool,
    /// Whether the key channel is up (bridge key port connected, or own serial port open);
    /// says nothing about a keypad being plugged in.
    pub keypad: bool,
    /// Changes with every restore and differs between server runs; clients reload when it changes.
    pub data_epoch: u32,
    /// The pulse that counts: the strap's, else the cockpit's (not its recovery result).
    pub pulse: Option<u16>,
    /// The strap that counts now (see `choose_strap`).
    pub strap: Option<StrapState>,
    /// `"ambiguous"` while several unassigned straps are connected.
    pub strap_note: Option<&'static str>,
    /// The server's recovery measurement with the strap.
    pub strap_recovery: Option<RecoveryView>,
    /// A strap recovery measurement can be started now.
    pub recovery_available: bool,
}

/// Which chart the clients show, toggled by LEFT or `UiCommand::ToggleView`.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChartView {
    #[default]
    Normal,
    Big,
}

/// The history of a ride is loaded from storage; live views add one sample
/// per `session.ride_secs`.
#[derive(Serialize, Clone, Debug)]
pub struct RideLive {
    pub ride_id: i64,
    pub person_id: i64,
    pub program_name: Option<String>,
    pub segments: Option<Vec<FlatSegment>>,
    pub session: SessionView,
    /// Modelled height after the last sample, in metres.
    pub elevation_m: f64,
    pub elevation_gain_m: f64,
    /// The program's profile if ridden on target; `None` for a free ride.
    pub elevation_preview: Option<ElevationPreview>,
}

/// `ride_id`, if given, must match the running ride; otherwise the command is ignored.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiCommand {
    Adjust {
        delta: i16,
        #[serde(default)]
        ride_id: Option<i64>,
    },
    Pause {
        #[serde(default)]
        ride_id: Option<i64>,
    },
    Resume {
        #[serde(default)]
        ride_id: Option<i64>,
    },
    Stop {
        #[serde(default)]
        ride_id: Option<i64>,
    },
    ToggleView {
        #[serde(default)]
        ride_id: Option<i64>,
    },
    /// Starts a strap recovery measurement; also allowed shortly after a ride.
    RecoveryStart {
        #[serde(default)]
        ride_id: Option<i64>,
    },
    RecoveryCancel,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum StartError {
    #[error("Es läuft bereits eine Fahrt")]
    AlreadyRunning,
    #[error("Unbekannte Person")]
    UnknownPerson,
    #[error("Unbekanntes Programm")]
    UnknownProgram,
    /// Also returned if the event task is gone (see `stopped`).
    #[error("Speicherfehler: {0}")]
    Storage(String),
}

enum Msg {
    Start {
        person_id: i64,
        program_id: Option<i64>,
        reply: oneshot::Sender<Result<i64, StartError>>,
    },
    Command(UiCommand),
    Key(KeyInput),
    Delete {
        target: Target,
        reply: oneshot::Sender<Result<(), DeleteError>>,
    },
    Restore {
        src: PathBuf,
        reply: oneshot::Sender<Result<BackupInfo, RestoreError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum DeleteError {
    #[error("Fahrt läuft noch – zuerst beenden")]
    RideRunning,
    #[error("Für diese Person läuft gerade eine Fahrt")]
    PersonRiding,
    #[error("Unbekannte Fahrt")]
    UnknownRide,
    #[error("Unbekannte Person")]
    UnknownPerson,
    /// Also returned if the event task is gone (see `stopped`).
    #[error("Speicherfehler: {0}")]
    Storage(String),
}

#[derive(Clone, Copy)]
enum Target {
    Ride(i64),
    Person(i64),
}

/// Storage writes, applied in order by the writer task.
enum DbWrite {
    SetStarted { ride_id: i64, started_at: i64 },
    AddSample { ride_id: i64, sample: Sample },
    AddRecovery { ride_id: i64, recovery: Recovery },
    Finish { summary: RideSummary },
    /// A ride without samples is not kept.
    Delete { ride_id: i64 },
    /// Deletes on request of a user, answered with the outcome.
    Remove { target: Target, reply: oneshot::Sender<Result<(), DeleteError>> },
    /// Answered once all earlier writes are done.
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct RideManager {
    tx: mpsc::UnboundedSender<Msg>,
    view: watch::Receiver<LiveView>,
    straps: StrapHub,
}

impl RideManager {
    /// `with_modem_keys(device, db, true)`.
    #[cfg(test)]
    pub fn new(device: DeviceHandle, db: Db) -> Self {
        Self::with_modem_keys(device, db, true)
    }

    /// Starts the event and writer tasks. They end when all managers are dropped or
    /// after `shutdown`; the device handle is dropped with them. Without `modem_keys`
    /// the device's `DeviceEvent::Key`s are ignored, so only `key` feeds the keys.
    #[cfg(test)]
    pub fn with_modem_keys(device: DeviceHandle, db: Db, modem_keys: bool) -> Self {
        let straps = StrapHub::spawn(crate::strap::Mode::Off, device.monitor());
        Self::with_straps(device, db, modem_keys, straps)
    }

    /// As `with_modem_keys`, with the pulse and recovery measurement of chest straps.
    pub fn with_straps(device: DeviceHandle, db: Db, modem_keys: bool, straps: StrapHub) -> Self {
        Self::with_options(device, db, modem_keys, straps, STRAP_RECOVERY)
    }

    /// `strap_recovery`: duration of a strap recovery measurement (shorter in tests).
    pub fn with_options(
        device: DeviceHandle,
        db: Db,
        modem_keys: bool,
        straps: StrapHub,
        strap_recovery: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (writer, writes) = mpsc::unbounded_channel();
        let (results_tx, results) = mpsc::unbounded_channel();
        let initial = LiveView {
            connected: false,
            device_id: None,
            last: None,
            recovery: None,
            ride: None,
            storage_error: None,
            chart_view: ChartView::Normal,
            stop_hold: false,
            keypad: false,
            data_epoch: first_data_epoch(),
            pulse: None,
            strap: None,
            strap_note: None,
            strap_recovery: None,
            recovery_available: false,
        };
        let data_epoch = initial.data_epoch;
        let (view_tx, view) = watch::channel(initial);
        let events = device.subscribe();
        tokio::spawn(write_loop(db.clone(), writes, results_tx));
        let task = Task {
            device,
            db,
            writer,
            view: view_tx,
            connected: false,
            device_id: None,
            last: None,
            recovery: RecoveryTracker::default(),
            last_ride: None,
            ride: None,
            trailing: None,
            storage_error: None,
            modem_keys,
            keys: KeyActions::default(),
            keys_t0: Instant::now(),
            chart_view: ChartView::Normal,
            stop_hold: false,
            keypad: false,
            data_epoch,
            straps: straps.clone(),
            strap_recovery: StrapRecovery::with_duration(strap_recovery),
            strap_result_since: None,
            strap_target: None,
            cockpit_recovery: false,
            recovery_t0: Instant::now(),
        };
        tokio::spawn(task.run(rx, events, results));
        Self { tx, view, straps }
    }

    pub fn straps(&self) -> StrapHub {
        self.straps.clone()
    }

    /// Finishes the running ride (as Stop does), sets the minimum load and waits, at most
    /// `SHUTDOWN_LIMIT`, until storage is written and the device got the command. The
    /// manager is stopped afterwards: `start` fails and commands are ignored.
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown { reply }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Allowed without a device connection; the session then waits.
    pub async fn start(&self, person_id: i64, program_id: Option<i64>) -> Result<i64, StartError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Start { person_id, program_id, reply })
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Deletes a finished ride with its samples. Decided inside the event task and written
    /// after all pending writes, so it can't race with a start or a finish.
    pub async fn delete_ride(&self, ride_id: i64) -> Result<(), DeleteError> {
        self.delete(Target::Ride(ride_id)).await
    }

    /// Deletes a person with all rides and samples, unless one of their rides is running.
    pub async fn delete_person(&self, person_id: i64) -> Result<(), DeleteError> {
        self.delete(Target::Person(person_id)).await
    }

    async fn delete(&self, target: Target) -> Result<(), DeleteError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Msg::Delete { target, reply }).map_err(|_| deleting_stopped())?;
        rx.await.map_err(|_| deleting_stopped())?
    }

    /// Replaces all data with the backup at `src` (a scratch copy, see `Db::restore_from`).
    /// Refused while a ride runs; no ride can start until it is done.
    pub async fn restore(&self, src: PathBuf) -> Result<BackupInfo, RestoreError> {
        let stopped = || RestoreError::Storage("Fahrtverwaltung ist beendet".into());
        let (reply, rx) = oneshot::channel();
        self.tx.send(Msg::Restore { src, reply }).map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Ignored when no ride is running or `ride_id` doesn't match it.
    pub fn command(&self, cmd: UiCommand) {
        let _ = self.tx.send(Msg::Command(cmd));
    }

    /// A handlebar key input from the bridge; serial keys arrive as `DeviceEvent::Key`.
    pub fn key(&self, input: KeyInput) {
        let _ = self.tx.send(Msg::Key(input));
    }

    pub fn watch(&self) -> watch::Receiver<LiveView> {
        self.view.clone()
    }
}

/// The event task is gone: after `shutdown`, or once the device's event channel
/// closes, which can't happen while the task holds a handle; mapped to
/// `Storage` rather than a variant of its own.
fn stopped() -> StartError {
    StartError::Storage("Fahrtverwaltung ist beendet".into())
}

fn deleting_stopped() -> DeleteError {
    DeleteError::Storage("Fahrtverwaltung ist beendet".into())
}

/// Applies the writes in order and reports each outcome to `results`
/// (`None` = success, otherwise the error).
async fn write_loop(
    db: Db,
    mut writes: mpsc::UnboundedReceiver<DbWrite>,
    results: mpsc::UnboundedSender<Option<String>>,
) {
    while let Some(write) = writes.recv().await {
        let write = match write {
            DbWrite::Flush(done) => {
                let _ = done.send(());
                continue;
            }
            DbWrite::Remove { target, reply } => {
                let db = db.clone();
                let outcome = spawn_blocking(move || match target {
                    Target::Ride(id) => db.delete_ride(id).map(|found| (found, DeleteError::UnknownRide)),
                    Target::Person(id) => db.delete_person(id).map(|found| (found, DeleteError::UnknownPerson)),
                })
                .await;
                let outcome = match outcome {
                    Ok(Ok((true, _))) => Ok(()),
                    Ok(Ok((false, unknown))) => Err(unknown),
                    Ok(Err(e)) => Err(DeleteError::Storage(format!("{e:#}"))),
                    Err(e) => Err(DeleteError::Storage(e.to_string())),
                };
                let _ = reply.send(outcome);
                continue;
            }
            write => write,
        };
        let db = db.clone();
        let result = spawn_blocking(move || match write {
            DbWrite::SetStarted { ride_id, started_at } => {
                (ride_id, "setting start time", db.set_ride_started(ride_id, started_at))
            }
            DbWrite::AddSample { ride_id, sample } => (
                ride_id,
                "storing sample",
                db.add_sample(ride_id, &sample),
            ),
            DbWrite::AddRecovery { ride_id, recovery } => {
                (ride_id, "storing recovery", db.add_recovery(ride_id, &recovery))
            }
            DbWrite::Finish { summary } => {
                (summary.id, "finishing ride", db.finish_ride(summary.id, &summary))
            }
            DbWrite::Delete { ride_id } => (ride_id, "deleting empty ride", db.delete_ride(ride_id).map(|_| ())),
            DbWrite::Flush(_) | DbWrite::Remove { .. } => unreachable!("handled above"),
        })
        .await;
        let error = match result {
            Ok((_, _, Ok(()))) => None,
            Ok((ride_id, what, Err(e))) => {
                tracing::warn!(ride_id, "{what} failed: {e:#}");
                Some(format!("{e:#}"))
            }
            Err(e) => {
                tracing::warn!("storage write panicked: {e}");
                Some(e.to_string())
            }
        };
        let _ = results.send(error);
    }
}

/// Running aggregates over the samples, rounded like `db::summarize`. The pulse is the
/// effective one (strap, else cockpit); 0 means "no reading" and rpm 0 "not pedalling",
/// both are excluded.
#[derive(Default)]
struct Stats {
    count: u64,
    watts_sum: u64,
    max_watts: u16,
    pulse_count: u64,
    pulse_sum: u64,
    max_pulse: u16,
    rpm_count: u64,
    rpm_sum: u64,
    max_rpm: u16,
}

impl Stats {
    fn add(&mut self, sample: &Sample) {
        let s = &sample.status;
        self.count += 1;
        self.watts_sum += u64::from(s.actual_watts);
        self.max_watts = self.max_watts.max(s.actual_watts);
        if let Some(pulse) = sample.effective_pulse() {
            self.pulse_count += 1;
            self.pulse_sum += u64::from(pulse);
            self.max_pulse = self.max_pulse.max(pulse);
        }
        if s.rpm > 0 {
            self.rpm_count += 1;
            self.rpm_sum += u64::from(s.rpm);
            self.max_rpm = self.max_rpm.max(s.rpm);
        }
    }

    fn avg_watts(&self) -> u16 {
        rounded_avg(self.watts_sum, self.count).unwrap_or(0)
    }

    fn avg_pulse(&self) -> Option<u16> {
        rounded_avg(self.pulse_sum, self.pulse_count)
    }

    fn max_pulse(&self) -> Option<u16> {
        (self.pulse_count > 0).then_some(self.max_pulse)
    }

    fn avg_rpm(&self) -> Option<u16> {
        rounded_avg(self.rpm_sum, self.rpm_count)
    }

    fn max_rpm(&self) -> Option<u16> {
        (self.rpm_count > 0).then_some(self.max_rpm)
    }
}

fn rounded_avg(sum: u64, n: u64) -> Option<u16> {
    (n > 0).then(|| ((sum + n / 2) / n) as u16)
}

struct ActiveRide {
    ride_id: i64,
    person_id: i64,
    /// Time of Start, replaced by the time the session first became active.
    started_at: i64,
    active_seen: bool,
    program_name: Option<String>,
    segments: Option<Vec<FlatSegment>>,
    session: Session,
    /// Base for the session's `at`.
    t0: Instant,
    stats: Stats,
    /// The ride's snapshot of the person's weight and flat power.
    params: ElevationParams,
    elevation: ElevationTrack,
    preview: Option<ElevationPreview>,
    straps: StrapAssignment,
}

/// Which straps belong to whom, as at the start of a ride (later changes count from the
/// next ride on).
#[derive(Clone, Default)]
struct StrapAssignment {
    own: Option<i64>,
    others: Vec<i64>,
}

/// A finished session that keeps the minimum load up until the device confirms
/// it, `TRAIL_LIMIT` passes or a new ride starts.
struct Trailing {
    session: Session,
    t0: Instant,
    since: Instant,
}

/// The last ride that was kept, for a recovery measurement right after it.
struct LastRide {
    ride_id: i64,
    ride_secs: u32,
    ended: Instant,
    straps: StrapAssignment,
}

/// State owned by the event task; nothing is shared.
struct Task {
    device: DeviceHandle,
    db: Db,
    writer: mpsc::UnboundedSender<DbWrite>,
    view: watch::Sender<LiveView>,
    connected: bool,
    device_id: Option<String>,
    last: Option<Status>,
    recovery: RecoveryTracker,
    last_ride: Option<LastRide>,
    ride: Option<ActiveRide>,
    trailing: Option<Trailing>,
    storage_error: Option<String>,
    /// Whether `DeviceEvent::Key` is used (the keys come from the serial port).
    modem_keys: bool,
    keys: KeyActions,
    /// Base for the key actions' time.
    keys_t0: Instant,
    chart_view: ChartView,
    stop_hold: bool,
    keypad: bool,
    data_epoch: u32,
    straps: StrapHub,
    strap_recovery: StrapRecovery,
    /// Since when a strap measurement's result or failure is shown.
    strap_result_since: Option<Instant>,
    /// The ride a strap measurement belongs to and its straps, fixed at its start.
    strap_target: Option<(i64, StrapAssignment)>,
    /// The last status was in the cockpit's recovery mode.
    cockpit_recovery: bool,
    /// Base for the strap recovery measurement's time.
    recovery_t0: Instant,
}

impl Task {
    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        mut events: broadcast::Receiver<DeviceEvent>,
        mut results: mpsc::UnboundedReceiver<Option<String>>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut strap_states = self.straps.watch();
        loop {
            let key_deadline = self.keys.next_deadline().map(|d| self.keys_t0 + d);
            tokio::select! {
                // Messages and device events first: a queued key release must win
                // over a key deadline that expired at the same time.
                biased;
                msg = rx.recv() => match msg {
                    Some(Msg::Start { person_id, program_id, reply }) => {
                        let _ = reply.send(self.start(person_id, program_id).await);
                    }
                    Some(Msg::Command(cmd)) => self.command(cmd),
                    Some(Msg::Key(input)) => self.key(input),
                    Some(Msg::Delete { target, reply }) => self.delete(target, reply),
                    Some(Msg::Restore { src, reply }) => {
                        let _ = reply.send(self.restore(src).await);
                    }
                    Some(Msg::Shutdown { reply }) => {
                        self.shutdown().await;
                        let _ = reply.send(());
                        return;
                    }
                    None => return,
                },
                ev = events.recv() => match ev {
                    Ok(ev) => self.device_event(ev),
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "ride manager lagged behind device events");
                        // A key release may be among the missed events. Deliberately released
                        // for either key source: a bridge key held across a lag is let go too.
                        self.key_input(KeyInput::Connected(false));
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    self.feed(|t0| Input::Tick { at: t0.elapsed() });
                    self.strap_tick();
                }
                // A new strap state is published below.
                Ok(()) = strap_states.changed() => {}
                // The future is built even while disabled, hence the fallback.
                () = sleep_until(key_deadline.unwrap_or_else(Instant::now)), if key_deadline.is_some() => {
                    let actions = self.keys.tick(self.keys_t0.elapsed());
                    self.key_actions(actions);
                }
                Some(error) = results.recv() => self.storage_error = error,
            }
            self.publish();
        }
    }

    fn delete(&mut self, target: Target, reply: oneshot::Sender<Result<(), DeleteError>>) {
        let blocked = match (target, &self.ride) {
            (Target::Ride(id), Some(r)) if r.ride_id == id => Some(DeleteError::RideRunning),
            (Target::Person(id), Some(r)) if r.person_id == id => Some(DeleteError::PersonRiding),
            _ => None,
        };
        match blocked {
            Some(e) => {
                let _ = reply.send(Err(e));
            }
            // Queued behind pending samples and summaries.
            None => {
                // A later recovery measurement must not refer to a deleted ride.
                self.last_ride = None;
                if let Err(mpsc::error::SendError(DbWrite::Remove { reply, .. })) =
                    self.writer.send(DbWrite::Remove { target, reply })
                {
                    let _ = reply.send(Err(deleting_stopped()));
                }
            }
        }
    }

    /// Awaited inside the event loop, so no start or delete can interleave.
    async fn restore(&mut self, src: PathBuf) -> Result<BackupInfo, RestoreError> {
        if self.ride.is_some() {
            return Err(RestoreError::RideRunning);
        }
        self.flush().await;
        let db = self.db.clone();
        let info = spawn_blocking(move || db.restore_from(&src))
            .await
            .map_err(|e| RestoreError::Storage(e.to_string()))??;
        // Ride ids now belong to the restored data.
        self.last_ride = None;
        self.trailing = None;
        self.data_epoch = self.data_epoch.wrapping_add(1);
        tracing::info!(?info, "database restored");
        Ok(info)
    }

    /// Waits until all queued writes are done.
    async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.writer.send(DbWrite::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }

    /// See `RideManager::shutdown`.
    async fn shutdown(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_LIMIT;
        let min = DeviceLimits::default().min;
        tracing::info!("ride manager shutting down");
        if let Some(ride) = &mut self.ride {
            // Sends the minimum and queues the summary.
            let out = ride.session.handle(Input::Stop);
            self.apply(out);
        } else {
            self.device.set_power(min);
        }
        // Only statuses received from here on can prove that the minimum went out.
        let mut events = self.device.subscribe();
        self.trailing = None;
        self.publish();

        let (done, flushed) = oneshot::channel();
        let _ = self.writer.send(DbWrite::Flush(done));
        if timeout_at(deadline, flushed).await.is_err() {
            tracing::warn!("storage writes still pending at shutdown");
        }
        if !self.connected {
            return;
        }
        // The device sends a queued PW before its next ST, so the second status after
        // the request (or one already showing the minimum) proves it went out.
        let sent = timeout_at(deadline, async {
            let mut statuses = 0;
            loop {
                match events.recv().await {
                    Ok(DeviceEvent::Status(s)) => {
                        statuses += 1;
                        if s.target_watts == min || statuses >= 2 {
                            return true;
                        }
                    }
                    Ok(DeviceEvent::Disconnected { .. }) | Err(RecvError::Closed) => return false,
                    Ok(DeviceEvent::Connected { .. } | DeviceEvent::Key(_)) | Err(RecvError::Lagged(_)) => {}
                }
            }
        })
        .await;
        if sent != Ok(true) {
            tracing::warn!("minimum load not confirmed sent at shutdown");
        }
    }

    async fn start(&mut self, person_id: i64, program_id: Option<i64>) -> Result<i64, StartError> {
        if self.ride.is_some() {
            return Err(StartError::AlreadyRunning);
        }
        // A delete accepted just before must be done, or the person could vanish under the ride.
        self.flush().await;
        let db = self.db.clone();
        let started_at = unix_now();
        let (ride_id, program, params, straps) = spawn_blocking(move || {
            let storage = |e: anyhow::Error| StartError::Storage(e.to_string());
            let persons = db.list_persons().map_err(storage)?;
            let Some(person) = persons.iter().find(|p| p.id == person_id) else {
                return Err(StartError::UnknownPerson);
            };
            let straps = StrapAssignment {
                own: person.strap_id,
                others: persons.iter().filter(|p| p.id != person_id).filter_map(|p| p.strap_id).collect(),
            };
            let program = match program_id {
                None => None,
                Some(id) => {
                    Some(db.get_program(id).map_err(storage)?.ok_or(StartError::UnknownProgram)?)
                }
            };
            // The ride's live parameters are the very snapshot that was stored.
            let (ride_id, params) = db.start_ride(person_id, program.as_ref(), started_at).map_err(storage)?;
            Ok((ride_id, program, params, straps))
        })
        .await
        .map_err(|e| StartError::Storage(e.to_string()))??;

        // The new session takes over the power control.
        self.trailing = None;
        // A key held before the start must not act on the new ride.
        self.keys = KeyActions::default();
        let segments = program.as_ref().map(Program::expand);
        let preview = segments.as_deref().map(|s| elevation::preview(&params, s));
        let mut session = Session::new(segments.clone(), DeviceLimits::default());
        // The session only learns about the connection through `Input::Connected`.
        let connect = self.connected.then(|| session.handle(Input::Connected));
        tracing::info!(ride_id, person_id, "ride started");
        self.ride = Some(ActiveRide {
            ride_id,
            person_id,
            started_at,
            active_seen: false,
            program_name: program.map(|p| p.name),
            segments,
            session,
            t0: Instant::now(),
            stats: Stats::default(),
            params,
            elevation: ElevationTrack::default(),
            preview,
            straps,
        });
        if let Some(out) = connect {
            self.apply(out);
        }
        Ok(ride_id)
    }

    /// `ToggleView` maps to no session input.
    fn command(&mut self, cmd: UiCommand) {
        let (input, ride_id) = match cmd {
            UiCommand::RecoveryStart { ride_id } => {
                let running = self.ride.as_ref().map(|r| r.ride_id);
                if ride_id.is_some_and(|id| Some(id) != running) {
                    tracing::debug!(?cmd, ?running, "recovery start for another ride ignored");
                } else if self.recovery_available() {
                    self.start_strap_recovery();
                }
                return;
            }
            UiCommand::RecoveryCancel => {
                if self.strap_recovery.is_measuring() {
                    tracing::info!("strap recovery measurement cancelled");
                    self.strap_recovery.cancel();
                    self.release_strap_target();
                }
                return;
            }
            UiCommand::Adjust { delta, ride_id } => (Some(Input::Adjust(delta)), ride_id),
            UiCommand::Pause { ride_id } => (Some(Input::Pause), ride_id),
            UiCommand::Resume { ride_id } => (Some(Input::Resume), ride_id),
            UiCommand::Stop { ride_id } => (Some(Input::Stop), ride_id),
            UiCommand::ToggleView { ride_id } => (None, ride_id),
        };
        let Some(ride) = &mut self.ride else {
            tracing::debug!(?cmd, "command without a ride ignored");
            return;
        };
        if ride_id.is_some_and(|id| id != ride.ride_id) {
            tracing::debug!(?cmd, active = ride.ride_id, "command for another ride ignored");
            return;
        }
        match input {
            Some(input) => {
                let out = ride.session.handle(input);
                self.apply(out);
            }
            None if ride.session.phase() == Phase::Finished => {}
            None => self.toggle_view(),
        }
    }

    fn toggle_view(&mut self) {
        self.chart_view = match self.chart_view {
            ChartView::Normal => ChartView::Big,
            ChartView::Big => ChartView::Normal,
        };
    }

    /// Key inputs from the bridge and from the serial port end up here.
    fn key(&mut self, input: KeyInput) {
        if let KeyInput::Connected(connected) = input {
            self.keypad = connected;
        }
        self.key_input(input);
    }

    fn key_input(&mut self, input: KeyInput) {
        let actions = self.keys.handle(input, self.keys_t0.elapsed());
        self.key_actions(actions);
    }

    /// Applies key actions like the matching commands. Without a running ride they are
    /// dropped, except hiding the stop hint.
    fn key_actions(&mut self, actions: Vec<KeyAction>) {
        for action in actions {
            let phase = self.ride.as_ref().map(|r| r.session.phase()).filter(|&p| p != Phase::Finished);
            match (action, phase) {
                (KeyAction::StopHint(false), _) => self.stop_hold = false,
                (_, None) => tracing::debug!(?action, "key action without a ride ignored"),
                (KeyAction::StopHint(true), _) => self.stop_hold = true,
                (KeyAction::Adjust(delta), _) => self.command(UiCommand::Adjust { delta, ride_id: None }),
                (KeyAction::TogglePause, Some(Phase::Paused)) => self.command(UiCommand::Resume { ride_id: None }),
                (KeyAction::TogglePause, _) => self.command(UiCommand::Pause { ride_id: None }),
                (KeyAction::ToggleView, _) => self.toggle_view(),
                (KeyAction::Stop, _) => self.command(UiCommand::Stop { ride_id: None }),
            }
        }
    }

    fn device_event(&mut self, ev: DeviceEvent) {
        match ev {
            DeviceEvent::Key(input) if self.modem_keys => self.key(input),
            DeviceEvent::Key(_) => {}
            DeviceEvent::Connected { id } => {
                // A repeated `Connected` only brings the id.
                self.device_id = (!id.is_empty()).then_some(id);
                if !self.connected {
                    self.connect();
                }
            }
            DeviceEvent::Disconnected { .. } => {
                self.connected = false;
                self.device_id = None;
                self.last = None;
                self.recovery.reset();
                self.cockpit_recovery = false;
                self.feed(|_| Input::Disconnected);
            }
            DeviceEvent::Status(status) => {
                self.last = Some(status);
                if let Some(result) = self.recovery.update(&status) {
                    self.store_recovery(result);
                }
                // A status proves the connection even if `Connected` was missed (lag).
                if !self.connected {
                    self.connect();
                }
                let entered_recovery = status.recovery && !self.cockpit_recovery;
                self.cockpit_recovery = status.recovery;
                // The session sees the recovery status first, so it pauses without a
                // power command; the hold then keeps it paused.
                self.feed(|t0| Input::Status { status, at: t0.elapsed() });
                if entered_recovery && !self.strap_recovery.is_measuring() {
                    self.start_strap_recovery();
                }
                self.strap_tick();
            }
        }
    }

    /// The running ride, or the one that ended up to `RECOVERY_AFTER_RIDE` ago, with the
    /// straps as assigned at its start.
    fn recovery_ride(&self) -> Option<(i64, StrapAssignment)> {
        match (&self.ride, &self.last_ride) {
            (Some(ride), _) => Some((ride.ride_id, ride.straps.clone())),
            (None, Some(last)) if last.ended.elapsed() <= RECOVERY_AFTER_RIDE => {
                Some((last.ride_id, last.straps.clone()))
            }
            _ => None,
        }
    }

    /// The strap that counts now and how it was chosen.
    fn current_strap(&self) -> (Option<StrapState>, Choice) {
        let assignment = self.recovery_ride().map(|(_, straps)| straps).unwrap_or_default();
        self.strap_for(&assignment)
    }

    fn strap_for(&self, assignment: &StrapAssignment) -> (Option<StrapState>, Choice) {
        let states = self.straps.states();
        let choice = choose_strap(assignment.own, &assignment.others, &states);
        let strap = match choice {
            Choice::Strap(id) => states.into_iter().find(|s| s.id == id),
            Choice::None | Choice::Ambiguous => None,
        };
        (strap, choice)
    }

    fn strap_pulse(&self) -> Option<u16> {
        self.current_strap().0.and_then(|s| s.pulse)
    }

    /// A strap gives a pulse, none is measured yet, and there is a ride to store it with.
    fn recovery_available(&self) -> bool {
        self.recovery_ride().is_some() && !self.strap_recovery.is_measuring() && self.strap_pulse().is_some()
    }

    /// Starts the server's measurement with the strap's pulse as P1 and holds the ride.
    /// The measurement keeps its ride and straps, even if another ride starts meanwhile.
    fn start_strap_recovery(&mut self) {
        let Some((ride_id, straps)) = self.recovery_ride() else { return };
        let Some(p1) = self.strap_for(&straps).0.and_then(|s| s.pulse) else { return };
        tracing::info!(ride_id, p1, "strap recovery measurement started");
        self.strap_recovery.clear();
        self.strap_result_since = None;
        self.strap_recovery.start(self.recovery_t0.elapsed(), p1);
        self.strap_target = Some((ride_id, straps));
        self.hold(ride_id, true);
    }

    /// Releases the measured ride, if it still runs.
    fn release_strap_target(&mut self) {
        if let Some((ride_id, _)) = self.strap_target.take() {
            self.hold(ride_id, false);
        }
    }

    /// Follows the strap measurement: stores its result, releases the ride at its end and
    /// hides the result after `STRAP_RESULT_SHOWN`.
    fn strap_tick(&mut self) {
        let measuring = self.strap_recovery.is_measuring();
        if measuring {
            let Some((ride_id, straps)) = self.strap_target.clone() else { return };
            let pulse = self.strap_for(&straps).0.and_then(|s| s.pulse);
            if let Some(result) = self.strap_recovery.update(self.recovery_t0.elapsed(), pulse) {
                self.store_recovery_with(ride_id, result);
            }
            if !self.strap_recovery.is_measuring() {
                if self.strap_recovery.view() == Some(RecoveryView::Failed) {
                    tracing::info!("strap recovery measurement failed: no pulse at the end");
                }
                self.strap_result_since = Some(Instant::now());
                self.release_strap_target();
            }
        } else if self.strap_result_since.is_some_and(|since| since.elapsed() >= STRAP_RESULT_SHOWN) {
            self.strap_recovery.clear();
            self.strap_result_since = None;
        }
    }

    /// Holds or releases the running ride if it is `ride_id`.
    fn hold(&mut self, ride_id: i64, on: bool) {
        if let Some(ride) = self.ride.as_mut().filter(|r| r.ride_id == ride_id) {
            let out = ride.session.handle(Input::Hold { on, at: ride.t0.elapsed() });
            self.apply(out);
        }
    }

    /// Stores a recovery result with the running ride at its current time, or with the
    /// ride that ended up to `RECOVERY_AFTER_RIDE` ago at its end.
    fn store_recovery(&mut self, result: RecoveryResult) {
        match self.recovery_ride() {
            Some((ride_id, _)) => self.store_recovery_with(ride_id, result),
            None => tracing::info!(?result, "recovery measurement without a ride, not stored"),
        }
    }

    /// Stores a recovery result with `ride_id`: at its current time while it runs, at its
    /// end once it ended.
    fn store_recovery_with(&mut self, ride_id: i64, result: RecoveryResult) {
        let ride_secs = match (&self.ride, &self.last_ride) {
            (Some(ride), _) if ride.ride_id == ride_id => Some(ride.session.view().ride_secs),
            (_, Some(last)) if last.ride_id == ride_id => Some(last.ride_secs),
            _ => None,
        };
        let Some(ride_secs) = ride_secs else {
            tracing::info!(ride_id, ?result, "recovery measurement for a ride no longer at hand, not stored");
            return;
        };
        tracing::info!(ride_id, ride_secs, ?result, "recovery measurement stored");
        let recovery = Recovery {
            ride_secs,
            pulse_start: result.pulse_start,
            difference: result.difference,
            grade_x10: result.grade_x10,
            at_unix_ms: unix_now_ms(),
            source: result.source,
        };
        let _ = self.writer.send(DbWrite::AddRecovery { ride_id, recovery });
    }

    /// Without a session in charge, the device is set to the minimum: it may still hold
    /// a program load from before a restart.
    fn connect(&mut self) {
        self.connected = true;
        if self.ride.is_none() && self.trailing.is_none() {
            self.device.set_power(DeviceLimits::default().min);
        }
        self.feed(|_| Input::Connected);
    }

    /// Feeds a device input (built from the session's start time) to the trailing
    /// and the running session.
    fn feed(&mut self, input: impl Fn(Instant) -> Input) {
        if let Some(trail) = &mut self.trailing {
            if trail.since.elapsed() >= TRAIL_LIMIT {
                tracing::debug!("finished session stops enforcing the minimum (time limit)");
                self.trailing = None;
            } else {
                if let Some(watts) = trail.session.handle(input(trail.t0)).send_power {
                    self.device.set_power(watts);
                }
                if trail.session.min_confirmed() {
                    self.trailing = None;
                }
            }
        }
        if let Some(ride) = &mut self.ride {
            let out = ride.session.handle(input(ride.t0));
            self.apply(out);
        }
    }

    /// Sends power, records the real start, queues the sample and finishes the ride once
    /// the session is done.
    fn apply(&mut self, out: Output) {
        let strap_pulse = if out.sample.is_some() { self.strap_pulse() } else { None };
        let Some(ride) = &mut self.ride else { return };
        if let Some(watts) = out.send_power {
            self.device.set_power(watts);
        }
        if !ride.active_seen && ride.session.phase() == Phase::Active {
            ride.active_seen = true;
            ride.started_at = unix_now();
            let _ = self
                .writer
                .send(DbWrite::SetStarted { ride_id: ride.ride_id, started_at: ride.started_at });
        }
        if let Some(mut sample) = out.sample {
            sample.at_unix_ms = Some(unix_now_ms());
            sample.strap_pulse = strap_pulse;
            ride.stats.add(&sample);
            ride.elevation.step(&ride.params, f64::from(sample.status.actual_watts));
            let _ = self.writer.send(DbWrite::AddSample { ride_id: ride.ride_id, sample });
        }
        if ride.session.is_finished() {
            self.finish();
        }
    }

    /// Queues the summary (after the ride's samples), or the removal of a ride without
    /// samples, and moves the session to trailing.
    fn finish(&mut self) {
        let Some(ride) = self.ride.take() else { return };
        // A key still held must not act on the next ride.
        self.keys = KeyActions::default();
        self.stop_hold = false;
        let view = ride.session.view();
        if ride.stats.count == 0 {
            tracing::info!(ride_id = ride.ride_id, "ride without samples discarded");
            let _ = self.writer.send(DbWrite::Delete { ride_id: ride.ride_id });
        } else {
            let summary = RideSummary {
                id: ride.ride_id,
                person_id: ride.person_id,
                program_name: ride.program_name,
                started_at: ride.started_at,
                ride_secs: view.ride_secs,
                avg_watts: ride.stats.avg_watts(),
                max_watts: ride.stats.max_watts,
                avg_pulse: ride.stats.avg_pulse(),
                max_pulse: ride.stats.max_pulse(),
                distance_m: view.distance_m,
                energy_kj: view.energy_kj,
                finished: true,
                ended_at: Some(unix_now()),
                avg_rpm: ride.stats.avg_rpm(),
                max_rpm: ride.stats.max_rpm(),
                // `finish_ride` keeps the stored snapshot and recomputes the gain from the
                // samples; the live gain is its fallback.
                weight_kg: ride.params.weight_kg,
                flat_watts: ride.params.flat_watts,
                elevation_gain_m: ride.elevation.gain_m(),
            };
            tracing::info!(ride_id = ride.ride_id, secs = view.ride_secs, "ride finished");
            let _ = self.writer.send(DbWrite::Finish { summary });
            self.last_ride = Some(LastRide {
                ride_id: ride.ride_id,
                ride_secs: view.ride_secs,
                ended: Instant::now(),
                straps: ride.straps.clone(),
            });
        }
        self.trailing = Some(Trailing { session: ride.session, t0: ride.t0, since: Instant::now() });
    }

    fn publish(&self) {
        let ride = self.ride.as_ref().map(|r| RideLive {
            ride_id: r.ride_id,
            person_id: r.person_id,
            program_name: r.program_name.clone(),
            segments: r.segments.clone(),
            session: r.session.view(),
            elevation_m: r.elevation.height_m(),
            elevation_gain_m: r.elevation.gain_m(),
            elevation_preview: r.preview.clone(),
        });
        let (strap, choice) = self.current_strap();
        // At the end of its recovery measurement the cockpit's pulse field holds the drop.
        let cockpit = self.last.filter(|l| l.pulse > 0 && !(l.recovery && l.elapsed_secs == 0)).map(|l| l.pulse);
        let pulse = strap.as_ref().and_then(|s| s.pulse).or(cockpit);
        self.view.send_replace(LiveView {
            connected: self.connected,
            device_id: self.device_id.clone(),
            last: self.last,
            recovery: self.recovery.view(),
            ride,
            storage_error: self.storage_error.clone(),
            chart_view: self.chart_view,
            stop_hold: self.stop_hold,
            keypad: self.keypad,
            data_epoch: self.data_epoch,
            pulse,
            strap,
            strap_note: (choice == Choice::Ambiguous).then_some("ambiguous"),
            strap_recovery: self.strap_recovery.view(),
            recovery_available: self.recovery_available(),
        });
    }
}

/// A start value unlike the one of an earlier server run or another manager.
fn first_data_epoch() -> u32 {
    static MANAGERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let n = MANAGERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    nanos ^ std::process::id().rotate_left(16) ^ n.wrapping_mul(0x9E37_79B9)
}

fn unix_now() -> i64 {
    unix_now_ms() / 1000
}

fn unix_now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    use super::*;
    use crate::db;
    use crate::device::spawn_device;
    use crate::key_actions::STOP_AFTER;
    use crate::keypad::{Key, KeyEvent};
    use crate::program::{Block, Program, Step};
    use crate::protocol::LineReader;
    use crate::session::Phase;
    use crate::strap::TestStraps;
    use crate::transport::DeviceUrl;

    const POLL: Duration = Duration::from_millis(50);
    const WAIT: Duration = Duration::from_secs(5);

    fn manager(url: DeviceUrl, db: &Db) -> (RideManager, i64) {
        let person = db.add_person("Anna").unwrap();
        (RideManager::new(spawn_device(url, POLL), db.clone()), person)
    }

    fn setup(url: DeviceUrl) -> (RideManager, Db, i64) {
        let db = db::open_in_memory().unwrap();
        let (rides, person) = manager(url, &db);
        (rides, db, person)
    }

    fn short_program(db: &Db) -> i64 {
        db.save_program(&Program {
            id: None,
            name: "Kurz".into(),
            blocks: vec![Block {
                repeat: 1,
                steps: vec![Step {
                    name: "Los".into(),
                    duration_secs: 2,
                    start_watts: 100,
                    end_watts: 100,
                }],
            }],
        })
        .unwrap()
    }

    /// Waits until `f` holds for the current view.
    async fn wait_view(
        rx: &mut watch::Receiver<LiveView>,
        limit: Duration,
        mut f: impl FnMut(&LiveView) -> bool,
    ) -> LiveView {
        timeout(limit, async {
            loop {
                {
                    let view = rx.borrow_and_update();
                    if f(&view) {
                        return view.clone();
                    }
                }
                rx.changed().await.expect("ride manager stopped");
            }
        })
        .await
        .expect("timed out")
    }

    /// Polls until `f` holds; storage writes land asynchronously.
    async fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) {
        timeout(limit, async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("timed out")
    }

    async fn wait_finished(db: &Db, person: i64) -> RideSummary {
        wait_until(WAIT, || db.list_rides(person).unwrap().first().is_some_and(|r| r.finished))
            .await;
        db.list_rides(person).unwrap().remove(0)
    }

    async fn wait_active(rx: &mut watch::Receiver<LiveView>) -> RideLive {
        wait_view(rx, WAIT, |v| {
            v.ride.as_ref().is_some_and(|r| r.session.phase == Phase::Active)
        })
        .await
        .ride
        .unwrap()
    }

    /// Scriptable TCP ergometer: answers CM/ID/ST/PW unless `silent`; ignores PW
    /// values while `ignore_pw` and records every PW.
    struct Fake {
        silent: bool,
        ignore_pw: bool,
        target: u16,
        pw: Vec<u16>,
        /// `ST` requests answered after the first PW.
        st_after_pw: u32,
        /// Time field while the cockpit measures recovery (`mm;ss`).
        recovery: Option<&'static str>,
        pulse: u16,
    }

    impl Fake {
        fn reply(&mut self, line: &str) -> Option<String> {
            if self.silent {
                return None;
            }
            match line.trim() {
                "CM" => Some("ACK\r\n".into()),
                "ID" => Some("FAKE\r\n".into()),
                "ST" => {
                    if !self.pw.is_empty() {
                        self.st_after_pw += 1;
                    }
                    Some(self.status())
                }
                other => {
                    let watts: u16 = other.strip_prefix("PW ")?.trim().parse().ok()?;
                    self.pw.push(watts);
                    if !self.ignore_pw {
                        self.target = watts;
                    }
                    Some(self.status())
                }
            }
        }

        fn status(&self) -> String {
            let time = self.recovery.unwrap_or("00:13");
            format!("{:03}\t070\t245\t001\t{:03}\t0004\t{time}\t{:03}\r\n", self.pulse, self.target, self.target)
        }
    }

    async fn fake_device(silent: bool) -> (DeviceUrl, Arc<Mutex<Fake>>) {
        let fake = Arc::new(Mutex::new(Fake { silent, ignore_pw: false, target: 25, pw: Vec::new(), st_after_pw: 0, recovery: None, pulse: 0 }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = fake.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let mut reader = LineReader::new();
                    let mut buf = [0u8; 256];
                    loop {
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        for line in reader.push(&buf[..n]) {
                            let reply = state.lock().unwrap().reply(&line);
                            if let Some(reply) = reply
                                && stream.write_all(reply.as_bytes()).await.is_err()
                            {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (DeviceUrl::Tcp(addr.to_string()), fake)
    }

    #[tokio::test]
    async fn ride_records_and_finishes() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let program = short_program(&db);
        let mut rx = rides.watch();
        let ride_id = timeout(WAIT, rides.start(person, Some(program))).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert_eq!(view.ride.unwrap().segments.unwrap().len(), 1);
        wait_view(&mut rx, Duration::from_secs(10), |v| v.ride.is_none()).await;

        let summary = wait_finished(&db, person).await;
        assert_eq!(db.list_rides(person).unwrap().len(), 1);
        assert_eq!(summary.id, ride_id);
        assert_eq!(summary.program_name.as_deref(), Some("Kurz"));
        assert_eq!(summary.ride_secs, 2);
        let ended_at = summary.ended_at.expect("end time");
        assert!(summary.started_at <= ended_at && ended_at - summary.started_at <= 5, "{summary:?}");
        let detail = db.get_ride(ride_id).unwrap().unwrap();
        assert!(detail.samples.len() >= 2, "{:?}", detail.samples);
        for s in &detail.samples {
            let ms = s.at_unix_ms.expect("wall clock");
            assert!(ms / 1000 >= summary.started_at && ms / 1000 <= ended_at, "{s:?}");
        }
        // Running sums give the same figures as summarizing the stored samples.
        let snapshot = ElevationParams { weight_kg: summary.weight_kg, flat_watts: summary.flat_watts };
        let stats = db::summarize(&detail.samples, &snapshot);
        assert!(stats.max_watts > 0, "{stats:?}");
        assert_eq!(summary.elevation_gain_m, stats.elevation_gain_m);
        assert_eq!(summary.avg_watts, stats.avg_watts);
        assert_eq!(summary.max_watts, stats.max_watts);
        assert_eq!(summary.avg_pulse, stats.avg_pulse);
        assert_eq!(summary.max_pulse, stats.max_pulse);
        assert_eq!(summary.avg_rpm, stats.avg_rpm);
        assert_eq!(summary.max_rpm, stats.max_rpm);
    }

    #[test]
    fn running_stats_match_summarize() {
        let samples: Vec<Sample> = [(100u16, 0u16, 0u16), (151, 120, 71), (90, 131, 90), (80, 0, 0)]
            .iter()
            .enumerate()
            .map(|(i, &(watts, pulse, rpm))| Sample {
                ride_secs: i as u32 + 1,
                status: Status {
                    pulse,
                    rpm,
                    speed_x10: 245,
                    distance: 0,
                    target_watts: 100,
                    energy: 0,
                    elapsed_secs: 0,
                    actual_watts: watts,
                    recovery: false,
                },
                target_watts: 100,
                at_unix_ms: None,
                // The last second has only the strap's pulse, the third both.
                strap_pulse: [None, None, Some(128), Some(140)][i],
            })
            .collect();
        let mut stats = Stats::default();
        for s in &samples {
            stats.add(s);
        }
        assert_eq!(db::summarize(&samples, &ElevationParams::DEFAULT).max_pulse, Some(140));
        let expected = db::summarize(&samples, &ElevationParams::DEFAULT);
        assert_eq!(stats.avg_watts(), expected.avg_watts);
        assert_eq!(stats.max_watts, expected.max_watts);
        assert_eq!(stats.avg_pulse(), expected.avg_pulse);
        assert_eq!(stats.max_pulse(), expected.max_pulse);
        // Zero rpm is skipped: (71 + 90) / 2 = 80.5, rounded up.
        assert_eq!(stats.avg_rpm(), Some(81));
        assert_eq!(stats.max_rpm(), Some(90));
        assert_eq!(stats.avg_rpm(), expected.avg_rpm);
        assert_eq!(stats.max_rpm(), expected.max_rpm);
        let empty = Stats::default();
        assert_eq!((empty.avg_watts(), empty.avg_pulse(), empty.max_pulse()), (0, None, None));
        assert_eq!((empty.avg_rpm(), empty.max_rpm()), (None, None));
    }

    #[tokio::test]
    async fn failing_sample_write_keeps_ride_going() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let db = db::open(&path).unwrap();
        rusqlite::Connection::open(&path).unwrap().execute_batch("DROP TABLE samples").unwrap();
        let program = short_program(&db);
        let (rides, person) = manager(DeviceUrl::Sim, &db);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, Some(program))).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.storage_error.is_some()).await;
        assert!(view.ride.is_some());
        // The successful finish clears the error.
        wait_view(&mut rx, Duration::from_secs(10), |v| v.ride.is_none() && v.storage_error.is_none())
            .await;
        let summary = wait_finished(&db, person).await;
        assert_eq!(summary.ride_secs, 2);
        assert!(summary.max_watts > 0, "{summary:?}");
    }

    #[tokio::test]
    async fn ride_without_samples_is_deleted() {
        let (rides, db, person) = setup(DeviceUrl::Tcp("127.0.0.1:1".into()));
        let mut rx = rides.watch();
        let id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        assert!(db.get_ride(id).unwrap().is_some());
        rides.command(UiCommand::Stop { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.ride.is_none()).await;
        wait_until(WAIT, || db.get_ride(id).unwrap().is_none()).await;
    }

    #[tokio::test]
    async fn silent_device_auto_pauses_ride() {
        let (url, fake) = fake_device(false).await;
        let (rides, _db, person) = setup(url);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        fake.lock().unwrap().silent = true;
        let view = wait_view(&mut rx, Duration::from_secs(8), |v| {
            v.ride.as_ref().is_some_and(|r| r.session.phase == Phase::AutoPaused)
        })
        .await;
        assert!(view.ride.is_some());
    }

    #[tokio::test]
    async fn shutdown_finishes_ride_and_sets_min() {
        let (url, fake) = fake_device(false).await;
        let (rides, db, person) = setup(url);
        let mut rx = rides.watch();
        let id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_view(&mut rx, WAIT, |v| {
            v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 1)
                && v.last.is_some_and(|s| s.target_watts == 50)
        })
        .await;
        fake.lock().unwrap().pw.clear();
        timeout(WAIT, rides.shutdown()).await.expect("shutdown hangs");
        // Storage and device are done when `shutdown` returns.
        let summary = db.get_ride(id).unwrap().unwrap().summary;
        assert!(summary.finished, "{summary:?}");
        assert!(summary.ended_at.is_some());
        assert_eq!(fake.lock().unwrap().pw, vec![25]);
        assert!(timeout(WAIT, rides.start(person, None)).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn shutdown_counts_only_statuses_after_the_minimum_was_queued() {
        let (url, fake) = fake_device(false).await;
        // The device keeps reporting 100 W and never shows the minimum, so only the rule
        // "second status after the request" can confirm, and it must count fresh statuses.
        {
            let mut f = fake.lock().unwrap();
            f.ignore_pw = true;
            f.target = 100;
        }
        let (rides, _db, person) = setup(url);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_view(&mut rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 1)).await;
        let started = Instant::now();
        timeout(WAIT, rides.shutdown()).await.expect("shutdown hangs");
        assert!(started.elapsed() < SHUTDOWN_LIMIT, "{:?}", started.elapsed());
        let fake = fake.lock().unwrap();
        assert!(fake.pw.contains(&25));
        // Reply to the PW is the first fresh status, the next ST answer the second one.
        assert!(fake.st_after_pw >= 1, "{}", fake.st_after_pw);
    }

    #[tokio::test]
    async fn deletes_go_through_the_manager() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let other = db.add_person("Bob").unwrap();
        let old = db.start_ride(other, None, 10).unwrap().0;
        db.add_sample(old, &crate::session::Sample {
            ride_secs: 1,
            status: Status { pulse: 0, rpm: 1, speed_x10: 1, distance: 0, target_watts: 50, energy: 0, elapsed_secs: 1, actual_watts: 50, recovery: false },
            target_watts: 50,
            at_unix_ms: None,
            strap_pulse: None,
        })
        .unwrap();
        let id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        assert_eq!(rides.delete_ride(id).await, Err(DeleteError::RideRunning));
        assert_eq!(rides.delete_person(person).await, Err(DeleteError::PersonRiding));
        assert_eq!(rides.delete_ride(999).await, Err(DeleteError::UnknownRide));
        assert_eq!(rides.delete_person(999).await, Err(DeleteError::UnknownPerson));
        assert_eq!(rides.delete_ride(old).await, Ok(()));
        assert!(db.get_ride(old).unwrap().is_none());
        assert_eq!(rides.delete_person(other).await, Ok(()));
        // A person deleted just before can't be started any more.
        let r = timeout(WAIT, rides.start(other, None)).await.unwrap();
        assert!(matches!(r, Err(StartError::UnknownPerson) | Err(StartError::AlreadyRunning)), "{r:?}");
    }

    #[tokio::test]
    async fn delete_after_finish_lands_after_pending_writes() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let program = short_program(&db);
        let mut rx = rides.watch();
        let id = timeout(WAIT, rides.start(person, Some(program))).await.unwrap().unwrap();
        wait_view(&mut rx, Duration::from_secs(10), |v| v.ride.is_some()).await;
        wait_view(&mut rx, Duration::from_secs(10), |v| v.ride.is_none()).await;
        // The summary may still be queued; the delete waits behind it.
        assert_eq!(rides.delete_person(person).await, Ok(()));
        assert!(db.get_ride(id).unwrap().is_none());
        assert!(db.list_persons().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(rx.borrow().storage_error, None);
    }

    #[tokio::test]
    async fn second_start_is_rejected() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let second = timeout(WAIT, rides.start(person, None)).await.unwrap();
        assert!(matches!(second, Err(StartError::AlreadyRunning)), "{second:?}");
    }

    #[tokio::test]
    async fn unknown_person_and_program_are_rejected() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let r = timeout(WAIT, rides.start(person + 100, None)).await.unwrap();
        assert!(matches!(r, Err(StartError::UnknownPerson)), "{r:?}");
        let r = timeout(WAIT, rides.start(person, Some(999))).await.unwrap();
        assert!(matches!(r, Err(StartError::UnknownProgram)), "{r:?}");
        assert_eq!(StartError::AlreadyRunning.to_string(), "Es läuft bereits eine Fahrt");
        assert_eq!(StartError::Storage("x".into()).to_string(), "Speicherfehler: x");
    }

    #[tokio::test]
    async fn start_without_device_waits() {
        let (rides, _db, person) = setup(DeviceUrl::Tcp("127.0.0.1:1".into()));
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert!(!view.connected);
        let ride = view.ride.unwrap();
        assert!(!ride.session.connected);
        assert_eq!(ride.session.phase, Phase::Waiting);
    }

    #[tokio::test]
    async fn start_while_connected_marks_session_connected() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.connected).await;
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert!(view.ride.unwrap().session.connected);
    }

    #[tokio::test]
    async fn device_connecting_later_reaches_session() {
        let (url, fake) = fake_device(true).await;
        let (rides, _db, person) = setup(url);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert!(!view.ride.unwrap().session.connected);
        fake.lock().unwrap().silent = false;
        // The id may follow in a second `Connected` (when ST answered before CM).
        let view = wait_view(&mut rx, WAIT, |v| {
            v.ride.as_ref().is_some_and(|r| r.session.connected) && v.device_id.is_some()
        })
        .await;
        assert!(view.connected);
        assert_eq!(view.device_id.as_deref(), Some("FAKE"));
    }

    #[tokio::test]
    async fn idle_connect_sends_min_once() {
        let (url, fake) = fake_device(false).await;
        fake.lock().unwrap().target = 80;
        let (rides, _db, _person) = setup(url);
        let mut rx = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.last.is_some_and(|s| s.target_watts == 25)).await;
        // A few more polls: no further PW.
        for _ in 0..3 {
            rx.changed().await.unwrap();
        }
        assert_eq!(fake.lock().unwrap().pw, vec![25]);
    }

    #[tokio::test]
    async fn repeated_connected_only_updates_id() {
        let (url, fake) = fake_device(false).await;
        let device = spawn_device(url, POLL);
        let db = db::open_in_memory().unwrap();
        let person = db.add_person("Anna").unwrap();
        let rides = RideManager::new(device.clone(), db);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        wait_view(&mut rx, WAIT, |v| v.last.is_some_and(|s| s.target_watts == 50)).await;
        fake.lock().unwrap().pw.clear();
        device.announce(DeviceEvent::Connected { id: "AX1S".into() });
        wait_view(&mut rx, WAIT, |v| v.device_id.as_deref() == Some("AX1S")).await;
        for _ in 0..3 {
            rx.changed().await.unwrap();
        }
        assert_eq!(fake.lock().unwrap().pw, Vec::<u16>::new());
    }

    #[tokio::test]
    async fn idle_view_shows_device_state() {
        let (rides, _db, _person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        let view = wait_view(&mut rx, WAIT, |v| v.last.is_some()).await;
        assert!(view.connected);
        assert_eq!(view.device_id.as_deref(), Some("AX1S"));
        assert!(view.ride.is_none());
    }

    #[tokio::test]
    async fn recovery_measurement_keeps_connection() {
        let (url, fake) = fake_device(false).await;
        let (rides, _db, _person) = setup(url);
        let mut rx = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.connected && v.last.is_some()).await;
        fake.lock().unwrap().recovery = Some("00;42");
        let view = wait_view(&mut rx, WAIT, |v| v.recovery.is_some()).await;
        assert_eq!(
            view.recovery,
            Some(RecoveryView::Measuring { remaining_secs: 42, pulse_start: None, pulse: None })
        );
        // Longer than three failed status cycles.
        let gone = timeout(Duration::from_secs(5), wait_view(&mut rx, Duration::from_secs(60), |v| !v.connected)).await;
        assert!(gone.is_err(), "disconnected during recovery");
        fake.lock().unwrap().recovery = None;
        wait_view(&mut rx, WAIT, |v| v.recovery.is_none()).await;
    }

    /// Lets the fake run a recovery measurement: countdown with P1 106, then difference 6.
    async fn measure_recovery(fake: &Arc<Mutex<Fake>>, rx: &mut watch::Receiver<LiveView>) {
        {
            let mut f = fake.lock().unwrap();
            f.recovery = Some("00;30");
            f.pulse = 106;
        }
        wait_view(rx, WAIT, |v| matches!(v.recovery, Some(RecoveryView::Measuring { .. }))).await;
        {
            let mut f = fake.lock().unwrap();
            f.recovery = Some("00;00");
            f.pulse = 6;
        }
        wait_view(rx, WAIT, |v| matches!(v.recovery, Some(RecoveryView::Result { .. }))).await;
        {
            let mut f = fake.lock().unwrap();
            f.recovery = None;
            f.pulse = 0;
        }
        wait_view(rx, WAIT, |v| v.recovery.is_none()).await;
    }

    #[tokio::test]
    async fn recovery_is_stored_with_running_and_just_finished_ride() {
        let (url, fake) = fake_device(false).await;
        let (rides, db, person) = setup(url);
        let mut rx = rides.watch();
        let ride_id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_view(&mut rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 2)).await;
        measure_recovery(&fake, &mut rx).await;
        let during = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await.ride.unwrap().session.ride_secs;
        rides.command(UiCommand::Stop { ride_id: None });
        let summary = wait_finished(&db, person).await;
        measure_recovery(&fake, &mut rx).await;
        let stored = |db: &Db| db.get_ride(ride_id).unwrap().unwrap().recoveries;
        wait_until(WAIT, || stored(&db).len() == 2).await;
        let r = stored(&db);
        assert!(r[0].ride_secs >= 2 && r[0].ride_secs <= during, "{r:?}");
        assert_eq!(r[1].ride_secs, summary.ride_secs);
        for x in &r {
            assert_eq!((x.pulse_start, x.difference, x.grade_x10), (106, 6, 57));
        }
    }

    #[tokio::test]
    async fn stop_finishes_ride() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        // At least one sample, so the ride is kept.
        wait_view(&mut rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 1)).await;
        rides.command(UiCommand::Stop { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.ride.is_none()).await;
        wait_finished(&db, person).await;
        assert_eq!(db.list_rides(person).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn finished_session_keeps_enforcing_min() {
        let (url, fake) = fake_device(false).await;
        let (rides, _db, person) = setup(url);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        // Free ride: 50 W, confirmed by the device.
        wait_view(&mut rx, WAIT, |v| v.last.is_some_and(|s| s.target_watts == 50)).await;
        {
            let mut f = fake.lock().unwrap();
            f.ignore_pw = true;
            f.pw.clear();
        }
        rides.command(UiCommand::Stop { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.ride.is_none()).await;
        // The device keeps 50 W; the finished session resends the minimum.
        wait_until(WAIT, || fake.lock().unwrap().pw.iter().filter(|&&w| w == 25).count() >= 2)
            .await;
        // A new ride takes over at once.
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn two_watchers_see_same_state() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut a = rides.watch();
        let mut b = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let target = wait_active(&mut a).await.session.target_watts + 10;
        rides.command(UiCommand::Adjust { delta: 10, ride_id: None });
        let has_target =
            |v: &LiveView| v.ride.as_ref().is_some_and(|r| r.session.target_watts == target);
        wait_view(&mut a, WAIT, has_target).await;
        wait_view(&mut b, WAIT, has_target).await;
    }

    #[tokio::test]
    async fn command_for_other_ride_is_ignored() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        let id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let base = wait_active(&mut rx).await.session.target_watts;
        rides.command(UiCommand::Adjust { delta: 10, ride_id: Some(id + 1) });
        rides.command(UiCommand::Stop { ride_id: Some(id + 1) });
        rides.command(UiCommand::Adjust { delta: 20, ride_id: Some(id) });
        // Commands are handled in order: only the matching one took effect.
        let view = wait_view(&mut rx, WAIT, |v| {
            v.ride.as_ref().is_some_and(|r| r.session.target_watts != base)
        })
        .await;
        let ride = view.ride.unwrap();
        assert_eq!(ride.session.target_watts, base + 20);
        assert_eq!(ride.session.phase, Phase::Active);
    }

    fn press(k: Key) -> KeyInput {
        KeyInput::Event(KeyEvent::Press(k))
    }

    fn release(k: Key) -> KeyInput {
        KeyInput::Event(KeyEvent::Release(k))
    }

    fn phase_is(p: Phase) -> impl FnMut(&LiveView) -> bool {
        move |v| v.ride.as_ref().is_some_and(|r| r.session.phase == p)
    }

    /// `Connected(true)` is handled after all earlier keys, so its echo proves they were handled.
    async fn keys_handled(rides: &RideManager, rx: &mut watch::Receiver<LiveView>) -> LiveView {
        rides.key(KeyInput::Connected(false));
        wait_view(rx, WAIT, |v| !v.keypad).await;
        rides.key(KeyInput::Connected(true));
        wait_view(rx, WAIT, |v| v.keypad).await
    }

    #[tokio::test]
    async fn keys_adjust_and_pause_like_commands() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let base = wait_active(&mut rx).await.session.target_watts;
        rides.key(press(Key::Up));
        rides.key(release(Key::Up));
        wait_view(&mut rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.target_watts == base + 10)).await;
        rides.key(press(Key::Right));
        rides.key(release(Key::Right));
        wait_view(&mut rx, WAIT, phase_is(Phase::Paused)).await;
        rides.key(press(Key::Right));
        rides.key(release(Key::Right));
        wait_view(&mut rx, WAIT, phase_is(Phase::Active)).await;
    }

    #[tokio::test]
    async fn left_hold_stops_ride() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_view(&mut rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 1)).await;
        rides.key(press(Key::Left));
        wait_view(&mut rx, Duration::from_secs(1), |v| v.stop_hold).await;
        let view = wait_view(&mut rx, Duration::from_secs(3), |v| v.ride.is_none()).await;
        assert!(!view.stop_hold);
        wait_finished(&db, person).await;
        rides.key(release(Key::Left));
        assert_eq!(keys_handled(&rides, &mut rx).await.chart_view, ChartView::Normal);
    }

    #[tokio::test]
    async fn toggle_view_is_shared_and_kept_across_rides() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        let id = timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        rides.command(UiCommand::ToggleView { ride_id: Some(id + 1) });
        rides.command(UiCommand::ToggleView { ride_id: None });
        let mut other = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.chart_view == ChartView::Big).await;
        wait_view(&mut other, WAIT, |v| v.chart_view == ChartView::Big).await;
        rides.command(UiCommand::Stop { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.ride.is_none()).await;
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert_eq!(view.chart_view, ChartView::Big);
        rides.key(press(Key::Left));
        rides.key(release(Key::Left));
        wait_view(&mut rx, WAIT, |v| v.chart_view == ChartView::Normal).await;
    }

    #[tokio::test]
    async fn keys_without_ride_are_ignored() {
        let (url, fake) = fake_device(false).await;
        // 25 W then proves the idle minimum went out.
        fake.lock().unwrap().target = 80;
        let (rides, _db, _person) = setup(url);
        let mut rx = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.last.is_some_and(|s| s.target_watts == 25)).await;
        fake.lock().unwrap().pw.clear();
        // Held past the first repeat.
        rides.key(press(Key::Up));
        tokio::time::sleep(Duration::from_millis(700)).await;
        rides.key(release(Key::Up));
        rides.key(press(Key::Left));
        rides.key(release(Key::Left));
        rides.command(UiCommand::ToggleView { ride_id: None });
        let view = keys_handled(&rides, &mut rx).await;
        assert_eq!(view.chart_view, ChartView::Normal);
        // A few more polls: no PW went out.
        for _ in 0..3 {
            rx.changed().await.unwrap();
        }
        assert_eq!(fake.lock().unwrap().pw, Vec::<u16>::new());
    }

    #[tokio::test]
    async fn keypad_flag_follows_connection() {
        let (rides, _db, _person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        assert!(!rx.borrow().keypad);
        rides.key(KeyInput::Connected(true));
        wait_view(&mut rx, WAIT, |v| v.keypad).await;
        rides.key(KeyInput::Connected(false));
        wait_view(&mut rx, WAIT, |v| !v.keypad).await;
    }

    #[tokio::test]
    async fn device_keys_reach_the_ride_and_lag_releases_them() {
        let device = spawn_device(DeviceUrl::Sim, POLL);
        let db = db::open_in_memory().unwrap();
        let person = db.add_person("Bob").unwrap();
        let rides = RideManager::new(device.clone(), db);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        device.announce(DeviceEvent::Key(press(Key::Left)));
        wait_view(&mut rx, Duration::from_secs(1), |v| v.stop_hold).await;
        // Overflow the device channel without yielding: the manager lags and may have
        // missed the release, so it lets go of LEFT instead of stopping the ride.
        for _ in 0..100 {
            device.announce(DeviceEvent::Key(KeyInput::Connected(true)));
        }
        wait_view(&mut rx, WAIT, |v| !v.stop_hold).await;
        tokio::time::sleep(STOP_AFTER).await;
        let view = wait_view(&mut rx, WAIT, |_| true).await;
        assert_eq!(view.ride.map(|r| r.session.phase), Some(Phase::Active));
    }

    #[tokio::test]
    async fn key_held_before_start_does_not_act() {
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        wait_view(&mut rx, WAIT, |v| v.connected).await;
        let pressed = Instant::now();
        rides.key(press(Key::Left));
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        tokio::time::sleep_until(pressed + STOP_AFTER + Duration::from_millis(500)).await;
        let view = wait_view(&mut rx, WAIT, |_| true).await;
        assert!(!view.stop_hold);
        assert_eq!(view.ride.map(|r| r.session.phase), Some(Phase::Active));
    }

    #[tokio::test]
    async fn modem_keys_disabled_are_ignored() {
        let device = spawn_device(DeviceUrl::Sim, POLL);
        let db = db::open_in_memory().unwrap();
        let person = db.add_person("Alice").unwrap();
        let rides = RideManager::with_modem_keys(device.clone(), db, false);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let base = wait_active(&mut rx).await.session.target_watts;
        device.announce(DeviceEvent::Key(KeyInput::Connected(true)));
        device.announce(DeviceEvent::Key(press(Key::Up)));
        device.announce(DeviceEvent::Key(release(Key::Up)));
        // Same channel, so this marker is handled after the keys.
        device.announce(DeviceEvent::Connected { id: "MARK".into() });
        let view = wait_view(&mut rx, WAIT, |v| v.device_id.as_deref() == Some("MARK")).await;
        assert!(!view.keypad);
        assert_eq!(view.ride.unwrap().session.target_watts, base);
    }

    #[test]
    fn ui_command_json() {
        let cmd: UiCommand = serde_json::from_str(r#"{"type":"toggle_view"}"#).unwrap();
        assert_eq!(cmd, UiCommand::ToggleView { ride_id: None });
        let cmd: UiCommand = serde_json::from_str(r#"{"type":"adjust","delta":-5}"#).unwrap();
        assert_eq!(cmd, UiCommand::Adjust { delta: -5, ride_id: None });
        let cmd: UiCommand = serde_json::from_str(r#"{"type":"stop"}"#).unwrap();
        assert_eq!(cmd, UiCommand::Stop { ride_id: None });
        let cmd: UiCommand = serde_json::from_str(r#"{"type":"pause","ride_id":7}"#).unwrap();
        assert_eq!(cmd, UiCommand::Pause { ride_id: Some(7) });
    }

    fn set_profile(db: &Db, person: i64, weight_kg: f64, flat_watts: u16) {
        let u = db::PersonUpdate { weight_kg: Some(weight_kg), flat_watts: Some(flat_watts), ..Default::default() };
        assert!(db.update_person(person, &u).unwrap());
    }

    /// A fake ergometer that always reports `watts`.
    async fn fake_at(watts: u16) -> (DeviceUrl, Arc<Mutex<Fake>>) {
        let (url, fake) = fake_device(false).await;
        {
            let mut f = fake.lock().unwrap();
            f.target = watts;
            f.ignore_pw = true;
        }
        (url, fake)
    }

    #[tokio::test]
    async fn live_elevation_follows_samples() {
        let (url, _fake) = fake_at(170).await;
        let (rides, db, person) = setup(url);
        set_profile(&db, person, 85.0, 70);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, Duration::from_secs(10), |v| {
            v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 3)
        })
        .await;
        let ride = view.ride.unwrap();
        // (170 - 70) W / ((85 + 10) kg * 9.81) per second.
        let expected = f64::from(ride.session.ride_secs) * 0.1073;
        assert!((ride.elevation_m - expected).abs() < 0.01, "{} vs {expected}", ride.elevation_m);
    }

    #[tokio::test]
    async fn program_ride_has_preview() {
        let (rides, db, person) = setup(DeviceUrl::Sim);
        set_profile(&db, person, 85.0, 70);
        let four = db.list_programs().unwrap().into_iter().find(|p| p.name == "4 × 4").unwrap();
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, four.id)).await.unwrap().unwrap();
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        let preview = view.ride.unwrap().elevation_preview.expect("preview");
        assert!((120.0..=122.0).contains(&preview.gain_m), "{}", preview.gain_m);
    }

    #[tokio::test]
    async fn free_ride_has_live_elevation_without_preview() {
        let (url, _fake) = fake_at(170).await;
        let (rides, _db, person) = setup(url);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let first = wait_active(&mut rx).await;
        assert!(first.elevation_preview.is_none());
        let later = wait_view(&mut rx, Duration::from_secs(10), |v| {
            v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= first.session.ride_secs + 2)
        })
        .await
        .ride
        .unwrap();
        assert!(later.elevation_preview.is_none());
        assert!(later.elevation_m > first.elevation_m, "{} vs {}", later.elevation_m, first.elevation_m);
    }

    #[tokio::test]
    async fn live_and_stored_gain_match() {
        // About 1 m per second, so the gain passes the hysteresis threshold quickly.
        let (url, _fake) = fake_at(400).await;
        let (rides, db, person) = setup(url);
        set_profile(&db, person, 30.0, 0);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        let mut live = std::collections::HashMap::new();
        wait_view(&mut rx, Duration::from_secs(10), |v| {
            if let Some(r) = &v.ride {
                live.insert(r.session.ride_secs, r.elevation_gain_m);
            }
            v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 3)
        })
        .await;
        rides.command(UiCommand::Stop { ride_id: None });
        wait_view(&mut rx, WAIT, |v| {
            if let Some(r) = &v.ride {
                live.insert(r.session.ride_secs, r.elevation_gain_m);
            }
            v.ride.is_none()
        })
        .await;
        let summary = wait_finished(&db, person).await;
        let detail = db.get_ride(summary.id).unwrap().unwrap();
        let params = ElevationParams { weight_kg: 30.0, flat_watts: 0 };
        let expected = elevation::heights(&params, detail.samples.iter().map(|s| f64::from(s.status.actual_watts))).1;
        assert!(expected > 1.0, "{expected}");
        assert!((summary.elevation_gain_m - expected).abs() <= 1e-9, "{} vs {expected}", summary.elevation_gain_m);
        // The watch channel coalesces views, so the final second may not have been observed.
        if let Some(last) = live.get(&summary.ride_secs) {
            assert!((summary.elevation_gain_m - last).abs() <= 0.01, "{} vs {last}", summary.elevation_gain_m);
        }
    }

    /// A backup file in `dir` that holds only Alice, with id 2 (the setup's person has id 1).
    fn alice_backup(dir: &std::path::Path) -> std::path::PathBuf {
        let source = db::open(&dir.join("source.db")).unwrap();
        let bob = source.add_person("Bob").unwrap();
        source.add_person("Alice").unwrap();
        source.delete_person(bob).unwrap();
        let path = dir.join("backup.db");
        source.backup_to(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn restore_refused_while_ride_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (rides, db, person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        timeout(WAIT, rides.start(person, None)).await.unwrap().unwrap();
        wait_active(&mut rx).await;
        let epoch = rx.borrow().data_epoch;
        let result = timeout(WAIT, rides.restore(alice_backup(dir.path()))).await.unwrap();
        assert!(matches!(result, Err(RestoreError::RideRunning)), "{result:?}");
        assert_eq!(rx.borrow().data_epoch, epoch);
        assert_eq!(db.list_persons().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restore_replaces_data_and_bumps_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let (rides, db, _person) = setup(DeviceUrl::Sim);
        let mut rx = rides.watch();
        let epoch = rx.borrow().data_epoch;
        let info = timeout(WAIT, rides.restore(alice_backup(dir.path()))).await.unwrap().unwrap();
        assert_eq!(info.persons, 1);
        let names: Vec<_> = db.list_persons().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Alice"]);
        wait_view(&mut rx, WAIT, |v| v.data_epoch != epoch).await;
    }

    #[tokio::test]
    async fn data_epoch_differs_per_manager() {
        // A restarted server must not look like one that saw no restore.
        let (a, _, _) = setup(DeviceUrl::Sim);
        let (b, _, _) = setup(DeviceUrl::Sim);
        assert_ne!(a.watch().borrow().data_epoch, b.watch().borrow().data_epoch);
    }

    #[tokio::test]
    async fn start_after_restore_uses_new_data() {
        let dir = tempfile::tempdir().unwrap();
        let (rides, _db, person) = setup(DeviceUrl::Sim);
        timeout(WAIT, rides.restore(alice_backup(dir.path()))).await.unwrap().unwrap();
        assert_eq!(timeout(WAIT, rides.start(person, None)).await.unwrap(), Err(StartError::UnknownPerson));
        assert!(timeout(WAIT, rides.start(2, None)).await.unwrap().is_ok());
    }

    // ---- Chest strap ------------------------------------------------------------------

    /// A ride manager on a scriptable ergometer with a test strap hub. The strap with id
    /// `strap` is registered and, while `pulse` holds a value, connected and sending it.
    struct StrapRig {
        rides: RideManager,
        db: Db,
        person: i64,
        strap: i64,
        fake: Arc<Mutex<Fake>>,
        test: Arc<TestStraps>,
        pulse: Arc<Mutex<Option<u16>>>,
    }

    const TEST_STRAP_RECOVERY: Duration = Duration::from_secs(2);

    async fn strap_rig() -> StrapRig {
        let (url, fake) = fake_device(false).await;
        let db = db::open_in_memory().unwrap();
        let person = db.add_person("Bob").unwrap();
        let strap = db.add_strap("00:00:00:00:00:01", "HRM-Dual").unwrap();
        let (hub, test) = StrapHub::test();
        hub.set_known(db.list_straps().unwrap());
        let rides = RideManager::with_options(spawn_device(url, POLL), db.clone(), true, hub, TEST_STRAP_RECOVERY);
        let test = Arc::new(test);
        let pulse = Arc::new(Mutex::new(Some(120)));
        tokio::spawn(feed_strap(test.clone(), strap, pulse.clone()));
        StrapRig { rides, db, person, strap, fake, test, pulse }
    }

    /// Keeps the strap's state fresh, like a strap sending once a second.
    async fn feed_strap(test: Arc<TestStraps>, id: i64, pulse: Arc<Mutex<Option<u16>>>) {
        loop {
            let value = *pulse.lock().unwrap();
            test.set(id, crate::strap::Link::Connected, value, Duration::ZERO);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn ride_running(rig: &StrapRig, rx: &mut watch::Receiver<LiveView>) -> i64 {
        let ride_id = timeout(WAIT, rig.rides.start(rig.person, None)).await.unwrap().unwrap();
        wait_view(rx, WAIT, |v| v.ride.as_ref().is_some_and(|r| r.session.ride_secs >= 2)).await;
        ride_id
    }

    fn recoveries(db: &Db, ride_id: i64) -> Vec<Recovery> {
        db.get_ride(ride_id).unwrap().unwrap().recoveries
    }

    #[tokio::test]
    async fn samples_carry_strap_pulse() {
        let rig = strap_rig().await;
        *rig.pulse.lock().unwrap() = Some(95);
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        let view = wait_view(&mut rx, WAIT, |v| v.pulse == Some(95)).await;
        assert_eq!(view.strap.map(|s| s.id), Some(rig.strap));
        rig.rides.command(UiCommand::Stop { ride_id: None });
        let summary = wait_finished(&rig.db, rig.person).await;
        let samples = rig.db.get_ride(ride_id).unwrap().unwrap().samples;
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|s| s.strap_pulse == Some(95)), "{samples:?}");
        assert_eq!((summary.avg_pulse, summary.max_pulse), (Some(95), Some(95)));
    }

    #[tokio::test]
    async fn stale_strap_falls_back_to_cockpit() {
        let rig = strap_rig().await;
        // The feeder would refresh it; a stale value alone stays stale.
        *rig.pulse.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_millis(300)).await;
        rig.test.set(rig.strap, crate::strap::Link::Connected, Some(130), Duration::from_secs(6));
        rig.fake.lock().unwrap().pulse = 80;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        let view = wait_view(&mut rx, WAIT, |v| v.pulse.is_some()).await;
        assert_eq!(view.pulse, Some(80));
        rig.rides.command(UiCommand::Stop { ride_id: None });
        wait_finished(&rig.db, rig.person).await;
        let samples = rig.db.get_ride(ride_id).unwrap().unwrap().samples;
        assert!(samples.iter().all(|s| s.strap_pulse.is_none() && s.status.pulse == 80), "{samples:?}");
    }

    #[tokio::test]
    async fn own_strap_wins_and_others_are_skipped() {
        let rig = strap_rig().await;
        let other = rig.db.add_strap("00:00:00:00:00:02", "Zweiter").unwrap();
        let alice = rig.db.add_person("Alice").unwrap();
        assign_strap(&rig.db, alice, rig.strap);
        assign_strap(&rig.db, rig.person, other);
        rig.rides_hub_known(&rig.db);
        tokio::spawn(feed_strap(rig.test.clone(), other, Arc::new(Mutex::new(Some(88)))));
        let mut rx = rig.rides.watch();
        ride_running(&rig, &mut rx).await;
        let view = wait_view(&mut rx, WAIT, |v| v.pulse == Some(88)).await;
        assert_eq!(view.strap.map(|s| s.id), Some(other));
        assert_eq!(view.strap_note, None);
    }

    #[tokio::test]
    async fn two_free_straps_are_ambiguous() {
        let rig = strap_rig().await;
        let other = rig.db.add_strap("00:00:00:00:00:02", "Zweiter").unwrap();
        rig.rides_hub_known(&rig.db);
        tokio::spawn(feed_strap(rig.test.clone(), other, Arc::new(Mutex::new(Some(88)))));
        rig.fake.lock().unwrap().pulse = 77;
        let mut rx = rig.rides.watch();
        ride_running(&rig, &mut rx).await;
        let view = wait_view(&mut rx, WAIT, |v| v.strap_note.is_some()).await;
        assert_eq!(view.strap_note, Some("ambiguous"));
        assert!(view.strap.is_none());
        assert_eq!(view.pulse, Some(77));
    }

    #[tokio::test]
    async fn cockpit_key_starts_strap_measurement_and_holds_ride() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        assert!(rx.borrow().recovery_available);
        // The cockpit waits for a pulse it never gets and gives up; the rider pedals on.
        rig.fake.lock().unwrap().recovery = Some("00;00");
        let view = wait_view(&mut rx, WAIT, |v| matches!(v.strap_recovery, Some(RecoveryView::Measuring { .. }))).await;
        assert_eq!(
            view.strap_recovery,
            Some(RecoveryView::Measuring { remaining_secs: 2, pulse_start: Some(120), pulse: Some(120) })
        );
        assert!(!view.recovery_available);
        tokio::time::sleep(Duration::from_millis(400)).await;
        rig.fake.lock().unwrap().recovery = None;
        let held = wait_view(&mut rx, WAIT, |v| v.last.is_some_and(|l| !l.recovery)).await;
        let session = held.ride.unwrap().session;
        assert!(session.held);
        assert_eq!(session.phase, Phase::AutoPaused);
        *rig.pulse.lock().unwrap() = Some(100);
        wait_view(&mut rx, WAIT, |v| matches!(v.strap_recovery, Some(RecoveryView::Result { .. }))).await;
        wait_until(WAIT, || recoveries(&rig.db, ride_id).len() == 1).await;
        let r = &recoveries(&rig.db, ride_id)[0];
        assert_eq!((r.source, r.pulse_start, r.difference), (db::RecoverySource::Strap, 120, 20));
        // Released: pedalling continues the ride.
        let active = wait_active(&mut rx).await;
        assert!(!active.session.held);
    }

    #[tokio::test]
    async fn button_starts_and_cancel_releases() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        rig.rides.command(UiCommand::RecoveryStart { ride_id: Some(ride_id + 1) });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(rx.borrow().strap_recovery.is_none(), "other ride id is ignored");
        rig.rides.command(UiCommand::RecoveryStart { ride_id: Some(ride_id) });
        let view = wait_view(&mut rx, WAIT, |v| v.strap_recovery.is_some()).await;
        assert!(view.ride.unwrap().session.held);
        rig.rides.command(UiCommand::RecoveryCancel);
        let view = wait_view(&mut rx, WAIT, |v| v.strap_recovery.is_none()).await;
        assert!(!view.ride.unwrap().session.held);
        wait_active(&mut rx).await;
        tokio::time::sleep(TEST_STRAP_RECOVERY + Duration::from_millis(500)).await;
        assert!(recoveries(&rig.db, ride_id).is_empty());
    }

    #[tokio::test]
    async fn stop_during_measurement_stores_with_ended_ride() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        rig.rides.command(UiCommand::RecoveryStart { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.strap_recovery.is_some()).await;
        rig.rides.command(UiCommand::Stop { ride_id: None });
        let summary = wait_finished(&rig.db, rig.person).await;
        wait_until(WAIT, || recoveries(&rig.db, ride_id).len() == 1).await;
        let r = &recoveries(&rig.db, ride_id)[0];
        assert_eq!((r.source, r.ride_secs), (db::RecoverySource::Strap, summary.ride_secs));
    }

    #[tokio::test]
    async fn button_after_the_ride_measures_too() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        rig.rides.command(UiCommand::Stop { ride_id: None });
        wait_finished(&rig.db, rig.person).await;
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_none() && v.recovery_available).await;
        assert!(view.recovery_available);
        rig.rides.command(UiCommand::RecoveryStart { ride_id: None });
        wait_until(WAIT, || recoveries(&rig.db, ride_id).len() == 1).await;
    }

    #[tokio::test]
    async fn measurement_stays_with_its_ride_when_the_next_starts() {
        let rig = strap_rig().await;
        let other = rig.db.add_strap("00:00:00:00:00:02", "Zweiter").unwrap();
        let alice = rig.db.add_person("Alice").unwrap();
        assign_strap(&rig.db, rig.person, rig.strap);
        assign_strap(&rig.db, alice, other);
        rig.rides_hub_known(&rig.db);
        tokio::spawn(feed_strap(rig.test.clone(), other, Arc::new(Mutex::new(Some(88)))));
        let mut rx = rig.rides.watch();
        let bob_ride = ride_running(&rig, &mut rx).await;
        rig.rides.command(UiCommand::Stop { ride_id: None });
        wait_finished(&rig.db, rig.person).await;
        wait_view(&mut rx, WAIT, |v| v.ride.is_none() && v.recovery_available).await;
        rig.rides.command(UiCommand::RecoveryStart { ride_id: None });
        wait_view(&mut rx, WAIT, |v| matches!(v.strap_recovery, Some(RecoveryView::Measuring { .. }))).await;
        // Alice gets on before Bob's measurement ends.
        let alice_ride = timeout(WAIT, rig.rides.start(alice, None)).await.unwrap().unwrap();
        *rig.pulse.lock().unwrap() = Some(100);
        let view = wait_view(&mut rx, WAIT, |v| v.ride.is_some()).await;
        assert!(!view.ride.unwrap().session.held, "Alice's ride is not held");
        wait_until(WAIT, || recoveries(&rig.db, bob_ride).len() == 1).await;
        let r = &recoveries(&rig.db, bob_ride)[0];
        assert_eq!((r.source, r.pulse_start, r.difference), (db::RecoverySource::Strap, 120, 20));
        assert!(recoveries(&rig.db, alice_ride).is_empty());
    }

    #[tokio::test]
    async fn lost_strap_fails_measurement() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        rig.rides.command(UiCommand::RecoveryStart { ride_id: None });
        wait_view(&mut rx, WAIT, |v| v.strap_recovery.is_some()).await;
        *rig.pulse.lock().unwrap() = None;
        let view = wait_view(&mut rx, WAIT, |v| v.strap_recovery == Some(RecoveryView::Failed)).await;
        assert!(!view.ride.unwrap().session.held);
        assert!(recoveries(&rig.db, ride_id).is_empty());
    }

    #[tokio::test]
    async fn cockpit_and_strap_both_store() {
        let rig = strap_rig().await;
        let mut rx = rig.rides.watch();
        let ride_id = ride_running(&rig, &mut rx).await;
        measure_recovery(&rig.fake, &mut rx).await;
        wait_until(WAIT, || recoveries(&rig.db, ride_id).len() == 2).await;
        let mut sources: Vec<_> = recoveries(&rig.db, ride_id).iter().map(|r| r.source).collect();
        sources.sort_by_key(|s| *s == db::RecoverySource::Strap);
        assert_eq!(sources, [db::RecoverySource::Cockpit, db::RecoverySource::Strap]);
    }

    impl StrapRig {
        fn rides_hub_known(&self, db: &Db) {
            self.rides.straps().set_known(db.list_straps().unwrap());
        }
    }

    fn assign_strap(db: &Db, person: i64, strap: i64) {
        let update = db::PersonUpdate { strap_id: Some(Some(strap)), ..Default::default() };
        assert!(db.update_person(person, &update).unwrap());
    }
}
