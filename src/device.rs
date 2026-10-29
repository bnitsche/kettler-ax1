//! Device task: owns the connection to the ergometer (handshake, polling,
//! set-power, reconnect) and publishes `DeviceEvent`s.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{MissedTickBehavior, timeout};

use crate::key_actions::KeyInput;
use crate::keypad::{self, Key, KeyDecoder, KeyEvent};
use crate::monitor::{Disconnect, Monitor, TraceKind, now_ms};
use crate::protocol::{Command, LineReader, Status, parse_status};
use crate::transport::{DeviceUrl, Link, ModemSource, Opened, open};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(1);
const CM_TIMEOUT: Duration = Duration::from_millis(500);
const PW_TIMEOUT: Duration = Duration::from_millis(500);
const ID_TIMEOUT: Duration = Duration::from_secs(1);
/// Short: an older cockpit may not know `VE`.
const VE_TIMEOUT: Duration = Duration::from_millis(500);
const ST_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_FAILED_CYCLES: u32 = 3;
const EVENT_CAPACITY: usize = 64;

type OpenFuture = Pin<Box<dyn Future<Output = io::Result<Opened>> + Send>>;

#[derive(Clone, Debug, Serialize)]
pub enum DeviceEvent {
    /// `id` is empty if the device has not answered `ID` yet; once it does, `Connected`
    /// is sent again with the id.
    Connected { id: String },
    Disconnected { reason: String },
    Status(Status),
    /// Handlebar keys read from the modem lines of the serial port.
    Key(KeyInput),
}

#[derive(Clone)]
pub struct DeviceHandle {
    commands: mpsc::UnboundedSender<u16>,
    events: broadcast::Sender<DeviceEvent>,
    monitor: Monitor,
}

impl DeviceHandle {
    /// Queues a power setting; it is sent in the next poll cycle.
    pub fn set_power(&self, watts: u16) {
        let _ = self.commands.send(watts);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.events.subscribe()
    }

    /// Raw traffic, identity and modem lines for the service page.
    pub fn monitor(&self) -> Monitor {
        self.monitor.clone()
    }

    /// Injects an event as if the device task had sent it.
    #[cfg(test)]
    pub fn announce(&self, ev: DeviceEvent) {
        let _ = self.events.send(ev);
    }
}

/// Spawns the device task. It ends (and closes the link) when all handles are dropped.
pub fn spawn_device(url: DeviceUrl, poll: Duration) -> DeviceHandle {
    spawn_device_with(
        move || {
            let url = url.clone();
            Box::pin(async move { open(&url).await }) as OpenFuture
        },
        poll,
        RECONNECT_DELAY,
    )
}

fn spawn_device_with(
    open: impl Fn() -> OpenFuture + Send + 'static,
    poll: Duration,
    retry_delay: Duration,
) -> DeviceHandle {
    let (commands, rx) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let monitor = Monitor::new();
    let handle = DeviceHandle { commands, events: events.clone(), monitor: monitor.clone() };
    tokio::spawn(run(open, poll, retry_delay, rx, events, monitor));
    handle
}

/// Why a session ended.
enum End {
    /// Too many cycles without a valid ST reply.
    NoStatus,
    Io(io::Error),
    /// All handles were dropped.
    Shutdown,
}

