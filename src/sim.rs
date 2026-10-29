//! Simulated Kettler AX1 ergometer, usable in-process (duplex) or over TCP.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::protocol::{DeviceLimits, LineReader};

/// Maximum change of the actual watts per second.
const RAMP_WATTS_PER_SEC: f32 = 15.0;

/// Pulse of the running lively simulation (0 = none), for the simulated chest strap.
static SIM_PULSE: AtomicU16 = AtomicU16::new(0);

/// The pulse of the lively in-process simulation, if one is running. Tests get `None`:
/// they run in parallel, so they hand the strap simulation its pulse source directly.
pub fn current_pulse() -> Option<u16> {
    if cfg!(test) {
        return None;
    }
    Some(SIM_PULSE.load(Ordering::Relaxed)).filter(|&p| p > 0)
}

/// A heart rate that follows the power with a delay (first-order lag).
#[derive(Debug, Clone, Copy)]
pub struct HeartModel {
    rest: f64,
    bpm: f64,
}

impl HeartModel {
    /// Seconds until the pulse has covered about two thirds of a change.
    const TIME_CONSTANT_SECS: f64 = 30.0;
    /// Pulse increase per watt above rest.
    const BPM_PER_WATT: f64 = 0.45;
    /// The pulse does not climb above this, however hard the ride.
    const MAX_BPM: f64 = 185.0;

    pub fn new(rest: f64) -> Self {
        Self { rest, bpm: rest }
    }

    /// Moves `secs` towards the pulse for `watts`; without pedalling towards rest.
    pub fn step(&mut self, watts: f64, pedalling: bool, secs: f64) {
        let goal = if pedalling { (self.rest + Self::BPM_PER_WATT * watts).min(Self::MAX_BPM) } else { self.rest };
        self.bpm += (goal - self.bpm) * (1.0 - (-secs / Self::TIME_CONSTANT_SECS).exp());
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }
}

#[derive(Debug)]
pub struct SimErgometer {
    limits: DeviceLimits,
    pc_mode: bool,
    rpm: u16,
    target: u16,
    actual: f32,
    elapsed: f32,
    /// Distance in 0.1 km units (provisional, device measurement pending).
    distance: f32,
    /// Metabolic energy in kJ (mechanical kJ x 4, as measured on the device).
    energy: f32,
    /// Cadence wobbles around `rpm` and the pulse field follows the load (for `sim://`).
    lively: bool,
    /// Seconds since the start, for the wobble.
    clock: f32,
    heart: HeartModel,
}

impl SimErgometer {
    pub fn new() -> Self {
        let limits = DeviceLimits::default();
        Self {
            limits,
            pc_mode: false,
            rpm: 0,
            target: limits.min,
            actual: 0.0,
            elapsed: 0.0,
            distance: 0.0,
            energy: 0.0,
            lively: false,
            clock: 0.0,
            heart: HeartModel::new(60.0),
        }
    }

    pub fn set_lively(&mut self, on: bool) {
        self.lively = on;
    }

    pub fn set_rpm(&mut self, rpm: u16) {
        self.rpm = rpm;
    }

    /// Cockpit button press: changes the target regardless of PC mode.
    #[cfg(test)]
    pub fn press_cockpit(&mut self, delta: i16) {
        self.target = self.limits.clamp(i32::from(self.target) + i32::from(delta));
    }

    /// The cadence the cockpit reports: `rpm`, wobbling by up to ±4 when lively.
    fn cadence(&self) -> u16 {
        if !self.lively || self.rpm == 0 {
            return self.rpm;
        }
        let t = f64::from(self.clock);
        let wobble = 3.0 * (t / 17.0).sin() + 1.5 * (t / 5.0).sin();
        (f64::from(self.rpm) + wobble).round().max(1.0) as u16
    }

    /// The pulse field: 0 like a cockpit without a receiver, unless lively.
    fn pulse(&self) -> u16 {
        if !self.lively {
            return 0;
        }
        let wobble = 2.0 * (f64::from(self.clock) / 7.0).sin();
        (self.heart.bpm() + wobble).round() as u16
    }

    fn speed_x10(&self) -> u16 {
        (u32::from(self.cadence()) * 35 / 10).min(u32::from(u16::MAX)) as u16
    }

