use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::Instrument;

use crate::keypad::{self, KeyDecoder, KeyEvent};
use crate::transport::{DeviceUrl, ModemSource, Opened, open};

/// Bridges one TCP client at a time to the serial port. The port is opened
/// only while a client is connected; the newest client replaces the active one.
/// Handlebar keys read from the modem lines go to every client of `keys_listen`.
pub async fn run(port: String, listen: SocketAddr, keys_listen: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    let keys_listener = bind_keys(keys_listen).await;
    tracing::info!(%listen, %port, "bridge listening");
    let span = tracing::info_span!("bridge", %port);
    let open_port = move || {
        let url = DeviceUrl::Serial(port.clone());
        async move { open(&url).await }
    };
    run_with(open_port, listener, keys_listener).instrument(span).await
}

/// Binds the key port. A taken port costs only the key channel, not the bridge.
async fn bind_keys(keys_listen: SocketAddr) -> Option<TcpListener> {
    match TcpListener::bind(keys_listen).await {
        Ok(listener) => Some(listener),
        Err(err) => {
            tracing::error!(%keys_listen, "cannot listen for key clients: {err} – bridge runs without the key channel");
            None
        }
    }
}

async fn run_with<F, Fut>(open: F, listener: TcpListener, keys_listener: Option<TcpListener>) -> anyhow::Result<()>
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = io::Result<Opened>> + Send + 'static,
{
    // Probe once so a wrong port shows up at startup, not only when the server connects.
    // The bridge keeps running either way: the adapter may be plugged in later.
    match open().await {
        Ok(opened) => {
            drop(opened);
            tracing::info!("serial port opened successfully (probe)");
        }
        Err(err) => tracing::error!("cannot open serial port: {err} – bridge keeps running and retries when a client connects"),
    }
    let (keys, _) = broadcast::channel(KEY_CAPACITY);
    if let Some(keys_listener) = keys_listener {
        tracing::info!("keys on {}", keys_listener.local_addr()?);
        tokio::spawn(serve_keys(keys_listener, keys.clone()).instrument(tracing::Span::current()));
    }
    let mut session: Option<JoinHandle<()>> = None;
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Some(old) = session.take() {
            tracing::info!("closing previous session, newest client wins");
            // Aborting drops the old socket and the serial port it owns.
            old.abort();
            let _ = old.await;
        }
        tracing::info!(%peer, "client connected");
        session = Some(tokio::spawn(serve_client_with(open.clone(), stream, peer, keys.clone()).instrument(tracing::Span::current())));
    }
}

const SERIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const HALF_CLOSE_GRACE: Duration = Duration::from_millis(300);
const OPEN_RETRY_WINDOW: Duration = Duration::from_secs(3);
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(200);
/// Key events buffered per key client before it starts skipping.
const KEY_CAPACITY: usize = 64;
/// Key clients get a `ping` line this often, so they can tell a dead bridge or a
/// stalled stream from a quiet one well before a held LEFT would stop the ride.
pub(crate) const KEY_HEARTBEAT: Duration = Duration::from_millis(250);
/// More key clients than this are turned away.
const MAX_KEY_CLIENTS: usize = 4;

/// Errors that mean "port exists but is not available yet".
fn is_busy(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::Other)
}

/// Opens the serial port, retrying while it is busy: Windows releases the
/// handle of a just-closed port with a delay.
async fn open_with_retry<F, Fut>(open: &F, peer: SocketAddr) -> Option<Opened>
where
    F: Fn() -> Fut,
    Fut: Future<Output = io::Result<Opened>>,
{
    let deadline = Instant::now() + OPEN_RETRY_WINDOW;
    loop {
        match open().await {
            Ok(opened) => return Some(opened),
            Err(err) if !is_busy(&err) || Instant::now() >= deadline => {
                tracing::error!(%peer, "cannot open serial port: {err}");
                return None;
            }
            Err(err) => {
                tracing::debug!(%peer, "serial port busy, retrying: {err}");
                tokio::time::sleep(OPEN_RETRY_INTERVAL).await;
            }
        }
    }
}