/// `Disconnected` is a state change: it is emitted when a connected device is
/// lost, and once at startup if the first attempt fails.
async fn run(
    open: impl Fn() -> OpenFuture,
    poll: Duration,
    retry_delay: Duration,
    mut commands: mpsc::UnboundedReceiver<u16>,
    events: broadcast::Sender<DeviceEvent>,
    monitor: Monitor,
) {
    // Latest requested power; kept across reconnects until it has been sent.
    let mut pending_power: Option<u16> = None;
    let mut connected = false;
    let mut announced = false;
    let mut last_reason = String::new();
    loop {
        let reason = match open().await {
            Ok(opened) => {
                let end = session(
                    opened,
                    poll,
                    &mut commands,
                    &mut pending_power,
                    &events,
                    &monitor,
                    &mut connected,
                )
                .await;
                monitor.set_modem(None);
                match end {
                    End::Shutdown => return,
                    End::NoStatus => {
                        format!("no valid ST reply in {MAX_FAILED_CYCLES} consecutive cycles")
                    }
                    End::Io(e) => e.to_string(),
                }
            }
            Err(e) => e.to_string(),
        };
        if connected || !announced {
            tracing::info!(%reason, "device disconnected");
            let _ = events.send(DeviceEvent::Disconnected { reason: reason.clone() });
            monitor.trace(TraceKind::Link, format!("getrennt: {reason}"));
            let at_ms = now_ms();
            monitor.update_info(|i| {
                i.connected = false;
                i.since_ms = Some(at_ms);
                i.last_disconnect = Some(Disconnect { at_ms, reason: reason.clone() });
            });
            connected = false;
            announced = true;
        } else if reason != last_reason {
            tracing::info!(%reason, "device still unavailable");
        } else {
            tracing::debug!(%reason, "device still unavailable");
        }
        last_reason = reason;

        let sleep = tokio::time::sleep(retry_delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = commands.recv() => match cmd {
                    Some(w) => pending_power = Some(w),
                    None => return,
                },
            }
        }
    }
}

struct Conn {
    link: Box<dyn Link>,
    reader: LineReader,
    lines: VecDeque<String>,
    monitor: Monitor,
    /// For a timeout entry in the raw traffic.
    last_command: String,
}

impl Conn {
    /// Sends a command after dropping stale lines, so replies cannot shift by a cycle.
    async fn send(&mut self, cmd: Command) -> io::Result<()> {
        tracing::debug!(?cmd, "send");
        for line in self.lines.drain(..) {
            self.monitor.trace(TraceKind::Discarded, line);
        }
        self.last_command = cmd.text();
        self.monitor.trace(TraceKind::Sent, self.last_command.clone());
        let write = async {
            self.link.write_all(&cmd.encode()).await?;
            self.link.flush().await
        };
        timeout(SEND_TIMEOUT, write)
            .await
            .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out")))
    }

    /// Reads lines until `pick` accepts one. Non-matching lines are dropped.
    /// `Ok(None)` on timeout, `Err` on I/O error or EOF.
    async fn wait<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&str) -> Option<T>,
    ) -> io::Result<Option<T>> {
        let fut = async {
            let mut buf = [0u8; 256];
            loop {
                while let Some(line) = self.lines.pop_front() {
                    if let Some(v) = pick(&line) {
                        self.monitor.trace(TraceKind::Received, line);
                        return Ok(v);
                    }
                    tracing::debug!(%line, "discarded line");
                    self.monitor.trace(TraceKind::Discarded, line);
                }
                let n = self.link.read(&mut buf).await?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
                }
                self.lines.extend(self.reader.push(&buf[..n]));
            }
        };
        match timeout(limit, fut).await {
            Ok(Ok(v)) => Ok(Some(v)),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.monitor.trace(TraceKind::Timeout, format!("keine Antwort auf {}", self.last_command));
                Ok(None)
            }
        }
    }
}

/// Runs one connection until it ends. Sets `connected` once `Connected` was emitted.
async fn session(
    Opened { link, modem }: Opened,
    poll: Duration,
    commands: &mut mpsc::UnboundedReceiver<u16>,
    pending_power: &mut Option<u16>,
    events: &broadcast::Sender<DeviceEvent>,
    monitor: &Monitor,
    connected: &mut bool,
) -> End {
    let mut conn = Conn {
        link,
        reader: LineReader::new(),
        lines: VecDeque::new(),
        monitor: monitor.clone(),
        last_command: String::new(),
    };
    let result = match modem {
        None => cycles(&mut conn, poll, commands, pending_power, events, connected).await,
        Some(modem) => {
            let mut keys = KeySampler { decoder: KeyDecoder::new(), live: true, events, monitor };
            let _ = events.send(DeviceEvent::Key(KeyInput::Connected(true)));
            let result = tokio::select! {
                () = keys.sample(modem) => unreachable!("key sampling never ends"),
                r = cycles(&mut conn, poll, commands, pending_power, events, connected) => r,
            };
            keys.stop();
            result
        }
    };
    match result {
        Ok(end) => end,
        Err(e) => End::Io(e),
    }
}

