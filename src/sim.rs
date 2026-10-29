//! Simulated Kettler AX1 ergometer, usable in-process (duplex) or over TCP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::protocol::{DeviceLimits, LineReader};

/// Maximum change of the actual watts per second.
const RAMP_WATTS_PER_SEC: f32 = 15.0;

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
        }
    }

    pub fn set_rpm(&mut self, rpm: u16) {
        self.rpm = rpm;
    }

    /// Cockpit button press: changes the target regardless of PC mode.
    #[cfg(test)]
    pub fn press_cockpit(&mut self, delta: i16) {
        self.target = self.limits.clamp(i32::from(self.target) + i32::from(delta));
    }

    fn speed_x10(&self) -> u16 {
        (u32::from(self.rpm) * 35 / 10).min(u32::from(u16::MAX)) as u16
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
    }

    fn status_line(&self) -> String {
        let secs = self.elapsed as u32;
        format!(
            "{:03}\t{:03}\t{:03}\t{:03}\t{:03}\t{:04}\t{:02}:{:02}\t{:03}",
            0,
            self.rpm,
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
            sim.lock().unwrap().tick(1.0);
        }
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
