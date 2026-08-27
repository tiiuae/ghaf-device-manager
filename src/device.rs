// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|v| v.trim().to_owned())
}

fn decimal(path: impl AsRef<Path>) -> Option<u32> {
    read(path)?.parse().ok()
}

fn hex(path: impl AsRef<Path>) -> Option<u32> {
    u32::from_str_radix(read(path)?.trim_start_matches("0x"), 16).ok()
}

/// Matches vhotplug's naming: hwdb-derived vendor/product names take priority
/// over a device's own (often missing or generic) USB descriptor strings.
fn hwdb_names(
    hwdb: Option<&udev::Hwdb>,
    vid: Option<&str>,
    pid: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(hwdb) = hwdb else {
        return (None, None);
    };
    let Some((vid, pid)) = vid.zip(pid) else {
        return (None, None);
    };
    let modalias = format!("usb:v{}p{}", vid.to_uppercase(), pid.to_uppercase());
    let lookup = |name: &str| {
        hwdb.query_one(modalias.as_str(), name)
            .and_then(|value| value.to_str())
            .map(str::to_owned)
    };
    (
        lookup("ID_VENDOR_FROM_DATABASE"),
        lookup("ID_MODEL_FROM_DATABASE"),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsbInterface {
    pub class: Option<u32>,
    pub subclass: Option<u32>,
    pub protocol: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsbDevice {
    pub sys_name: String,
    pub device_node: Option<String>,
    #[serde(rename = "busnum")]
    pub bus: Option<u32>,
    #[serde(rename = "devnum")]
    pub port: Option<u32>,
    #[serde(rename = "portnum")]
    pub root_port: Option<u32>,
    pub vid: Option<String>,
    pub pid: Option<String>,
    pub vendor_name: Option<String>,
    pub product_name: Option<String>,
    pub serial: Option<String>,
    #[serde(skip_serializing)]
    pub removable: Option<String>,
    pub device_class: Option<u32>,
    pub device_subclass: Option<u32>,
    pub device_protocol: Option<u32>,
    #[serde(rename = "interfaces")]
    pub interfaces_text: Option<String>,
    #[serde(skip)]
    pub interfaces: Vec<UsbInterface>,
    #[serde(skip)]
    pub driver_paths: Vec<String>,
    #[serde(skip)]
    pub modaliases: Vec<String>,
}

impl UsbDevice {
    #[must_use]
    pub(crate) fn persistent_id(&self) -> String {
        format!(
            "usb-{}:{}:{}",
            self.vid.as_deref().unwrap_or("None"),
            self.pid.as_deref().unwrap_or("None"),
            self.serial.as_deref().unwrap_or("None")
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PciDevice {
    pub address: String,
    pub driver: Option<String>,
    pub vendor_id: Option<u32>,
    pub device_id: Option<u32>,
    #[serde(rename = "vid")]
    pub vendor_id_text: Option<String>,
    #[serde(rename = "did")]
    pub device_id_text: Option<String>,
    pub vendor_name: Option<String>,
    pub device_name: Option<String>,
    #[serde(rename = "pci_class")]
    pub class: Option<u32>,
    #[serde(rename = "pci_subclass")]
    pub subclass: Option<u32>,
    #[serde(rename = "pci_prog_if")]
    pub prog_if: Option<u32>,
    pub pci_subsystem_vendor_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
}

impl PciDevice {
    #[must_use]
    pub(crate) fn persistent_id(&self) -> String {
        format!("pci-{}", self.address)
    }
}

pub(crate) fn scan_usb(root: &Path) -> Result<Vec<UsbDevice>> {
    let protect_host_storage = root == Path::new("/sys/bus/usb/devices");
    let hwdb = udev::Hwdb::new().ok();
    let mut devices = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to scan {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        let sys_name = entry.file_name().to_string_lossy().into_owned();
        if !path.join("idVendor").exists() || !path.join("busnum").exists() {
            continue;
        }
        if protect_host_storage && usb_backs_host_storage(&path)? {
            continue;
        }
        let bus = decimal(path.join("busnum"));
        let port = decimal(path.join("devnum"));
        let device_node = bus
            .zip(port)
            .map(|(bus, port)| format!("/dev/bus/usb/{bus:03}/{port:03}"));
        let root_port = sys_name
            .split_once('-')
            .and_then(|(_, ports)| ports.split('.').next())
            .and_then(|value| value.parse().ok());
        let mut interfaces = Vec::new();
        let mut driver_paths = Vec::new();
        let mut modaliases = Vec::new();
        if let Ok(children) = fs::read_dir(root) {
            let prefix = format!("{sys_name}:");
            for child in children.flatten() {
                if child.file_name().to_string_lossy().starts_with(&prefix) {
                    interfaces.push(UsbInterface {
                        class: hex(child.path().join("bInterfaceClass")),
                        subclass: hex(child.path().join("bInterfaceSubClass")),
                        protocol: hex(child.path().join("bInterfaceProtocol")),
                    });
                    if let Ok(driver) = fs::canonicalize(child.path().join("driver")) {
                        driver_paths.push(driver.to_string_lossy().into_owned());
                    }
                    if let Some(modalias) = read(child.path().join("modalias")) {
                        modaliases.push(modalias);
                    }
                }
            }
        }
        let vid = read(path.join("idVendor"));
        let pid = read(path.join("idProduct"));
        let (hwdb_vendor, hwdb_product) = hwdb_names(hwdb.as_ref(), vid.as_deref(), pid.as_deref());
        let vendor_name = hwdb_vendor.or_else(|| read(path.join("manufacturer")));
        let product_name = hwdb_product
            .or_else(|| read(path.join("product")))
            .or_else(|| {
                vid.as_ref()
                    .zip(pid.as_ref())
                    .map(|(vid, pid)| format!("USB device {vid}:{pid}"))
            });
        let interfaces_text = (!interfaces.is_empty()).then(|| {
            format!(
                ":{}:",
                interfaces
                    .iter()
                    .map(|interface| format!(
                        "{:02x}{:02x}{:02x}",
                        interface.class.unwrap_or_default(),
                        interface.subclass.unwrap_or_default(),
                        interface.protocol.unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(":")
            )
        });
        devices.push(UsbDevice {
            sys_name,
            device_node,
            bus,
            port,
            root_port,
            vid,
            pid,
            vendor_name,
            product_name,
            serial: read(path.join("serial")),
            removable: read(path.join("removable")),
            device_class: hex(path.join("bDeviceClass")),
            device_subclass: hex(path.join("bDeviceSubClass")),
            device_protocol: hex(path.join("bDeviceProtocol")),
            interfaces_text,
            interfaces,
            driver_paths,
            modaliases,
        });
    }
    devices.sort_by(|a, b| a.sys_name.cmp(&b.sys_name));
    Ok(devices)
}

#[derive(Debug)]
struct BlockDevice {
    device_id: String,
    sys_path: std::path::PathBuf,
    slaves: Vec<String>,
    has_holders: bool,
}

fn usb_backs_host_storage(device: &Path) -> Result<bool> {
    let active_device_ids = active_host_storage_device_ids()?;
    usb_backs_active_storage(device, Path::new("/sys/class/block"), &active_device_ids)
}

fn usb_backs_active_storage(
    device: &Path,
    block_root: &Path,
    active_device_ids: &HashSet<String>,
) -> Result<bool> {
    let device = fs::canonicalize(device)
        .with_context(|| format!("failed to resolve USB device {}", device.display()))?;
    let mut blocks = HashMap::new();
    for entry in fs::read_dir(block_root)
        .with_context(|| format!("failed to scan {}", block_root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(sys_path) = fs::canonicalize(&path) else {
            continue;
        };
        let Some(device_id) = read(path.join("dev")) else {
            continue;
        };
        let slaves = fs::read_dir(path.join("slaves"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let has_holders =
            fs::read_dir(path.join("holders")).is_ok_and(|mut entries| entries.next().is_some());
        blocks.insert(
            name,
            BlockDevice {
                device_id,
                sys_path,
                slaves,
                has_holders,
            },
        );
    }

    let mut backing = blocks
        .iter()
        .filter(|(_, block)| block.sys_path.starts_with(&device))
        .map(|(name, _)| name.clone())
        .collect::<HashSet<_>>();
    if backing
        .iter()
        .any(|name| blocks.get(name).is_some_and(|block| block.has_holders))
    {
        return Ok(true);
    }

    loop {
        let dependents = blocks
            .iter()
            .filter(|(name, block)| {
                !backing.contains(*name) && block.slaves.iter().any(|slave| backing.contains(slave))
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if dependents.is_empty() {
            break;
        }
        backing.extend(dependents);
    }

    Ok(backing.iter().any(|name| {
        blocks
            .get(name)
            .is_some_and(|block| active_device_ids.contains(&block.device_id))
    }))
}

fn active_host_storage_device_ids() -> Result<HashSet<String>> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .context("failed to read mounted host filesystems")?;
    let mut device_ids = mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect::<HashSet<_>>();

    let swaps = fs::read_to_string("/proc/swaps").context("failed to read active host swaps")?;
    for source in swaps
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
    {
        let Ok(metadata) = fs::metadata(source) else {
            continue;
        };
        let raw_device = if metadata.file_type().is_block_device() {
            metadata.rdev()
        } else {
            metadata.dev()
        };
        device_ids.insert(format!(
            "{}:{}",
            linux_major(raw_device),
            linux_minor(raw_device)
        ));
    }
    Ok(device_ids)
}

fn linux_major(device: u64) -> u64 {
    ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000)
}

fn linux_minor(device: u64) -> u64 {
    (device & 0xff) | ((device >> 12) & 0xfff_ff00)
}

pub(crate) fn scan_pci(root: &Path) -> Result<Vec<PciDevice>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to scan {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        let address = entry.file_name().to_string_lossy().into_owned();
        let vendor_id = hex(path.join("vendor"));
        let device_id = hex(path.join("device"));
        let class_code = hex(path.join("class"));
        devices.push(PciDevice {
            address,
            driver: fs::read_link(path.join("driver")).ok().and_then(|driver| {
                driver
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            }),
            vendor_id,
            device_id,
            vendor_id_text: vendor_id.map(|value| format!("{value:04x}")),
            device_id_text: device_id.map(|value| format!("{value:04x}")),
            vendor_name: vendor_id.map(|value| format!("PCI vendor {value:04x}")),
            device_name: device_id.map(|value| format!("PCI device {value:04x}")),
            class: class_code.map(|value| (value >> 16) & 0xff),
            subclass: class_code.map(|value| (value >> 8) & 0xff),
            prog_if: class_code.map(|value| value & 0xff),
            pci_subsystem_vendor_id: hex(path.join("subsystem_vendor"))
                .map(|value| format!("{value:04x}")),
            pci_subsystem_id: hex(path.join("subsystem_device"))
                .map(|value| format!("{value:04x}")),
        });
    }
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(devices)
}

pub(crate) fn iommu_group(address: &str, root: &Path) -> Result<Vec<String>> {
    let link = root.join(address).join("iommu_group");
    if !link.exists() {
        return Ok(Vec::new());
    }
    let group = fs::canonicalize(&link)?;
    let mut members = fs::read_dir(group.join("devices"))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    members.sort();
    Ok(members)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    fn write(path: impl AsRef<Path>, value: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn block(root: &Path, name: &str, target: &Path, device_id: &str) {
        fs::create_dir_all(target.join("slaves")).unwrap();
        fs::create_dir_all(target.join("holders")).unwrap();
        write(target.join("dev"), device_id);
        symlink(target, root.join(name)).unwrap();
    }

    fn usb_storage_fixture(dir: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let usb = dir.path().join("sys/devices/usb1/1-2");
        let block_root = dir.path().join("sys/class/block");
        fs::create_dir_all(&block_root).unwrap();
        let disk = usb.join("1-2:1.0/host0/target0:0:0/0:0:0:0/block/sda");
        block(&block_root, "sda", &disk, "8:0");
        block(&block_root, "sda2", &disk.join("sda2"), "8:2");
        (usb, block_root)
    }

    #[test]
    fn excludes_directly_mounted_usb_storage() {
        let dir = tempfile::tempdir().unwrap();
        let (usb, block_root) = usb_storage_fixture(&dir);
        assert!(
            usb_backs_active_storage(&usb, &block_root, &HashSet::from(["8:2".into()])).unwrap()
        );
    }

    #[test]
    fn excludes_usb_storage_mounted_through_device_mapper() {
        let dir = tempfile::tempdir().unwrap();
        let (usb, block_root) = usb_storage_fixture(&dir);
        let mapper = dir.path().join("sys/devices/virtual/block/dm-0");
        block(&block_root, "dm-0", &mapper, "254:0");
        symlink(block_root.join("sda2"), mapper.join("slaves/sda2")).unwrap();

        assert!(
            usb_backs_active_storage(&usb, &block_root, &HashSet::from(["254:0".into()])).unwrap()
        );
    }

    #[test]
    fn excludes_usb_storage_with_an_active_holder() {
        let dir = tempfile::tempdir().unwrap();
        let (usb, block_root) = usb_storage_fixture(&dir);
        let holder = dir.path().join("sys/devices/virtual/block/dm-0");
        block(&block_root, "dm-0", &holder, "254:0");
        symlink(&holder, block_root.join("sda2/holders/dm-0")).unwrap();

        assert!(usb_backs_active_storage(&usb, &block_root, &HashSet::new()).unwrap());
    }

    #[test]
    fn allows_idle_usb_storage() {
        let dir = tempfile::tempdir().unwrap();
        let (usb, block_root) = usb_storage_fixture(&dir);
        assert!(!usb_backs_active_storage(&usb, &block_root, &HashSet::new()).unwrap());
    }
}