async fn serve_client_with<F, Fut>(open: F, stream: TcpStream, peer: SocketAddr, keys: broadcast::Sender<KeyEvent>)
where
    F: Fn() -> Fut,
    Fut: Future<Output = io::Result<Opened>>,
{
    let Some(Opened { link, modem }) = open_with_retry(&open, peer).await else {
        return;
    };
    // Releases a held key however the session ends, also when it is aborted.
    let mut key_session = KeySession { decoder: KeyDecoder::new(), keys, started: Instant::now() };
    let (mut client_rx, mut client_tx) = stream.into_split();
    let (mut link_rx, mut link_tx) = tokio::io::split(link);
    // The session ends as soon as the client closes or either side errors.
    // The serial side never reaches EOF on its own, so the client decides.
    let mut to_serial = 0;
    let mut to_client = 0;
    let result = {
        let up = copy_counted(&mut client_rx, &mut link_tx, &mut to_serial, Some(SERIAL_WRITE_TIMEOUT));
        let down = copy_counted(&mut link_rx, &mut client_tx, &mut to_client, None);
        let sample = key_session.sample(modem);
        tokio::pin!(up, down, sample);
        tokio::select! {
            () = &mut sample => unreachable!("key sampling never ends"),
            r = &mut up => {
                if r.is_ok() {
                    // Client half-closed: let an in-flight reply still reach it.
                    let _ = tokio::time::timeout(HALF_CLOSE_GRACE, &mut down).await;
                }
                r
            }
            r = &mut down => r,
        }
    };
    match result {
        Ok(()) => tracing::info!(%peer, to_serial, to_client, "client disconnected"),
        Err(err) => tracing::warn!(%peer, to_serial, to_client, "bridge session ended: {err}"),
    }
    // Dropping the halves closes the serial port and the socket.
}

/// Decodes the modem lines of one serial session into key events.
struct KeySession {
    decoder: KeyDecoder,
    keys: broadcast::Sender<KeyEvent>,
    started: Instant,
}

impl KeySession {
    /// Samples every [`keypad::POLL`] and publishes the decoded events. Never returns:
    /// without a modem, or after a read error, it just waits for the session to end.
    async fn sample(&mut self, modem: Option<Arc<dyn ModemSource>>) {
        if let Some(modem) = modem {
            let mut tick = tokio::time::interval(keypad::POLL);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut seen = None;
            loop {
                tick.tick().await;
                let lines = match modem.read() {
                    Ok(lines) => lines,
                    Err(err) => {
                        tracing::warn!("modem lines unreadable, key sampling stopped: {err}");
                        // A lost line source counts as released.
                        self.release();
                        break;
                    }
                };
                // Raw line changes, for checking a keypad (RUST_LOG=kettler=debug).
                if seen != Some(lines) {
                    tracing::debug!(?lines, "modem lines");
                    seen = Some(lines);
                }
                if let Some(ev) = self.decoder.update(lines, self.started.elapsed()) {
                    tracing::info!(?ev, "key");
                    // Having no key client is fine.
                    let _ = self.keys.send(ev);
                }
            }
        }
        std::future::pending().await
    }

    /// Publishes the release of a running press, if any.
    fn release(&mut self) {
        if let Some(ev) = self.decoder.reset() {
            let _ = self.keys.send(ev);
        }
    }
}

impl Drop for KeySession {
    fn drop(&mut self) {
        self.release();
    }
}

/// Accepts up to [`MAX_KEY_CLIENTS`] key clients; each gets every key event as a line.
async fn serve_keys(listener: TcpListener, keys: broadcast::Sender<KeyEvent>) {
    let slots = Arc::new(Semaphore::new(MAX_KEY_CLIENTS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "key accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!(%peer, "too many key clients, connection closed");
            continue;
        };
        tracing::info!(%peer, "key client connected");
        // Key lines are tiny; do not let Nagle hold a release back.
        if let Err(err) = stream.set_nodelay(true) {
            tracing::debug!(%peer, "key client: cannot set TCP_NODELAY: {err}");
        }
        let rx = keys.subscribe();
        tokio::spawn(
            async move {
                serve_key_client(stream, peer, rx).await;
                drop(slot);
            }
            .instrument(tracing::Span::current()),
        );
    }
}

