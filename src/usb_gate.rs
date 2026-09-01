// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Sysfs writes deciding which USB devices host drivers may bind to. A
//! routed device must never be bound: the claim would detach the driver
//! mid-command, which wedges some bridge firmware until power cycle. The
//! mechanism is described in README.md, Host Driver Binding.

use std::{fs, path::Path};

use anyhow::{Context, Result};

pub(crate) const AUTOPROBE: &str = "drivers_autoprobe";

pub(crate) fn close(bus: &Path) -> Result<()> {
    autoprobe(bus, "0")
}

pub(crate) fn open(bus: &Path) -> Result<()> {
    autoprobe(bus, "1")
}

/// Whether the gate is currently shut. A crashed daemon leaves it that
/// way, which is the only reason a start reopens something it never closed.
pub(crate) fn is_closed(bus: &Path) -> bool {
    fs::read_to_string(bus.join(AUTOPROBE)).is_ok_and(|value| value.trim() == "0")
}

fn autoprobe(bus: &Path, value: &str) -> Result<()> {
    let path = bus.join(AUTOPROBE);
    fs::write(&path, value).with_context(|| format!("failed to write {}", path.display()))
}

pub(crate) fn probe(bus: &Path, name: &str) -> Result<()> {
    let path = bus.join("drivers_probe");
    fs::write(&path, name).with_context(|| format!("failed to probe a host driver for {name}"))
}

/// Safe where a claim is not: an unbind runs the driver's own disconnect,
/// which drains its outstanding commands, while a claim kills them mid-flight.
pub(crate) fn release(devices: &Path, name: &str) -> Result<()> {
    let path = devices.join(name).join("driver/unbind");
    fs::write(&path, name).with_context(|| format!("failed to unbind the host driver from {name}"))
}

/// usbfs as a bound driver is a VM holding the device, not a host driver.
pub(crate) const CLAIM: &str = "usbfs";

pub(crate) fn driver(devices: &Path, name: &str) -> Option<String> {
    fs::read_link(devices.join(name).join("driver"))
        .ok()?
        .file_name()?
        .to_str()
        .map(ToOwned::to_owned)
}

/// Devices only: an interface carries a colon, as in `2-1:1.0`.
pub(crate) fn device_names(devices: &Path) -> Result<Vec<String>> {
    let mut names = fs::read_dir(devices)
        .with_context(|| format!("failed to scan {}", devices.display()))?
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|name| !name.contains(':'))
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

/// A root hub is the naming exception: device `usbN`, interface `N-0:1.0`.
/// The trailing colon stops `2-1` from collecting the interfaces of `2-10`.
pub(crate) fn interfaces(devices: &Path, sys_name: &str) -> Result<Vec<String>> {
    let prefix = sys_name
        .strip_prefix("usb")
        .filter(|bus| !bus.is_empty() && bus.chars().all(|c| c.is_ascii_digit()))
        .map_or_else(|| format!("{sys_name}:"), |bus| format!("{bus}-0:"));
    let entries =
        fs::read_dir(devices).with_context(|| format!("failed to scan {}", devices.display()))?;
    let mut names = entries
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|name| name.starts_with(&prefix))
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

/// Unbinding hub collapses every device behind it: a rule matching a hub is
/// a misconfiguration to survive rather than obey.
pub(crate) const HUB: &str = "hub";

pub(crate) fn releasable(driver: &str) -> bool {
    driver != CLAIM && driver != HUB
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> tempfile::TempDir {
        let bus = tempfile::tempdir().unwrap();
        fs::create_dir(bus.path().join("devices")).unwrap();
        bus
    }

    fn device(devices: &Path, name: &str) {
        fs::create_dir(devices.join(name)).unwrap();
    }

    fn bind(devices: &Path, name: &str, driver: &str) {
        let target = devices.parent().unwrap().join("drivers").join(driver);
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(target, devices.join(name).join("driver")).unwrap();
    }

    #[test]
    fn closing_and_opening_write_the_attribute() {
        let bus = bus();
        let path = bus.path().join(AUTOPROBE);
        fs::write(&path, "1").unwrap();
        close(bus.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "0");
        open(bus.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "1");
    }

    #[test]
    fn is_closed_reads_the_attribute() {
        let bus = bus();
        assert!(!is_closed(bus.path()));
        close(bus.path()).unwrap();
        assert!(is_closed(bus.path()));
        open(bus.path()).unwrap();
        assert!(!is_closed(bus.path()));
    }

    #[test]
    fn probe_names_the_device_for_the_kernel() {
        let bus = bus();
        probe(bus.path(), "2-1:1.0").unwrap();
        assert_eq!(
            fs::read_to_string(bus.path().join("drivers_probe")).unwrap(),
            "2-1:1.0"
        );
    }

    #[test]
    fn interfaces_do_not_leak_between_similar_device_names() {
        let bus = bus();
        let devices = bus.path().join("devices");
        for name in ["2-1", "2-1:1.0", "2-1:1.1", "2-10", "2-10:1.0"] {
            device(&devices, name);
        }
        assert_eq!(interfaces(&devices, "2-1").unwrap(), ["2-1:1.0", "2-1:1.1"]);
        assert_eq!(interfaces(&devices, "2-10").unwrap(), ["2-10:1.0"]);
    }

    #[test]
    fn a_root_hub_finds_its_interface_despite_the_naming() {
        let bus = bus();
        let devices = bus.path().join("devices");
        for name in ["usb2", "2-0:1.0", "2-1", "2-1:1.0"] {
            device(&devices, name);
        }
        assert_eq!(interfaces(&devices, "usb2").unwrap(), ["2-0:1.0"]);
        assert_eq!(interfaces(&devices, "2-1").unwrap(), ["2-1:1.0"]);
    }

    #[test]
    fn a_claim_and_a_hub_are_not_releasable() {
        assert!(!releasable("usbfs"));
        assert!(!releasable("hub"));
        assert!(releasable("uas"));
    }

    #[test]
    fn device_names_leave_out_interfaces() {
        let bus = bus();
        let devices = bus.path().join("devices");
        for name in ["usb2", "2-1", "2-1:1.0"] {
            device(&devices, name);
        }
        assert_eq!(device_names(&devices).unwrap(), ["2-1", "usb2"]);
    }

    #[test]
    fn driver_reads_the_bound_name() {
        let bus = bus();
        let devices = bus.path().join("devices");
        device(&devices, "2-1:1.0");
        assert!(driver(&devices, "2-1:1.0").is_none());
        bind(&devices, "2-1:1.0", "uas");
        assert_eq!(driver(&devices, "2-1:1.0").as_deref(), Some("uas"));
    }

    #[test]
    fn release_names_the_interface_for_its_driver() {
        let bus = bus();
        let devices = bus.path().join("devices");
        device(&devices, "2-1:1.0");
        bind(&devices, "2-1:1.0", "uas");
        release(&devices, "2-1:1.0").unwrap();
        assert_eq!(
            fs::read_to_string(bus.path().join("drivers/uas/unbind")).unwrap(),
            "2-1:1.0"
        );
    }
}