    pub fn tick(&mut self, secs: f32) {
        if self.rpm > 0 {
            self.elapsed += secs;
            self.distance += f32::from(self.speed_x10()) / 10.0 / 3600.0 * 10.0 * secs;
            let goal = f32::from(self.target);
            let step = RAMP_WATTS_PER_SEC * secs;
            self.actual += (goal - self.actual).clamp(-step, step);
        } else {
            self.actual = (self.actual - RAMP_WATTS_PER_SEC * secs).max(0.0);
        }
        self.energy += self.actual * secs / 1000.0 * 4.0;
        if self.lively {
            self.clock += secs;
            self.heart.step(f64::from(self.actual), self.rpm > 0, f64::from(secs));
        }
    }

    fn status_line(&self) -> String {
        let secs = self.elapsed as u32;
        format!(
            "{:03}\t{:03}\t{:03}\t{:03}\t{:03}\t{:04}\t{:02}:{:02}\t{:03}",
            self.pulse(),
            self.cadence(),
            self.speed_x10(),
            self.distance as u32,
            self.target,
            self.energy as u32,
            secs / 60,
            secs % 60,
            self.actual.round() as u32,
        )
    }

    /// Returns the response line without `\r\n`; `None` means the device stays silent.
    pub fn handle(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        match line {
            "RS" => {
                self.pc_mode = false;
                self.target = self.limits.min;
                self.actual = 0.0;
                self.elapsed = 0.0;
                self.distance = 0.0;
                self.energy = 0.0;
                Some("ACK".into())
            }
            "ID" => Some("AX1S".into()),
            "VE" => Some("018".into()),
            "CM" if self.rpm > 0 => {
                self.pc_mode = true;
                Some("ACK".into())
            }
            "ST" => Some(self.status_line()),
            _ => {
                let watts: i32 = line.strip_prefix("PW ")?.trim().parse().ok()?;
                if self.pc_mode {
                    self.target = self.limits.clamp(watts);
                }
                Some(self.status_line())
            }
        }
    }
}

impl Default for SimErgometer {
    fn default() -> Self {
        Self::new()
    }
}

/// Ticks the simulation once a second until nobody else holds it any more.
fn spawn_ticker(sim: Weak<Mutex<SimErgometer>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let Some(sim) = sim.upgrade() else { break };
            let mut sim = sim.lock().unwrap();
            sim.tick(1.0);
            if sim.lively {
                SIM_PULSE.store(sim.pulse(), Ordering::Relaxed);
            }
        }
        SIM_PULSE.store(0, Ordering::Relaxed);
    });
}

/// Answers commands on `io` until it closes or fails.
async fn serve_io<S: AsyncRead + AsyncWrite + Unpin>(sim: Arc<Mutex<SimErgometer>>, mut io: S) {
    let mut reader = LineReader::new();
    let mut buf = [0u8; 256];
    loop {
        let n = match io.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for line in reader.push(&buf[..n]) {
            let reply = sim.lock().unwrap().handle(&line);
            if let Some(reply) = reply
                && io.write_all(format!("{reply}\r\n").as_bytes()).await.is_err()
            {
                return;
            }
        }
    }
}

pub fn spawn_duplex(sim: Arc<Mutex<SimErgometer>>) -> DuplexStream {
    let (client, device) = tokio::io::duplex(1024);
    spawn_ticker(Arc::downgrade(&sim));
    tokio::spawn(serve_io(sim, device));
    client
}

/// Binds `addr` and serves one client at a time in a background task; the
/// simulation state persists across clients. Returns the bound address.
pub async fn bind_tcp(addr: SocketAddr, rpm: u16) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let sim = Arc::new(Mutex::new(SimErgometer::new()));
    sim.lock().unwrap().set_rpm(rpm);
    spawn_ticker(Arc::downgrade(&sim));
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, rpm, "simulated AX1 listening");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            tracing::info!(%peer, "client connected");
            serve_io(sim.clone(), stream).await;
            tracing::info!(%peer, "client disconnected");
        }
    });
    Ok((local, handle))
}

