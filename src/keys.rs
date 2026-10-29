//! Where the server gets handlebar key events from: the bridge's key port, the
//! serial port it drives itself, or nowhere.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::key_actions::KeyInput;
use crate::keypad;
use crate::transport::DeviceUrl;

/// Port the bridge publishes key lines on.
const BRIDGE_KEYS_PORT: u16 = 4002;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// A key connection that stays silent this long is considered lost: the bridge
/// sends a `ping` every 250 ms, so silence means a dead host or a stalled stream,
/// not a quiet keypad. Well below the 2 s LEFT stop, so a stalled release ends as
/// a lost connection (no stop) instead of a held key.
const KEY_SILENCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeysSource {
    Off,
    /// Key lines from a bridge at `host:port`.
    Tcp(String),
    /// The modem lines of the device's own serial port.
    Device,
}

/// Resolves `KETTLER_KEYS`: `off`, `tcp://host:port`, or (unset) derived from the device.
pub fn resolve(setting: Option<&str>, device: &DeviceUrl) -> Result<KeysSource, String> {
    match setting.map(str::trim).filter(|s| !s.is_empty()) {
        Some("off") => Ok(KeysSource::Off),
        Some(s) => match s.strip_prefix("tcp://").filter(|a| !a.is_empty()) {
            Some(addr) => Ok(KeysSource::Tcp(addr.into())),
            None => Err(format!("unsupported KETTLER_KEYS {s:?}; use off or tcp://host:port")),
        },
        None => match device {
            DeviceUrl::Tcp(addr) => bridge_keys_addr(addr).map(KeysSource::Tcp),
            DeviceUrl::Serial(_) => Ok(KeysSource::Device),
            DeviceUrl::Sim => Ok(KeysSource::Off),
        },
    }
}

/// The device address with its port replaced by the bridge's key port.
fn bridge_keys_addr(device_addr: &str) -> Result<String, String> {
    let no_port = || format!("device address {device_addr:?} has no port; set KETTLER_KEYS to tcp://host:port or off");
    let (host, port) = device_addr.rsplit_once(':').ok_or_else(no_port)?;
    if host.is_empty() || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(no_port());
    }
    Ok(format!("{host}:{BRIDGE_KEYS_PORT}"))
}

/// Connects to a bridge's key port and feeds `sink` until the process ends. Sends
/// `Connected(true)` once connected and `Connected(false)` when the connection ends;
/// reconnects every 2 s.
pub fn spawn_tcp(addr: String, sink: impl Fn(KeyInput) + Send + Sync + 'static) {
    tokio::spawn(async move {
        let mut announced_failure = false;
        loop {
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    announced_failure = false;
                    // Key lines are tiny; do not let Nagle hold a release back.
                    if let Err(err) = stream.set_nodelay(true) {
                        tracing::debug!(%addr, "keys: cannot set TCP_NODELAY: {err}");
                    }
                    tracing::info!(%addr, "keys connected");
                    sink(KeyInput::Connected(true));
                    read_lines(stream, &sink).await;
                    sink(KeyInput::Connected(false));
                }
                Err(err) if !announced_failure => {
                    announced_failure = true;
                    tracing::info!(%addr, "keys not reachable at {addr}: {err}");
                }
                Err(err) => tracing::debug!(%addr, "keys not reachable at {addr}: {err}"),
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    });
}