/// Writes key events to one client until it closes or a write fails.
/// What the client sends is read only to notice that it is gone.
async fn serve_key_client(stream: TcpStream, peer: SocketAddr, mut rx: broadcast::Receiver<KeyEvent>) {
    let (mut client_rx, mut client_tx) = stream.into_split();
    let mut discard = [0u8; 256];
    let mut heartbeat = tokio::time::interval_at(Instant::now() + KEY_HEARTBEAT, KEY_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let ev = tokio::select! {
            _ = heartbeat.tick() => {
                if let Err(err) = client_tx.write_all(b"ping\n").await {
                    tracing::info!(%peer, "key client disconnected: {err}");
                    return;
                }
                continue;
            }
            read = client_rx.read(&mut discard) => match read {
                Ok(0) => {
                    tracing::info!(%peer, "key client disconnected");
                    return;
                }
                Ok(_) => continue,
                Err(err) => {
                    tracing::info!(%peer, "key client disconnected: {err}");
                    return;
                }
            },
            ev = rx.recv() => match ev {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(%peer, missed, "key client lagging, events skipped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        if let Err(err) = client_tx.write_all(keypad::encode(ev).as_bytes()).await {
            tracing::info!(%peer, "key client disconnected: {err}");
            return;
        }
    }
}

/// Copies until EOF or error, keeping the byte count visible to the caller
/// even when the future is cancelled by the other direction finishing.
/// With a write timeout, a stalled writer ends the copy with `TimedOut`.
async fn copy_counted<R, W>(
    reader: &mut R,
    writer: &mut W,
    total: &mut u64,
    write_timeout: Option<Duration>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let write = async {
            writer.write_all(&buf[..n]).await?;
            writer.flush().await
        };
        match write_timeout {
            Some(limit) => tokio::time::timeout(limit, write)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write to serial port timed out"))??,
            None => write.await?,
        }
        *total += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::DuplexStream;

    use crate::keypad::ModemLines;
    use crate::transport::ModemSource;

    async fn tcp_pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        (client, server, peer)
    }

    fn duplex_opener(
        link: DuplexStream,
        allow: Arc<AtomicBool>,
    ) -> impl Fn() -> std::future::Ready<io::Result<Opened>> + Clone + Send + Sync + 'static {
        let link = Arc::new(std::sync::Mutex::new(Some(link)));
        move || {
            let taken = if allow.load(Ordering::SeqCst) { link.lock().unwrap().take() } else { None };
            std::future::ready(match taken {
                Some(link) => Ok(Opened::plain(Box::new(link))),
                None => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            })
        }
    }

    fn spawn_session(link: DuplexStream, server: TcpStream, peer: SocketAddr) -> JoinHandle<()> {
        let allow = Arc::new(AtomicBool::new(true));
        tokio::spawn(serve_client_with(duplex_opener(link, allow), server, peer, no_keys()))
    }

    fn no_keys() -> broadcast::Sender<KeyEvent> {
        broadcast::channel(KEY_CAPACITY).0
    }

    async fn keys_listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// Modem lines the test sets by hand; `None` makes every read fail.
    type FakeLines = Arc<std::sync::Mutex<Option<ModemLines>>>;

    struct FakeModem(FakeLines);

    impl ModemSource for FakeModem {
        fn read(&self) -> io::Result<ModemLines> {
            self.0.lock().unwrap().ok_or_else(|| io::Error::other("modem gone"))
        }
    }

    fn fake_lines(lines: ModemLines) -> FakeLines {
        Arc::new(std::sync::Mutex::new(Some(lines)))
    }

    /// Opener that hands out a fresh duplex link on every call (the startup probe
    /// takes one too), with the given modem. The device ends are kept open.
    fn modem_opener(
        modem: Option<FakeLines>,
    ) -> impl Fn() -> std::future::Ready<io::Result<Opened>> + Clone + Send + Sync + 'static {
        let devices = Arc::new(std::sync::Mutex::new(Vec::new()));
        move || {
            let (link, device) = tokio::io::duplex(64);
            devices.lock().unwrap().push(device);
            let modem = modem.clone().map(|m| Arc::new(FakeModem(m)) as Arc<dyn ModemSource>);
            std::future::ready(Ok(Opened { link: Box::new(link), modem }))
        }
    }

    /// Starts a bridge and returns its data and key addresses.
    async fn spawn_bridge<F, Fut>(open: F) -> (SocketAddr, SocketAddr)
    where
        F: Fn() -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = io::Result<Opened>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let keys_listener = keys_listener().await;
        let addrs = (listener.local_addr().unwrap(), keys_listener.local_addr().unwrap());
        tokio::spawn(run_with(open, listener, Some(keys_listener)));
        addrs
    }

    /// The next key line, skipping heartbeats.
    async fn read_key_line(reader: &mut tokio::io::BufReader<TcpStream>) -> String {
        use tokio::io::AsyncBufReadExt;
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
                .await
                .expect("no key line")
                .unwrap();
            if line != "ping\n" {
                return line;
            }
        }
    }

    const UP_LINES: ModemLines = ModemLines { cts: true, dsr: false, ri: true, dcd: false };
    const LEFT_LINES: ModemLines = ModemLines { cts: false, dsr: false, ri: false, dcd: true };

    #[tokio::test]
    async fn key_presses_reach_key_clients() {
        let lines = fake_lines(ModemLines::default());
        let (data, keys) = spawn_bridge(modem_opener(Some(lines.clone()))).await;
        let _client = TcpStream::connect(data).await.unwrap();
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        *lines.lock().unwrap() = Some(UP_LINES);
        tokio::time::sleep(Duration::from_millis(200)).await;
        *lines.lock().unwrap() = Some(ModemLines::default());
        assert_eq!(read_key_line(&mut key_client).await, "press UP\n");
        assert_eq!(read_key_line(&mut key_client).await, "release UP\n");
    }

    #[tokio::test]
    async fn session_end_releases_held_key() {
        let lines = fake_lines(ModemLines::default());
        let (data, keys) = spawn_bridge(modem_opener(Some(lines.clone()))).await;
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        *lines.lock().unwrap() = Some(LEFT_LINES);
        let client = TcpStream::connect(data).await.unwrap();
        assert_eq!(read_key_line(&mut key_client).await, "press LEFT\n");
        drop(client);
        assert_eq!(read_key_line(&mut key_client).await, "release LEFT\n");
    }

    #[tokio::test]
    async fn modem_error_releases_held_key() {
        let lines = fake_lines(LEFT_LINES);
        let (data, keys) = spawn_bridge(modem_opener(Some(lines.clone()))).await;
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _client = TcpStream::connect(data).await.unwrap();
        assert_eq!(read_key_line(&mut key_client).await, "press LEFT\n");
        *lines.lock().unwrap() = None;
        assert_eq!(read_key_line(&mut key_client).await, "release LEFT\n");
    }

    #[tokio::test]
    async fn aborted_session_releases_held_key() {
        let lines = fake_lines(LEFT_LINES);
        let (data, keys) = spawn_bridge(modem_opener(Some(lines.clone()))).await;
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _first = TcpStream::connect(data).await.unwrap();
        assert_eq!(read_key_line(&mut key_client).await, "press LEFT\n");
        let _second = TcpStream::connect(data).await.unwrap();
        assert_eq!(read_key_line(&mut key_client).await, "release LEFT\n");
        // The new session sees the still-held key as a new press.
        assert_eq!(read_key_line(&mut key_client).await, "press LEFT\n");
    }

    #[tokio::test]
    async fn key_clients_are_capped_and_reaped() {
        let lines = fake_lines(ModemLines::default());
        let (data, keys) = spawn_bridge(modem_opener(Some(lines.clone()))).await;
        let mut clients = Vec::new();
        for _ in 0..MAX_KEY_CLIENTS {
            clients.push(TcpStream::connect(keys).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut extra = TcpStream::connect(keys).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), extra.read(&mut buf)).await.expect("extra client not closed");
        assert!(matches!(read, Ok(0) | Err(_)), "extra client got {read:?}");

        drop(clients.pop());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut late = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _client = TcpStream::connect(data).await.unwrap();
        *lines.lock().unwrap() = Some(UP_LINES);
        assert_eq!(read_key_line(&mut late).await, "press UP\n");
    }

    #[tokio::test]
    async fn key_clients_get_heartbeat() {
        let (_data, keys) = spawn_bridge(modem_opener(None)).await;
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        let started = std::time::Instant::now();
        let mut line = String::new();
        tokio::time::timeout(KEY_HEARTBEAT * 2, tokio::io::AsyncBufReadExt::read_line(&mut key_client, &mut line))
            .await
            .expect("no heartbeat")
            .unwrap();
        assert_eq!(line, "ping\n");
        assert!(started.elapsed() >= KEY_HEARTBEAT - Duration::from_millis(50));
    }

    #[tokio::test]
    async fn taken_key_port_leaves_data_port_working() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(bind_keys(taken.local_addr().unwrap()).await.is_none());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (link, mut device) = tokio::io::duplex(64);
        // Closed for the startup probe, so the link is left for the client.
        let allow = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_with(duplex_opener(link, allow.clone()), listener, None));
        tokio::time::sleep(Duration::from_millis(100)).await;
        allow.store(true, Ordering::SeqCst);
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"ID\r\n").await.unwrap();
        let mut got = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), device.read_exact(&mut got)).await.unwrap().unwrap();
        assert_eq!(&got, b"ID\r\n");
    }

    #[tokio::test]
    async fn no_modem_no_key_lines() {
        let (data, keys) = spawn_bridge(modem_opener(None)).await;
        let _client = TcpStream::connect(data).await.unwrap();
        let mut key_client = tokio::io::BufReader::new(TcpStream::connect(keys).await.unwrap());
        let read = tokio::time::timeout(Duration::from_millis(600), read_key_line(&mut key_client)).await;
        assert!(read.is_err(), "key client got {read:?}");
    }

    #[tokio::test]
    async fn session_ends_when_client_closes() {
        let (client, server, peer) = tcp_pair().await;
        let (link, mut device) = tokio::io::duplex(64);
        let session = spawn_session(link, server, peer);
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), session).await.expect("session did not end").unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(device.read(&mut buf).await.unwrap(), 0, "link was not dropped");
    }

    #[tokio::test]
    async fn open_is_retried_until_it_succeeds() {
        let (mut client, server, peer) = tcp_pair().await;
        let (link, mut device) = tokio::io::duplex(64);
        let link = std::sync::Mutex::new(Some(link));
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let session = tokio::spawn(async move {
            serve_client_with(
                || {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let link = if n < 2 { None } else { link.lock().unwrap().take() };
                    async move {
                        match link {
                            Some(link) => Ok(Opened::plain(Box::new(link))),
                            None => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                        }
                    }
                },
                server,
                peer,
                no_keys(),
            )
            .await
        });
        client.write_all(b"ID\r\n").await.unwrap();
        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), device.read_exact(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf, b"ID\r\n");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        drop(client);
        session.await.unwrap();
    }

    #[tokio::test]
    async fn open_gives_up_after_three_seconds() {
        let (mut client, server, peer) = tcp_pair().await;
        let started = std::time::Instant::now();
        let session = tokio::spawn(serve_client_with(
            || async { Err::<Opened, _>(io::Error::from(io::ErrorKind::PermissionDenied)) },
            server,
            peer,
            no_keys(),
        ));
        tokio::time::timeout(Duration::from_millis(3500), session).await.expect("did not give up").unwrap();
        assert!(started.elapsed() >= Duration::from_secs(3));
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).await.unwrap(), 0, "client socket not closed");
    }

    #[tokio::test]
    async fn open_does_not_retry_other_errors() {
        let (mut client, server, peer) = tcp_pair().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let session = tokio::spawn(serve_client_with(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err::<Opened, _>(io::Error::from(io::ErrorKind::NotFound)) }
            },
            server,
            peer,
            no_keys(),
        ));
        tokio::time::timeout(Duration::from_millis(500), session).await.expect("retried").unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn newest_client_aborts_session_in_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (link, mut device) = tokio::io::duplex(64);
        let allow = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_with(duplex_opener(link, allow.clone()), listener, Some(keys_listener().await)));

        let mut a = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await; // A is retrying
        let mut b = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let a_read = tokio::time::timeout(Duration::from_secs(1), a.read(&mut buf)).await.expect("A not closed");
        assert!(matches!(a_read, Ok(0) | Err(_)));

        allow.store(true, Ordering::SeqCst);
        b.write_all(b"ID\r\n").await.unwrap();
        let mut got = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), device.read_exact(&mut got)).await.unwrap().unwrap();
        assert_eq!(&got, b"ID\r\n");
    }

    #[tokio::test]
    async fn port_is_probed_once_at_startup_and_bridge_keeps_running() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let bridge = tokio::spawn(run_with(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err::<Opened, _>(io::Error::from(io::ErrorKind::NotFound)) }
            },
            listener,
            Some(keys_listener().await),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!bridge.is_finished());
        // Still accepting: a client triggers the next attempt.
        let _client = TcpStream::connect(addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no second open attempt");
    }

    #[tokio::test]
    async fn serial_eof_ends_session() {
        let (mut client, server, peer) = tcp_pair().await;
        let (link, device) = tokio::io::duplex(64);
        let session = spawn_session(link, server, peer);
        drop(device);
        tokio::time::timeout(Duration::from_secs(1), session).await.expect("session did not end").unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn bytes_pass_both_ways_unchanged() {
        let (mut client, server, peer) = tcp_pair().await;
        let (link, mut device) = tokio::io::duplex(64);
        let _session = spawn_session(link, server, peer);
        let up: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let down: Vec<u8> = (0..5000u32).map(|i| (i % 241) as u8).collect();
        let (mut client_rx, mut client_tx) = client.split();
        let (mut dev_rx, mut dev_tx) = tokio::io::split(&mut device);
        let work = async {
            let mut got_up = vec![0u8; 5000];
            let mut got_down = vec![0u8; 5000];
            let (a, b, c, d) = tokio::join!(
                client_tx.write_all(&up),
                dev_tx.write_all(&down),
                dev_rx.read_exact(&mut got_up),
                client_rx.read_exact(&mut got_down),
            );
            a.unwrap();
            b.unwrap();
            c.unwrap();
            d.unwrap();
            (got_up, got_down)
        };
        let (got_up, got_down) = tokio::time::timeout(Duration::from_secs(5), work).await.unwrap();
        assert_eq!(got_up, up);
        assert_eq!(got_down, down);
    }

    #[tokio::test]
    #[allow(deprecated)] // SO_LINGER 0 is exactly what forces the RST this test needs
    async fn client_reset_ends_session() {
        let (client, server, peer) = tcp_pair().await;
        let (link, _device) = tokio::io::duplex(64);
        let session = spawn_session(link, server, peer);
        client.set_linger(Some(Duration::ZERO)).unwrap();
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), session).await.expect("session did not end").unwrap();
    }

    #[tokio::test]
    async fn half_close_still_delivers_reply() {
        let (mut client, server, peer) = tcp_pair().await;
        let (link, mut device) = tokio::io::duplex(64);
        let _session = spawn_session(link, server, peer);
        client.write_all(b"ID\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let mut cmd = [0u8; 4];
        device.read_exact(&mut cmd).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        device.write_all(b"AX1S\r\n").await.unwrap();
        let mut reply = [0u8; 6];
        tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut reply)).await.unwrap().unwrap();
        assert_eq!(&reply, b"AX1S\r\n");
    }

    #[tokio::test]
    async fn stalled_serial_write_ends_session() {
        let (mut client, server, peer) = tcp_pair().await;
        let (link, _device) = tokio::io::duplex(8); // never read: writes stall
        let session = spawn_session(link, server, peer);
        client.write_all(&[0u8; 64]).await.unwrap();
        tokio::time::timeout(Duration::from_millis(3000), session).await.expect("stall not detected").unwrap();
    }
}