pub async fn serve_tcp(addr: SocketAddr, rpm: u16) -> anyhow::Result<()> {
    let (_, handle) = bind_tcp(addr, rpm).await?;
    handle.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_status;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn cm_only_acks_while_pedalling() {
        let mut s = SimErgometer::new();
        assert_eq!(s.handle("CM"), None);
        s.set_rpm(60);
        assert_eq!(s.handle("CM").as_deref(), Some("ACK"));
    }
    #[test]
    fn pw_ignored_without_pc_mode() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        s.handle("PW 80");
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().target_watts, 25);
    }
    #[test]
    fn pw_works_at_rpm_zero_once_in_pc_mode() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        assert_eq!(s.handle("CM").as_deref(), Some("ACK"));
        s.set_rpm(0);
        s.handle("PW 80");
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().target_watts, 80);
    }
    #[test]
    fn clock_stops_at_rpm_zero() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        for _ in 0..3 {
            s.tick(1.0);
        }
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().elapsed_secs, 3);
        s.set_rpm(0);
        for _ in 0..5 {
            s.tick(1.0);
        }
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().elapsed_secs, 3);
    }
    #[test]
    fn energy_counts_metabolic_kj() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        s.handle("CM");
        s.handle("PW 100");
        while parse_status(&s.handle("ST").unwrap()).unwrap().actual_watts != 100 {
            s.tick(1.0);
        }
        let before = parse_status(&s.handle("ST").unwrap()).unwrap().energy;
        for _ in 0..180 {
            s.tick(1.0);
        }
        let after = parse_status(&s.handle("ST").unwrap()).unwrap().energy;
        assert!((i32::from(after) - i32::from(before) - 72).abs() <= 1, "{before} -> {after}");
    }
    #[test]
    fn actual_watts_ramp_towards_target() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        s.handle("CM");
        s.handle("PW 100");
        s.tick(1.0);
        let a = parse_status(&s.handle("ST").unwrap()).unwrap().actual_watts;
        assert!(a > 0 && a < 100);
        for _ in 0..10 {
            s.tick(1.0);
        }
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().actual_watts, 100);
    }
    #[test]
    fn cockpit_press_overrides_target() {
        let mut s = SimErgometer::new();
        s.set_rpm(60);
        s.handle("CM");
        s.handle("PW 80");
        s.press_cockpit(5);
        assert_eq!(parse_status(&s.handle("ST").unwrap()).unwrap().target_watts, 85);
    }
    fn lively_at(watts: u16) -> SimErgometer {
        let mut s = SimErgometer::new();
        s.set_rpm(88);
        s.set_lively(true);
        s.handle("CM");
        s.handle(&format!("PW {watts}"));
        s
    }
    fn status(s: &mut SimErgometer) -> crate::protocol::Status {
        parse_status(&s.handle("ST").unwrap()).unwrap()
    }
    #[test]
    fn lively_cadence_stays_in_band() {
        let mut s = lively_at(100);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..600 {
            s.tick(1.0);
            let rpm = status(&mut s).rpm;
            assert!((84..=92).contains(&rpm), "{rpm}");
            seen.insert(rpm);
        }
        assert!(seen.len() >= 3, "{seen:?}");
    }
    #[test]
    fn pulse_follows_load() {
        let mut s = lively_at(170);
        for _ in 0..300 {
            s.tick(1.0);
        }
        let high = status(&mut s).pulse;
        assert!((128..=144).contains(&high), "{high}");
        s.handle("PW 60");
        for _ in 0..300 {
            s.tick(1.0);
        }
        let low = status(&mut s).pulse;
        assert!(low < 100, "{low}");
    }
    #[test]
    fn pulse_stays_below_max() {
        let mut s = lively_at(400);
        for _ in 0..600 {
            s.tick(1.0);
            let pulse = status(&mut s).pulse;
            assert!(pulse <= 187, "{pulse}");
        }
    }
    #[test]
    fn pulse_relaxes_at_rest() {
        let mut s = lively_at(170);
        for _ in 0..300 {
            s.tick(1.0);
        }
        let before = status(&mut s).pulse;
        s.set_rpm(0);
        s.tick(1.0);
        let after = status(&mut s).pulse;
        assert!(before.abs_diff(after) <= 3, "{before} -> {after}");
        for _ in 0..300 {
            s.tick(1.0);
        }
        let rest = status(&mut s).pulse;
        assert!(rest < 75, "{rest}");
    }
    #[test]
    fn plain_sim_unchanged() {
        let mut s = SimErgometer::new();
        s.set_rpm(70);
        s.handle("CM");
        s.handle("PW 150");
        for _ in 0..120 {
            s.tick(1.0);
            let st = status(&mut s);
            assert_eq!((st.rpm, st.pulse), (70, 0));
        }
    }
    #[tokio::test]
    async fn duplex_answers_commands() {
        let sim = Arc::new(Mutex::new(SimErgometer::new()));
        let mut io = spawn_duplex(sim);
        io.write_all(b"ID\r\n").await.unwrap();
        let mut buf = [0u8; 16];
        let n = io.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"AX1S\r\n");
    }
}
