use crate::Host;
use flame_api::NodePciDevice;
use std::path::Path;

fn attribute(host: &mut impl Host, device: &Path, name: &str) -> Option<String> {
    let bytes = host.read(&device.join(name)).ok()??;
    Some(String::from_utf8(bytes).ok()?.trim().to_owned())
}

fn hex(value: &str, width: usize) -> Option<u32> {
    if value.len() == width && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        u32::from_str_radix(value, 16).ok()
    } else {
        None
    }
}

pub(crate) fn pci_address(value: &str) -> Option<String> {
    let (domain, rest) = value.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (device, function) = rest.split_once('.')?;
    if !(4..=8).contains(&domain.len())
        || hex(domain, domain.len()).is_none()
        || hex(bus, 2).is_none()
        || hex(device, 2)? > 31
        || hex(function, 1)? > 7
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn pci_id(host: &mut impl Host, device: &Path, name: &str, width: usize) -> Option<String> {
    let value = attribute(host, device, name)?;
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    Some(format!("{:0width$x}", hex(digits, width)?))
}

fn read_device(host: &mut impl Host, path: &Path) -> Option<NodePciDevice> {
    Some(NodePciDevice {
        address: pci_address(path.file_name()?.to_str()?)?,
        vendor_id: pci_id(host, path, "vendor", 4)?,
        device_id: pci_id(host, path, "device", 4)?,
        class_id: pci_id(host, path, "class", 6)?,
        driver: attribute(host, path, "uevent").and_then(|value| {
            value.lines().find_map(|line| {
                let driver = line.strip_prefix("DRIVER=")?;
                (!driver.is_empty()).then(|| driver.to_owned())
            })
        }),
        numa_node: attribute(host, path, "numa_node")
            .and_then(|node| node.parse().ok())
            .filter(|node| *node >= 0),
    })
}

pub(crate) fn inventory(host: &mut impl Host) -> Option<Vec<NodePciDevice>> {
    let root = Path::new("/sys/bus/pci");
    let directory = root.join("devices");
    let paths = host.read_dir(&directory).ok()?;
    // Host::read_dir also returns an empty list for a missing directory.
    if paths.is_empty() && !host.read_dir(root).ok()?.contains(&directory) {
        return None;
    }
    let mut devices: Vec<_> = paths
        .iter()
        .filter_map(|path| read_device(host, path))
        .collect();
    devices.sort_unstable_by(|left, right| left.address.cmp(&right.address));
    Some(devices)
}
