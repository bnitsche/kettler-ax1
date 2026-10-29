//! The BlueZ backend of the strap hub (Linux only): talks to `bluetoothd` over D-Bus,
//! connects to the known straps and follows their heart rate notifications.
//!
//! Everything here runs in its own tasks; errors end up as a strap's link state or as
//! the hub's availability, never in a panic.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures_util::{StreamExt, pin_mut};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

use crate::db::Strap;
use crate::hrm::{BATTERY_LEVEL, BATTERY_SERVICE, HEART_RATE_MEASUREMENT, HEART_RATE_SERVICE, parse_measurement};
use crate::strap::{Availability, Backend, Found, Link, Reporter, SCAN_TIME, ScanError, STALE_AFTER};

/// A new attempt to reach BlueZ after it failed.
const RETRY_BLUEZ: Duration = Duration::from_secs(30);
/// Pause between connection attempts to a strap.
const RETRY_STRAP: Duration = Duration::from_secs(5);
/// How long a strap BlueZ doesn't know yet is searched for per attempt.
const FIND_TIME: Duration = Duration::from_secs(10);
const CONNECT_TIME: Duration = Duration::from_secs(20);
/// The GATT services of a fresh connection must be there by then.
const RESOLVE_TIME: Duration = Duration::from_secs(10);
/// A connected strap that sends nothing this long counts as gone.
const SILENT_TIME: Duration = Duration::from_secs(10);
/// How often a working adapter is checked.
const CHECK_ADAPTER: Duration = Duration::from_secs(30);
/// Letting a strap go must not hold up a removal or the shutdown.
const DISCONNECT_TIME: Duration = Duration::from_secs(2);
const STOPPED: &str = "Server wird beendet";

/// A Bluetooth base UUID with a 16-bit short id.
fn uuid16(short: u16) -> Uuid {
    Uuid::from_u128((u128::from(short) << 96) | 0x0000_1000_8000_0080_5F9B_34FB)
}

pub async fn run(mut backend: Backend) {
    while !*backend.stop.borrow() {
        match open().await {
            Ok(adapter) => {
                tracing::info!(adapter = adapter.name(), "bluetooth ready");
                backend.reporter.set_availability(Availability::Ready);
                let reason = serve(&adapter, &mut backend).await;
                tracing::warn!("bluetooth stopped: {reason}");
                backend.reporter.set_availability(Availability::Unavailable { reason });
            }
            Err(e) => {
                let reason = unavailable_reason(&e);
                tracing::warn!("bluetooth not available: {e}");
                backend.reporter.set_availability(Availability::Unavailable { reason });
            }
        }
        for strap in backend.known.borrow().iter() {
            backend.reporter.report(strap.id, Link::OutOfRange, None, None);
        }
        // Searches fail while BlueZ is out of reach.
        let until = Instant::now() + RETRY_BLUEZ;
        loop {
            tokio::select! {
                request = backend.scans.recv() => match request {
                    Some(reply) => { let _ = reply.send(Err(ScanError::Unavailable)); }
                    None => return,
                },
                _ = backend.stop.changed() => return,
                () = tokio::time::sleep_until(until) => break,
            }
        }
    }
}

/// One connection task per known strap; on `stop` it lets its strap go and ends.
struct Worker {
    strap: Strap,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Worker {
    fn spawn(adapter: &Adapter, strap: &Strap, reporter: &Reporter) -> Worker {
        let (stop, stopped) = oneshot::channel();
        let (adapter, own, reporter) = (adapter.clone(), strap.clone(), reporter.clone());
        let task = tokio::spawn(async move {
            // Ending `keep_connected` at its current await means it sends nothing more
            // to BlueZ, so the disconnect below is the last word.
            tokio::select! {
                () = keep_connected(&adapter, &own, &reporter) => {}
                _ = stopped => {}
            }
            release(&adapter, &own.address).await;
        });
        Worker { strap: strap.clone(), stop, task }
    }
}

/// Lets the strap go, so a phone can have it.
async fn release(adapter: &Adapter, address: &str) {
    if let Ok(addr) = address.parse::<Address>()
        && let Ok(device) = adapter.device(addr)
    {
        let _ = timeout(DISCONNECT_TIME, device.disconnect()).await;
    }
}

fn unavailable_reason(e: &bluer::Error) -> String {
    match e.kind {
        bluer::ErrorKind::Internal(_) => format!("kein Zugang zu BlueZ (D-Bus-Socket eingebunden?): {e}"),
        _ => format!("kein Bluetooth-Adapter: {e}"),
    }
}

async fn open() -> bluer::Result<Adapter> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    if !adapter.is_powered().await? {
        adapter.set_powered(true).await?;
    }
    adapter
        .set_discovery_filter(DiscoveryFilter {
            uuids: HashSet::from([uuid16(HEART_RATE_SERVICE)]),
            transport: DiscoveryTransport::Le,
            ..Default::default()
        })
        .await?;
    Ok(adapter)
}