/// Decodes the modem lines of one serial session into `DeviceEvent::Key`s.
struct KeySampler<'a> {
    decoder: KeyDecoder,
    /// Whether `Connected(true)` was sent and `Connected(false)` is still due.
    live: bool,
    events: &'a broadcast::Sender<DeviceEvent>,
    monitor: &'a Monitor,
}

impl KeySampler<'_> {
    /// Samples every [`keypad::POLL`]. After a read error it stops sampling and
    /// waits for the session to end; it never returns.
    async fn sample(&mut self, modem: Arc<dyn ModemSource>) {
        let started = tokio::time::Instant::now();
        let mut tick = tokio::time::interval(keypad::POLL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match modem.read() {
                Ok(lines) => {
                    // Raw for the service page; the decoder debounces on its own.
                    self.monitor.set_modem(Some(lines));
                    if let Some(ev) = self.decoder.update(lines, started.elapsed()) {
                        tracing::info!(?ev, "key");
                        self.monitor.trace(TraceKind::Key, key_text(ev));
                        let _ = self.events.send(DeviceEvent::Key(KeyInput::Event(ev)));
                    }
                }
                Err(err) => {
                    tracing::warn!("modem lines unreadable, key sampling stopped: {err}");
                    self.stop();
                    break;
                }
            }
        }
        std::future::pending().await
    }

    /// Releases a held key and reports the keys as gone, once.
    fn stop(&mut self) {
        self.monitor.set_modem(None);
        if let Some(ev) = self.decoder.reset() {
            self.monitor.trace(TraceKind::Key, key_text(ev));
            let _ = self.events.send(DeviceEvent::Key(KeyInput::Event(ev)));
        }
        if self.live {
            self.live = false;
            let _ = self.events.send(DeviceEvent::Key(KeyInput::Connected(false)));
        }
    }
}

/// A reply to `ID` or `VE`: the first line that is no `ACK` and no status line.
fn other_reply(line: &str) -> Option<String> {
    let l = line.trim();
    (!l.is_empty() && l != "ACK" && parse_status(l).is_err()).then(|| l.to_string())
}

/// German, as shown on the service page.
fn key_text(ev: KeyEvent) -> String {
    let name = |key| match key {
        Key::Up => "HOCH",
        Key::Down => "RUNTER",
        Key::Left => "LINKS",
        Key::Right => "RECHTS",
    };
    match ev {
        KeyEvent::Press(key) => format!("{} gedrückt", name(key)),
        KeyEvent::Release(key) => format!("{} losgelassen", name(key)),
    }
}

