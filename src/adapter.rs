//! The USB serial adapter behind a port, from Linux sysfs (vendor, product, serial number).

use std::path::Path;

use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Adapter {
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

/// How far above the tty the USB device may sit (tty → interface → device).
const MAX_LEVELS: usize = 4;

/// `port` as in `KETTLER_DEVICE`, e.g. `/dev/ttyUSB0` or a `/dev/serial/by-id/…` link.
/// `None` without sysfs (Windows, no USB, no access).
pub fn adapter_info(port: &str) -> Option<Adapter> {
    adapter_info_in(Path::new("/sys"), port)
}

pub fn adapter_info_in(sys: &Path, port: &str) -> Option<Adapter> {
    let resolved = std::fs::canonicalize(port).unwrap_or_else(|_| port.into());
    let name = resolved.file_name()?.to_str()?.to_string();
    let device = std::fs::canonicalize(sys.join("class/tty").join(name).join("device")).ok()?;
    usb_attributes(&device)
}

/// The USB device at or above `start`: the first directory with an `idVendor` file. Its
/// strings are optional; without a product name, vendor and product id stand in.
fn usb_attributes(start: &Path) -> Option<Adapter> {
    let read = |dir: &Path, file: &str| {
        std::fs::read_to_string(dir.join(file)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let dir = start.ancestors().take(MAX_LEVELS + 1).find(|dir| dir.join("idVendor").is_file())?;
    let ids = || Some(format!("USB {}:{}", read(dir, "idVendor")?, read(dir, "idProduct")?));
    Some(Adapter {
        manufacturer: read(dir, "manufacturer"),
        product: read(dir, "product").or_else(ids),
        serial: read(dir, "serial"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sysfs as Linux has it for an FTDI adapter on ttyUSB0.
    #[cfg(unix)]
    fn ftdi_sysfs(root: &Path) {
        let usb = root.join("devices/pci0000:00/usb1/1-1");
        let tty = usb.join("1-1:1.0/ttyUSB0");
        std::fs::create_dir_all(&tty).unwrap();
        std::fs::write(usb.join("idVendor"), "0403\n").unwrap();
        std::fs::write(usb.join("manufacturer"), "FTDI\n").unwrap();
        std::fs::write(usb.join("product"), "FT232R USB UART\n").unwrap();
        std::fs::write(usb.join("serial"), "A1B2C3D4\n").unwrap();
        let class = root.join("class/tty/ttyUSB0");
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(&tty, class.join("device")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reads_ftdi_from_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        ftdi_sysfs(dir.path());
        let expected = Adapter {
            manufacturer: Some("FTDI".into()),
            product: Some("FT232R USB UART".into()),
            serial: Some("A1B2C3D4".into()),
        };
        assert_eq!(adapter_info_in(dir.path(), "/dev/ttyUSB0"), Some(expected));
    }

    #[test]
    fn walks_up_from_the_tty_to_the_usb_device() {
        let dir = tempfile::tempdir().unwrap();
        let usb = dir.path().join("1-1");
        // Linux names the interface "1-1:1.0"; Windows allows no colon in names.
        let tty = usb.join("1-1.1.0/ttyUSB0");
        std::fs::create_dir_all(&tty).unwrap();
        std::fs::write(usb.join("idVendor"), "0403\n").unwrap();
        std::fs::write(usb.join("serial"), "A1B2C3D4\n").unwrap();
        let found = usb_attributes(&tty).unwrap();
        assert_eq!(found.serial.as_deref(), Some("A1B2C3D4"));
        assert_eq!(found.product, None);
    }

    #[test]
    fn adapter_without_strings_does_not_borrow_the_hub() {
        // A cheap clone has idVendor/idProduct but no product or serial string;
        // the root hub above it has both and must not be taken for the adapter.
        let dir = tempfile::tempdir().unwrap();
        let hub = dir.path().join("usb1");
        let usb = hub.join("1-1");
        let tty = usb.join("1-1.1.0/ttyUSB0");
        std::fs::create_dir_all(&tty).unwrap();
        std::fs::write(usb.join("idVendor"), "067b
").unwrap();
        std::fs::write(usb.join("idProduct"), "2303
").unwrap();
        std::fs::write(hub.join("idVendor"), "1d6b
").unwrap();
        std::fs::write(hub.join("product"), "xHCI Host Controller
").unwrap();
        std::fs::write(hub.join("serial"), "0000:00:14.0
").unwrap();
        let found = usb_attributes(&tty).unwrap();
        assert_eq!(found.product.as_deref(), Some("USB 067b:2303"));
        assert_eq!(found.serial, None);
    }

    #[test]
    fn missing_sysfs_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(adapter_info_in(dir.path(), "/dev/ttyUSB0"), None);
        assert_eq!(adapter_info_in(dir.path(), "COM3"), None);
    }
}
