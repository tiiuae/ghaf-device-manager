// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path};

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
    pub fn persistent_id(&self) -> String {
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
    pub fn persistent_id(&self) -> String {
        format!("pci-{}", self.address)
    }
}

pub fn scan_usb(root: &Path) -> Result<Vec<UsbDevice>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to scan {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        let sys_name = entry.file_name().to_string_lossy().into_owned();
        if !path.join("idVendor").exists() || !path.join("busnum").exists() {
            continue;
        }
        if is_boot_usb(&path) {
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
        let product_name = read(path.join("product")).or_else(|| {
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
            vendor_name: read(path.join("manufacturer")),
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

fn is_boot_usb(device: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let Ok(device) = fs::canonicalize(device) else {
        return false;
    };
    mounts.lines().any(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.get(4) != Some(&"/boot") {
            return false;
        }
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            return false;
        };
        let Some(source) = fields.get(separator + 2) else {
            return false;
        };
        let Some(block) = Path::new(source).file_name() else {
            return false;
        };
        fs::canonicalize(Path::new("/sys/class/block").join(block).join("device"))
            .is_ok_and(|block_device| block_device.starts_with(&device))
    })
}

pub fn scan_pci(root: &Path) -> Result<Vec<PciDevice>> {
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

pub fn iommu_group(address: &str, root: &Path) -> Result<Vec<String>> {
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