async fn cycles(
    conn: &mut Conn,
    poll: Duration,
    commands: &mut mpsc::UnboundedReceiver<u16>,
    pending_power: &mut Option<u16>,
    events: &broadcast::Sender<DeviceEvent>,
    connected: &mut bool,
) -> io::Result<End> {
    let mut pc_mode = false;
    let mut id: Option<String> = None;
    let mut failed = 0u32;
    let mut interval = tokio::time::interval(poll);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        loop {
            match commands.try_recv() {
                Ok(w) => *pending_power = Some(w),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(End::Shutdown),
            }
        }

        if !pc_mode {
            conn.send(Command::PcMode).await?;
            if conn.wait(CM_TIMEOUT, |l| (l.trim() == "ACK").then_some(())).await?.is_some() {
                pc_mode = true;
                conn.send(Command::Id).await?;
                id = conn
                    .wait(ID_TIMEOUT, other_reply)
                    .await?;
                conn.send(Command::Version).await?;
                let firmware = conn.wait(VE_TIMEOUT, other_reply).await?;
                tracing::debug!(?id, ?firmware, "PC mode active");
                let known = id.clone();
                conn.monitor.update_info(|i| {
                    if known.is_some() {
                        i.id = known;
                    }
                    i.firmware = firmware;
                });
                // Connected went out before the ID was known: announce it again.
                if *connected && let Some(id) = &id {
                    tracing::info!(%id, "device identified");
                    let _ = events.send(DeviceEvent::Connected { id: id.clone() });
                }
            }
        }

        if let Some(w) = *pending_power {
            conn.send(Command::SetPower(w)).await?;
            // The reply is a status line; drop it and let ST fetch a fresh one.
            conn.wait(PW_TIMEOUT, |l| parse_status(l).ok()).await?;
            *pending_power = None;
        }

        conn.send(Command::Status).await?;
        match conn.wait(ST_TIMEOUT, |l| parse_status(l).ok()).await? {
            Some(status) => {
                failed = 0;
                if !*connected {
                    *connected = true;
                    let id = id.clone().unwrap_or_default();
                    tracing::info!(%id, "device connected");
                    let text = if id.is_empty() { "verbunden".to_string() } else { format!("verbunden: {id}") };
                    conn.monitor.trace(TraceKind::Link, text);
                    conn.monitor.update_info(|i| {
                        i.connected = true;
                        i.since_ms = Some(now_ms());
                    });
                    let _ = events.send(DeviceEvent::Connected { id });
                }
                let _ = events.send(DeviceEvent::Status(status));
            }
            None => {
                failed += 1;
                tracing::debug!(failed, "no valid ST reply");
                if failed >= MAX_FAILED_CYCLES {
                    return Ok(End::NoStatus);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_actions::KeyInput;
    use crate::keypad::{Key, KeyEvent, ModemLines};
    use crate::monitor::TraceKind;
    use crate::sim::bind_tcp;

    const POLL: Duration = Duration::from_millis(50);
    const WAIT: Duration = Duration::from_secs(2);
    const FAST_RETRY: Duration = Duration::from_millis(20);
    const GOOD_ST: &[u8] = b"000\t070\t245\t001\t080\t0004\t00:13\t080\r\n";

    type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    async fn next(rx: &mut broadcast::Receiver<DeviceEvent>) -> DeviceEvent {
        timeout(WAIT, rx.recv()).await.expect("timed out").expect("channel closed")
    }

    async fn wait_for(
        rx: &mut broadcast::Receiver<DeviceEvent>,
        limit: Duration,
        mut f: impl FnMut(&DeviceEvent) -> bool,
    ) -> DeviceEvent {
        timeout(limit, async {
            loop {
                let ev = rx.recv().await.expect("channel closed");
                if f(&ev) {
                    return ev;
                }
            }
        })
        .await
        .expect("timed out")
    }

    /// Fake ergometer on a duplex stream: answers CM and ID, answers ST with
    /// `st_reply` (silent if `None`) and records every received command. The
    /// task finishes when the client side closes.
    fn fake_peer(
        mut peer: tokio::io::DuplexStream,
        st_reply: Option<&'static [u8]>,
        log: Log,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut reader = LineReader::new();
            let mut buf = [0u8; 64];
            loop {
                let n = match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for line in reader.push(&buf[..n]) {
                    log.lock().unwrap().push(line.clone());
                    let reply: Option<&[u8]> = match line.as_str() {
                        "CM" => Some(b"ACK\r\n"),
                        "ID" => Some(b"AX1S\r\n"),
                        "VE" => Some(b"018\r\n"),
                        "ST" => st_reply,
                        _ => None,
                    };
                    if let Some(reply) = reply
                        && peer.write_all(reply).await.is_err()
                    {
                        return;
                    }
                }
            }
        })
    }

    /// Opener that hands out `client` once and fails afterwards.
    fn once_opener(client: tokio::io::DuplexStream) -> impl Fn() -> OpenFuture + Send + 'static {
        let link = std::sync::Mutex::new(Some(Opened::plain(Box::new(client))));
        move || {
            let r = link.lock().unwrap().take().ok_or_else(|| io::Error::other("already used"));
            Box::pin(async move { r }) as OpenFuture
        }
    }

    #[tokio::test]
    async fn emits_connected_then_status() {
        let h = spawn_device(DeviceUrl::Sim, POLL);
        let mut rx = h.subscribe();
        match next(&mut rx).await {
            DeviceEvent::Connected { id } => assert_eq!(id, "AX1S"),
            other => panic!("expected Connected, got {other:?}"),
        }
        assert!(matches!(next(&mut rx).await, DeviceEvent::Status(_)));
    }

    #[tokio::test]
    async fn set_power_reaches_device() {
        let h = spawn_device(DeviceUrl::Sim, POLL);
        let mut rx = h.subscribe();
        h.set_power(120);
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(s) if s.target_watts == 120)).await;
    }

    #[tokio::test]
    async fn reconnects_after_tcp_drop() {
        let (addr, server) = bind_tcp("127.0.0.1:0".parse().unwrap(), 70).await.unwrap();
        let h = spawn_device(DeviceUrl::Tcp(addr.to_string()), POLL);
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Connected { .. })).await;

        // Aborting the accept task also drops the connected client stream.
        server.abort();
        let _ = server.await;
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Disconnected { .. })).await;

        let (_addr, _server) = bind_tcp(addr, 70).await.unwrap();
        wait_for(&mut rx, Duration::from_secs(6), |e| matches!(e, DeviceEvent::Connected { .. })).await;
    }

    #[tokio::test]
    async fn survives_garbage_between_lines() {
        let (client, mut peer) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            let mut reader = LineReader::new();
            let mut buf = [0u8; 64];
            loop {
                let n = match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for line in reader.push(&buf[..n]) {
                    let reply: &[u8] = match line.as_str() {
                        "CM" => b"ACK\r\n",
                        "ID" => b"AX1S\r\n",
                        "ST" => b"\xff\xfe\r\n000\t070\t245\t001\t080\t0004\t00:13\t080\r\n",
                        _ => continue,
                    };
                    if peer.write_all(reply).await.is_err() {
                        return;
                    }
                }
            }
        });
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        let mut seen = Vec::new();
        loop {
            let ev = next(&mut rx).await;
            assert!(!matches!(ev, DeviceEvent::Disconnected { .. }), "{ev:?}");
            let done = matches!(ev, DeviceEvent::Status(_));
            seen.push(ev);
            if done {
                break;
            }
        }
        assert!(matches!(seen[0], DeviceEvent::Connected { .. }));
    }

    #[tokio::test]
    async fn three_silent_cycles_disconnect() {
        let (client, peer) = tokio::io::duplex(1024);
        let _peer = fake_peer(peer, None, Log::default());
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        // 3 x (1 s ST timeout + poll) plus margin.
        let ev = wait_for(&mut rx, Duration::from_secs(5), |e| {
            assert!(!matches!(e, DeviceEvent::Connected { .. } | DeviceEvent::Status(_)), "{e:?}");
            matches!(e, DeviceEvent::Disconnected { .. })
        })
        .await;
        assert!(matches!(ev, DeviceEvent::Disconnected { .. }));
    }

    #[tokio::test]
    async fn handshake_sends_no_rs_and_id_once() {
        let (client, peer) = tokio::io::duplex(1024);
        let log = Log::default();
        let _peer = fake_peer(peer, Some(GOOD_ST), log.clone());
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        for _ in 0..11 {
            wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        }
        let log = log.lock().unwrap();
        assert!(!log.iter().any(|l| l == "RS"), "{log:?}");
        assert_eq!(log.iter().filter(|l| *l == "ID").count(), 1, "{log:?}");
        assert_eq!(log.iter().filter(|l| *l == "CM").count(), 1, "{log:?}");
        assert_eq!(log.iter().filter(|l| *l == "VE").count(), 1, "{log:?}");
    }

    /// A peer that answers CM, ID and ST (after `before_st`) but never VE.
    fn peer_without_ve(mut peer: tokio::io::DuplexStream, before_st: &'static [u8]) {
        tokio::spawn(async move {
            let mut reader = LineReader::new();
            let mut buf = [0u8; 64];
            loop {
                let n = match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for line in reader.push(&buf[..n]) {
                    let reply: Vec<u8> = match line.as_str() {
                        "CM" => b"ACK\r\n".to_vec(),
                        "ID" => b"AX1S\r\n".to_vec(),
                        "ST" => [before_st, GOOD_ST].concat(),
                        _ => continue,
                    };
                    if peer.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            }
        });
    }

    #[tokio::test]
    async fn firmware_from_ve() {
        let (client, peer) = tokio::io::duplex(1024);
        let _peer = fake_peer(peer, Some(GOOD_ST), Log::default());
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        let info = h.monitor().info();
        assert!(info.connected);
        assert_eq!(info.id.as_deref(), Some("AX1S"));
        assert_eq!(info.firmware.as_deref(), Some("018"));
        assert!(info.since_ms.is_some());
    }

    #[tokio::test]
    async fn no_ve_reply_still_connects() {
        let (client, peer) = tokio::io::duplex(1024);
        peer_without_ve(peer, b"");
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        for _ in 0..3 {
            wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        }
        let info = h.monitor().info();
        assert!(info.connected);
        assert_eq!(info.id.as_deref(), Some("AX1S"));
        assert_eq!(info.firmware, None);
        let history = h.monitor().history();
        assert!(history.iter().any(|e| e.kind == TraceKind::Timeout && e.text.contains("VE")), "{history:?}");
    }

    #[tokio::test]
    async fn trace_shows_commands_replies_and_discards() {
        let (client, peer) = tokio::io::duplex(1024);
        peer_without_ve(peer, b"xyz\r\n");
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        let history = h.monitor().history();
        let has = |kind: TraceKind, f: &dyn Fn(&str) -> bool| history.iter().any(|e| e.kind == kind && f(&e.text));
        assert!(has(TraceKind::Sent, &|t| t == "CM"), "{history:?}");
        assert!(has(TraceKind::Received, &|t| t == "ACK"), "{history:?}");
        assert!(has(TraceKind::Sent, &|t| t == "ST"), "{history:?}");
        assert!(has(TraceKind::Discarded, &|t| t == "xyz"), "{history:?}");
        assert!(has(TraceKind::Received, &|t| t.contains('\t')), "{history:?}");
        assert!(has(TraceKind::Link, &|t| t.starts_with("verbunden")), "{history:?}");
    }

    #[tokio::test]
    async fn disconnect_is_recorded() {
        let h = spawn_device_with(
            || Box::pin(async { Err(io::Error::other("down")) }) as OpenFuture,
            POLL,
            FAST_RETRY,
        );
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Disconnected { .. })).await;
        let info = h.monitor().info();
        assert!(!info.connected);
        assert_eq!(info.last_disconnect.map(|d| d.reason).as_deref(), Some("down"));
        let history = h.monitor().history();
        assert!(history.iter().any(|e| e.kind == TraceKind::Link && e.text == "getrennt: down"), "{history:?}");
    }

    #[tokio::test]
    async fn disconnected_only_on_transition() {
        let h = spawn_device_with(
            || Box::pin(async { Err(io::Error::other("down")) }) as OpenFuture,
            POLL,
            FAST_RETRY,
        );
        let mut rx = h.subscribe();
        let mut count = 0;
        let _ = timeout(Duration::from_millis(600), async {
            loop {
                if let Ok(DeviceEvent::Disconnected { .. }) = rx.recv().await {
                    count += 1;
                }
            }
        })
        .await;
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn id_learned_later_is_announced_again() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (client, mut peer) = tokio::io::duplex(1024);
        let pedalling = std::sync::Arc::new(AtomicBool::new(false));
        let flag = pedalling.clone();
        // Answers ST at once, but CM (and so ID) only once someone pedals.
        tokio::spawn(async move {
            let mut reader = LineReader::new();
            let mut buf = [0u8; 64];
            loop {
                let n = match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for line in reader.push(&buf[..n]) {
                    let reply: &[u8] = match line.as_str() {
                        "CM" if flag.load(Ordering::SeqCst) => b"ACK\r\n",
                        "ID" => b"AX1S\r\n",
                        "ST" => GOOD_ST,
                        _ => continue,
                    };
                    if peer.write_all(reply).await.is_err() {
                        return;
                    }
                }
            }
        });
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        match wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Connected { .. })).await {
            DeviceEvent::Connected { id } => assert_eq!(id, ""),
            _ => unreachable!(),
        }
        pedalling.store(true, Ordering::SeqCst);
        let ev = wait_for(&mut rx, WAIT, |e| {
            assert!(!matches!(e, DeviceEvent::Disconnected { .. }), "{e:?}");
            matches!(e, DeviceEvent::Connected { .. })
        })
        .await;
        assert!(matches!(ev, DeviceEvent::Connected { id } if id == "AX1S"));
    }

    type FakeLines = std::sync::Arc<std::sync::Mutex<Option<ModemLines>>>;

    /// Modem lines the test sets by hand; `None` makes every read fail.
    struct FakeModem(FakeLines);

    impl ModemSource for FakeModem {
        fn read(&self) -> io::Result<ModemLines> {
            self.0.lock().unwrap().ok_or_else(|| io::Error::other("modem gone"))
        }
    }

    const LEFT_LINES: ModemLines = ModemLines { cts: false, dsr: false, ri: false, dcd: true };

    /// A device with a fake modem; returns the handle, the lines and the fake ergometer task.
    fn modem_device() -> (DeviceHandle, FakeLines, tokio::task::JoinHandle<()>) {
        let (client, peer) = tokio::io::duplex(1024);
        let peer = fake_peer(peer, Some(GOOD_ST), Log::default());
        let lines: FakeLines = std::sync::Arc::new(std::sync::Mutex::new(Some(ModemLines::default())));
        let modem: std::sync::Arc<dyn ModemSource> = std::sync::Arc::new(FakeModem(lines.clone()));
        let opened = std::sync::Mutex::new(Some(Opened { link: Box::new(client), modem: Some(modem) }));
        let h = spawn_device_with(
            move || {
                let r = opened.lock().unwrap().take().ok_or_else(|| io::Error::other("already used"));
                Box::pin(async move { r }) as OpenFuture
            },
            POLL,
            FAST_RETRY,
        );
        (h, lines, peer)
    }

    fn is_key(ev: &DeviceEvent) -> bool {
        matches!(ev, DeviceEvent::Key(_))
    }

    #[tokio::test]
    async fn modem_keys_become_device_events() {
        let (h, lines, _peer) = modem_device();
        let mut rx = h.subscribe();
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Connected(true))
        ));
        *lines.lock().unwrap() = Some(LEFT_LINES);
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Event(KeyEvent::Press(Key::Left)))
        ));
        *lines.lock().unwrap() = Some(ModemLines::default());
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Event(KeyEvent::Release(Key::Left)))
        ));
    }

    #[tokio::test]
    async fn modem_error_releases_and_disconnects_keys_once() {
        let (h, lines, peer) = modem_device();
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, is_key).await; // Connected(true)
        *lines.lock().unwrap() = Some(LEFT_LINES);
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Key(KeyInput::Event(KeyEvent::Press(_))))).await;
        *lines.lock().unwrap() = None;
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Event(KeyEvent::Release(Key::Left)))
        ));
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Connected(false))
        ));
        // The data session continues.
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        // Ending the session later does not report the keys as gone a second time.
        peer.abort();
        wait_for(&mut rx, WAIT, |e| {
            assert!(!is_key(e), "{e:?}");
            matches!(e, DeviceEvent::Disconnected { .. })
        })
        .await;
    }

    #[tokio::test]
    async fn session_end_releases_held_key() {
        let (h, lines, peer) = modem_device();
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, is_key).await; // Connected(true)
        *lines.lock().unwrap() = Some(LEFT_LINES);
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Key(KeyInput::Event(KeyEvent::Press(_))))).await;
        peer.abort();
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Event(KeyEvent::Release(Key::Left)))
        ));
        assert!(matches!(
            wait_for(&mut rx, WAIT, is_key).await,
            DeviceEvent::Key(KeyInput::Connected(false))
        ));
    }

    #[tokio::test]
    async fn task_ends_when_handles_dropped() {
        let (client, peer) = tokio::io::duplex(1024);
        let peer = fake_peer(peer, Some(GOOD_ST), Log::default());
        let h = spawn_device_with(once_opener(client), POLL, FAST_RETRY);
        let mut rx = h.subscribe();
        wait_for(&mut rx, WAIT, |e| matches!(e, DeviceEvent::Status(_))).await;
        drop(h);
        timeout(WAIT, peer).await.expect("link not closed").unwrap();
    }
}