/// Keeps one connection task per known strap until the adapter fails; returns why.
async fn serve(adapter: &Adapter, backend: &mut Backend) -> String {
    let mut workers: HashMap<i64, Worker> = HashMap::new();
    let mut check = tokio::time::interval(CHECK_ADAPTER);
    let reason = loop {
        let known = backend.known.borrow_and_update().clone();
        reconcile(adapter, &backend.reporter, &mut workers, &known);
        tokio::select! {
            changed = backend.known.changed() => if changed.is_err() {
                break "Gurt-Verwaltung beendet".to_string();
            },
            request = backend.scans.recv() => match request {
                Some(reply) => {
                    let adapter = adapter.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(scan(&adapter).await);
                    });
                }
                None => break "Gurt-Verwaltung beendet".to_string(),
            },
            _ = check.tick() => match adapter.is_powered().await {
                Err(e) => break format!("Adapter nicht mehr erreichbar: {e}"),
                // `open` switches it on again at the next attempt.
                Ok(false) => break "Bluetooth-Adapter ist ausgeschaltet".to_string(),
                Ok(true) => {}
            },
            _ = backend.stop.changed() => break STOPPED.to_string(),
        }
    };
    let mut tasks = Vec::new();
    for worker in workers.into_values() {
        let _ = worker.stop.send(());
        tasks.push(worker.task);
        backend.reporter.report(worker.strap.id, Link::OutOfRange, None, None);
    }
    for task in tasks {
        let _ = task.await;
    }
    reason
}

/// Starts tasks for new straps and stops those of removed or changed ones.
fn reconcile(adapter: &Adapter, reporter: &Reporter, workers: &mut HashMap<i64, Worker>, known: &[Strap]) {
    let gone: Vec<i64> = workers
        .iter()
        .filter(|(id, w)| !known.iter().any(|k| k.id == **id && k.address == w.strap.address))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        if let Some(worker) = workers.remove(&id) {
            let _ = worker.stop.send(());
        }
    }
    for strap in known {
        workers.entry(strap.id).or_insert_with(|| Worker::spawn(adapter, strap, reporter));
    }
}

async fn keep_connected(adapter: &Adapter, strap: &Strap, reporter: &Reporter) {
    let Ok(addr) = strap.address.parse::<Address>() else {
        tracing::warn!(address = strap.address, "strap with an invalid address");
        reporter.report(strap.id, Link::OutOfRange, None, None);
        return;
    };
    loop {
        reporter.report(strap.id, Link::Searching, None, None);
        match follow(adapter, addr, strap, reporter).await {
            Ok(()) => tracing::info!(strap = strap.name, "strap went silent"),
            Err(e) => {
                reporter.fail(strap.id, &e.to_string());
            }
        }
        reporter.report(strap.id, Link::OutOfRange, None, None);
        sleep(RETRY_STRAP).await;
    }
}

/// Connects and reports each heart rate until the strap goes silent or away.
async fn follow(adapter: &Adapter, addr: Address, strap: &Strap, reporter: &Reporter) -> bluer::Result<()> {
    if !adapter.device_addresses().await?.contains(&addr) {
        find(adapter, addr).await?;
    }
    let device = adapter.device(addr)?;
    let result = async {
        if !device.is_connected().await? {
            timeout(CONNECT_TIME, device.connect()).await.map_err(|_| timed_out("Verbinden"))??;
        }
        stream(&device, strap, reporter).await
    }
    .await;
    // Also after a timed-out connect: BlueZ would otherwise keep connecting.
    let _ = timeout(DISCONNECT_TIME, device.disconnect()).await;
    result
}