/// Reads key lines until EOF, an error or [`KEY_SILENCE`] without any line.
async fn read_lines(stream: TcpStream, sink: &impl Fn(KeyInput)) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match timeout(KEY_SILENCE, lines.next_line()).await {
            Ok(Ok(Some(line))) => match keypad::parse(&line) {
                Some(ev) => sink(KeyInput::Event(ev)),
                // The bridge's heartbeat.
                None if line.trim() == "ping" => {}
                None => tracing::debug!(%line, "unparsable key line"),
            },
            Ok(Ok(None)) => {
                tracing::info!("key connection closed");
                return;
            }
            Ok(Err(err)) => {
                tracing::info!("key connection failed: {err}");
                return;
            }
            Err(_) => {
                tracing::info!("key connection silent, treated as lost");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypad::{Key, KeyEvent};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    fn tcp(addr: &str) -> DeviceUrl {
        DeviceUrl::Tcp(addr.into())
    }

    #[test]
    fn resolve_defaults() {
        let serial = DeviceUrl::Serial("COM3".into());
        assert_eq!(resolve(None, &tcp("10.0.0.5:4001")), Ok(KeysSource::Tcp("10.0.0.5:4002".into())));
        assert_eq!(resolve(None, &serial), Ok(KeysSource::Device));
        assert_eq!(resolve(None, &DeviceUrl::Sim), Ok(KeysSource::Off));
        assert_eq!(resolve(Some("off"), &serial), Ok(KeysSource::Off));
        assert_eq!(resolve(Some("tcp://h:9"), &serial), Ok(KeysSource::Tcp("h:9".into())));
        assert!(resolve(Some("udp://x"), &serial).is_err());
        assert_eq!(resolve(None, &tcp("[::1]:4001")), Ok(KeysSource::Tcp("[::1]:4002".into())));
    }

    #[test]
    fn empty_setting_means_automatic() {
        // compose passes `KETTLER_KEYS=${KETTLER_KEYS:-}`, so unset arrives as "".
        let serial = DeviceUrl::Serial("COM3".into());
        assert_eq!(resolve(Some(""), &serial), Ok(KeysSource::Device));
        assert_eq!(resolve(Some("  "), &tcp("10.0.0.5:4001")), Ok(KeysSource::Tcp("10.0.0.5:4002".into())));
        assert_eq!(resolve(Some(""), &DeviceUrl::Sim), Ok(KeysSource::Off));
    }

    #[test]
    fn device_address_without_port_is_an_error() {
        assert!(resolve(None, &tcp("host")).is_err());
        assert!(resolve(None, &tcp("[::1]")).is_err());
        assert!(resolve(None, &tcp("host:")).is_err());
        assert_eq!(resolve(Some("off"), &tcp("host")), Ok(KeysSource::Off));
    }

    fn channel_sink() -> (impl Fn(KeyInput) + Send + Sync + 'static, mpsc::UnboundedReceiver<KeyInput>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (move |input| tx.send(input).unwrap_or_default(), rx)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<KeyInput>, limit: Duration) -> KeyInput {
        timeout(limit, rx.recv()).await.expect("timed out").expect("sink dropped")
    }

    #[tokio::test]
    async fn tcp_keys_are_delivered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"press UP\nnonsense\nrelease UP\n").await.unwrap();
        });
        let (sink, mut rx) = channel_sink();
        spawn_tcp(addr, sink);
        let limit = Duration::from_secs(2);
        assert_eq!(next(&mut rx, limit).await, KeyInput::Connected(true));
        assert_eq!(next(&mut rx, limit).await, KeyInput::Event(KeyEvent::Press(Key::Up)));
        assert_eq!(next(&mut rx, limit).await, KeyInput::Event(KeyEvent::Release(Key::Up)));
        assert_eq!(next(&mut rx, limit).await, KeyInput::Connected(false));
    }

    #[tokio::test]
    async fn silent_key_channel_counts_as_lost() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // Accepts and then says nothing, like a bridge host that died without a FIN.
        let _server = tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });
        let (sink, mut rx) = channel_sink();
        spawn_tcp(addr, sink);
        assert_eq!(next(&mut rx, Duration::from_secs(1)).await, KeyInput::Connected(true));
        assert_eq!(next(&mut rx, KEY_SILENCE * 2).await, KeyInput::Connected(false));
    }

    #[tokio::test]
    async fn stalled_press_is_released_before_stop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // A press, then a stalled stream: the release never arrives in time.
        let _server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"press LEFT\n").await.unwrap();
            std::future::pending::<()>().await
        });
        let (sink, mut rx) = channel_sink();
        spawn_tcp(addr, sink);
        let limit = Duration::from_secs(1);
        assert_eq!(next(&mut rx, limit).await, KeyInput::Connected(true));
        assert_eq!(next(&mut rx, limit).await, KeyInput::Event(KeyEvent::Press(Key::Left)));
        let pressed = std::time::Instant::now();
        assert_eq!(next(&mut rx, crate::key_actions::STOP_AFTER).await, KeyInput::Connected(false));
        assert!(pressed.elapsed() < crate::key_actions::STOP_AFTER, "released only after {:?}", pressed.elapsed());
    }

    #[test]
    fn silence_sits_between_heartbeat_and_stop() {
        assert!(KEY_SILENCE < crate::key_actions::STOP_AFTER);
        assert!(crate::bridge::KEY_HEARTBEAT * 2 < KEY_SILENCE);
    }

    #[tokio::test]
    async fn missing_key_port_does_not_disturb() {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let (sink, mut rx) = channel_sink();
        spawn_tcp(addr.to_string(), sink);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(rx.try_recv().is_err(), "unexpected input while nothing listens");

        let listener = TcpListener::bind(addr).await.unwrap();
        let _accepted = tokio::spawn(async move { listener.accept().await });
        assert_eq!(next(&mut rx, Duration::from_secs(3)).await, KeyInput::Connected(true));
    }
}
