use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPort, SerialPortBuilderExt, SerialStream, StopBits};

use crate::keypad::ModemLines;
use crate::sim::{SimErgometer, spawn_duplex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceUrl {
    Tcp(String),
    Serial(String),
    Sim,
}

impl FromStr for DeviceUrl {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(addr) = s.strip_prefix("tcp://").filter(|r| !r.is_empty()) {
            Ok(Self::Tcp(addr.into()))
        } else if let Some(port) = s.strip_prefix("serial://").filter(|r| !r.is_empty()) {
            Ok(Self::Serial(port.into()))
        } else if s == "sim://" {
            Ok(Self::Sim)
        } else {
            Err(format!("unsupported device URL {s:?}; use tcp://host:port, serial://COM3, serial:///dev/ttyUSB0 or sim://"))
        }
    }
}

pub trait Link: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Link for T {}

/// Reads the input lines of a serial port (where the handlebar keypad is wired).
pub trait ModemSource: Send + Sync {
    fn read(&self) -> io::Result<ModemLines>;
}

/// An open device connection; `modem` is only there for a real serial port.
pub struct Opened {
    pub link: Box<dyn Link>,
    pub modem: Option<Arc<dyn ModemSource>>,
}

impl Opened {
    /// A link without modem lines.
    pub fn plain(link: Box<dyn Link>) -> Self {
        Self { link, modem: None }
    }
}

pub async fn open(url: &DeviceUrl) -> io::Result<Opened> {
    match url {
        DeviceUrl::Tcp(addr) => Ok(Opened::plain(Box::new(TcpStream::connect(addr).await?))),
        DeviceUrl::Serial(port) => {
            let mut stream = tokio_serial::new(port, 9600)
                .data_bits(DataBits::Eight)
                .parity(Parity::None)
                .stop_bits(StopBits::One)
                .flow_control(FlowControl::None)
                .dtr_on_open(true)
                .open_native_async()?;
            // The keypad is powered by DTR (UP, LEFT, RIGHT) and RTS (DOWN). On Windows the
            // async open closes and reopens the port, which drops the DTR set above, and
            // without flow control RTS stays off, so both are set on the final handle.
            if let Err(err) = stream.write_data_terminal_ready(true) {
                tracing::warn!("cannot set DTR, the handlebar keys get no power: {err}");
            }
            if let Err(err) = stream.write_request_to_send(true) {
                tracing::warn!("cannot set RTS, the DOWN key gets no power: {err}");
            }
            let shared = Arc::new(Mutex::new(stream));
            Ok(Opened {
                link: Box::new(SerialLink(shared.clone())),
                modem: Some(Arc::new(SerialModem(shared))),
            })
        }
        DeviceUrl::Sim => {
            let sim = Arc::new(Mutex::new(SimErgometer::new()));
            {
                let mut sim = sim.lock().unwrap();
                sim.set_rpm(88);
                sim.set_lively(true);
            }
            Ok(Opened::plain(Box::new(spawn_duplex(sim))))
        }
    }
}

/// Locks the port; a poisoned lock still holds a usable port.
fn lock(port: &Mutex<SerialStream>) -> MutexGuard<'_, SerialStream> {
    port.lock().unwrap_or_else(|e| e.into_inner())
}

/// The data side of a serial port whose modem lines are read elsewhere.
/// `SerialStream` cannot be cloned, so both share it; each poll holds the lock only briefly.
struct SerialLink(Arc<Mutex<SerialStream>>);

impl AsyncRead for SerialLink {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *lock(&self.0)).poll_read(cx, buf)
    }
}

impl AsyncWrite for SerialLink {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut *lock(&self.0)).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *lock(&self.0)).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *lock(&self.0)).poll_shutdown(cx)
    }
}

/// The modem lines of the port shared with a [`SerialLink`].
struct SerialModem(Arc<Mutex<SerialStream>>);

impl ModemSource for SerialModem {
    fn read(&self) -> io::Result<ModemLines> {
        let mut port = lock(&self.0);
        Ok(ModemLines {
            cts: port.read_clear_to_send()?,
            dsr: port.read_data_set_ready()?,
            ri: port.read_ring_indicator()?,
            dcd: port.read_carrier_detect()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn parses_device_urls() {
        assert_eq!("tcp://192.168.1.50:4001".parse::<DeviceUrl>().unwrap(), DeviceUrl::Tcp("192.168.1.50:4001".into()));
        assert_eq!("serial://COM3".parse::<DeviceUrl>().unwrap(), DeviceUrl::Serial("COM3".into()));
        assert_eq!("serial:///dev/ttyUSB0".parse::<DeviceUrl>().unwrap(), DeviceUrl::Serial("/dev/ttyUSB0".into()));
        assert_eq!("sim://".parse::<DeviceUrl>().unwrap(), DeviceUrl::Sim);
        assert!("COM3".parse::<DeviceUrl>().is_err());
        assert!("tcp://".parse::<DeviceUrl>().is_err());
        assert!("serial://".parse::<DeviceUrl>().is_err());
    }

    #[tokio::test]
    async fn tcp_link_talks_to_simulator() {
        let (addr, _h) = crate::sim::bind_tcp("127.0.0.1:0".parse().unwrap(), 70).await.unwrap();
        let opened = open(&DeviceUrl::Tcp(addr.to_string())).await.unwrap();
        assert!(opened.modem.is_none());
        let mut link = opened.link;
        link.write_all(b"ID\r\n").await.unwrap();
        let mut buf = [0u8; 6];
        link.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"AX1S\r\n");
    }

    #[tokio::test]
    async fn sim_link_talks_to_simulator() {
        let opened = open(&DeviceUrl::Sim).await.unwrap();
        assert!(opened.modem.is_none());
        let mut link = opened.link;
        link.write_all(b"ID\r\n").await.unwrap();
        let mut buf = [0u8; 6];
        link.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"AX1S\r\n");
    }
}