async fn stream(device: &Device, strap: &Strap, reporter: &Reporter) -> bluer::Result<()> {
    let resolve_until = Instant::now() + RESOLVE_TIME;
    while !device.is_services_resolved().await? {
        if Instant::now() >= resolve_until {
            return Err(timed_out("GATT-Dienste"));
        }
        sleep(Duration::from_millis(200)).await;
    }
    let Some(measurement) = characteristic(device, HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT).await? else {
        return Err(timed_out("kein Herzfrequenz-Dienst"));
    };
    let battery = match characteristic(device, BATTERY_SERVICE, BATTERY_LEVEL).await {
        Ok(Some(c)) => c.read().await.ok().and_then(|v| v.first().copied()),
        _ => None,
    };
    let values = measurement.notify().await?;
    pin_mut!(values);
    tracing::info!(strap = strap.name, ?battery, "strap connected");
    reporter.report(strap.id, Link::Connected, None, battery);
    while let Ok(Some(value)) = timeout(SILENT_TIME.max(STALE_AFTER), values.next()).await {
        if let Some(hr) = parse_measurement(&value) {
            reporter.report(strap.id, Link::Connected, Some(hr), None);
        }
    }
    Ok(())
}

async fn characteristic(device: &Device, service: u16, characteristic: u16) -> bluer::Result<Option<bluer::gatt::remote::Characteristic>> {
    for s in device.services().await? {
        if s.uuid().await? != uuid16(service) {
            continue;
        }
        for c in s.characteristics().await? {
            if c.uuid().await? == uuid16(characteristic) {
                return Ok(Some(c));
            }
        }
    }
    Ok(None)
}

/// Searches until BlueZ knows `addr`, at most `FIND_TIME`.
async fn find(adapter: &Adapter, addr: Address) -> bluer::Result<()> {
    let events = adapter.discover_devices().await?;
    pin_mut!(events);
    let found = timeout(FIND_TIME, async {
        while let Some(event) = events.next().await {
            if matches!(event, AdapterEvent::DeviceAdded(a) if a == addr) {
                return true;
            }
        }
        false
    })
    .await;
    match found {
        Ok(true) => Ok(()),
        _ => Err(timed_out("nicht gefunden")),
    }
}

/// Every device with a heart rate service seen within `SCAN_TIME`.
async fn scan(adapter: &Adapter) -> Result<Vec<Found>, ScanError> {
    let failed = |e: bluer::Error| ScanError::Failed(e.to_string());
    let events = adapter.discover_devices().await.map_err(failed)?;
    pin_mut!(events);
    let mut seen = Vec::new();
    let _ = timeout(SCAN_TIME, async {
        while let Some(event) = events.next().await {
            if let AdapterEvent::DeviceAdded(addr) = event
                && !seen.contains(&addr)
            {
                seen.push(addr);
            }
        }
    })
    .await;
    let mut found = Vec::new();
    for addr in seen {
        let Ok(device) = adapter.device(addr) else { continue };
        let heart_rate = device.uuids().await.ok().flatten().is_some_and(|u| u.contains(&uuid16(HEART_RATE_SERVICE)));
        if heart_rate {
            found.push(Found {
                address: addr.to_string(),
                name: device.name().await.ok().flatten(),
                rssi: device.rssi().await.ok().flatten(),
            });
        }
    }
    Ok(found)
}

fn timed_out(what: &str) -> bluer::Error {
    bluer::Error { kind: bluer::ErrorKind::Failed, message: what.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_uuids() {
        assert_eq!(uuid16(0x180D).to_string(), "0000180d-0000-1000-8000-00805f9b34fb");
        assert_eq!(uuid16(0x2A37).to_string(), "00002a37-0000-1000-8000-00805f9b34fb");
    }
}
