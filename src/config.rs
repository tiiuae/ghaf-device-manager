// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::Value;

use crate::device::{PciDevice, UsbDevice};

fn default_true() -> bool {
    true
}

fn default_state_path() -> String {
    "/var/lib/vhotplug/vhotplug.state".into()
}

fn default_crosvm() -> String {
    "crosvm".into()
}

fn default_modprobe() -> String {
    "modprobe".into()
}

fn default_modinfo() -> String {
    "modinfo".into()
}

fn default_api_port() -> u32 {
    2000
}

fn default_api_host() -> String {
    "127.0.0.1".into()
}

fn default_unix_socket() -> String {
    "/var/lib/vhotplug/vhotplug.sock".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub usb_passthrough: Vec<Value>,
    #[serde(default)]
    pub pci_passthrough: Vec<Value>,
    #[serde(default)]
    pub evdev_passthrough: Vec<Value>,
    #[serde(default)]
    pub acpi_passthrough: Vec<Value>,
    #[serde(default)]
    pub vms: Vec<Vm>,
    #[serde(default)]
    pub general: General,
    #[serde(skip)]
    driver_cache: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vm {
    pub name: String,
    #[serde(rename = "type")]
    pub vm_type: String,
    pub socket: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct General {
    #[serde(default = "default_true")]
    pub persistency: bool,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    #[serde(default = "default_crosvm")]
    pub crosvm: String,
    #[serde(default = "default_modprobe")]
    pub modprobe: String,
    #[serde(default = "default_modinfo")]
    pub modinfo: String,
    #[serde(default)]
    pub api: Api,
}

impl Default for General {
    fn default() -> Self {
        Self {
            persistency: true,
            state_path: default_state_path(),
            crosvm: default_crosvm(),
            modprobe: default_modprobe(),
            modinfo: default_modinfo(),
            api: Api::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Api {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub transports: Vec<String>,
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u32,
    #[serde(default)]
    pub allowed_cids: Vec<u32>,
    #[serde(default = "default_unix_socket")]
    pub unix_socket: String,
    pub unix_socket_user: Option<String>,
    pub unix_socket_group: Option<String>,
    pub unix_socket_mode: Option<String>,
}

impl Default for Api {
    fn default() -> Self {
        Self {
            enable: true,
            transports: Vec::new(),
            host: default_api_host(),
            port: default_api_port(),
            allowed_cids: Vec::new(),
            unix_socket: default_unix_socket(),
            unix_socket_user: None,
            unix_socket_group: None,
            unix_socket_mode: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleMatch {
    pub target_vm: Option<String>,
    pub allowed_vms: Vec<String>,
    pub skip_on_suspend: bool,
    pub iommu_add_all: bool,
    pub iommu_skip_if_shared: bool,
    pub crosvm_use_root_bus: bool,
    pub tag: Option<String>,
    pub order: usize,
}

fn enabled(value: &Value) -> bool {
    let Some(node) = value.as_object() else {
        return false;
    };
    if let Some(disable) = node.get("disable") {
        return !disable.as_bool().unwrap_or(false);
    }
    node.get("enable").and_then(Value::as_bool).unwrap_or(true)
}

fn string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn bool_value(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn case_eq(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
}

fn regex_match(pattern: Option<&str>, value: Option<&str>) -> bool {
    let (Some(pattern), Some(value)) = (pattern, value) else {
        return false;
    };
    RegexBuilder::new(&format!("^(?:{pattern})"))
        .case_insensitive(true)
        .build()
        .is_ok_and(|regex| regex.is_match(value))
}

fn hex(value: Option<&str>) -> Option<u32> {
    value.and_then(|text| u32::from_str_radix(text.trim_start_matches("0x"), 16).ok())
}

fn usb_selector_matches(selector: &Value, device: &UsbDevice) -> bool {
    if !enabled(selector) {
        return false;
    }
    if let Some(values) = selector.get("removable").and_then(Value::as_array)
        && !values.iter().filter_map(Value::as_str).any(|value| {
            device
                .removable
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(value))
        })
    {
        return false;
    }
    if case_eq(
        selector.get("vendorId").and_then(Value::as_str),
        device.vid.as_deref(),
    ) && case_eq(
        selector.get("productId").and_then(Value::as_str),
        device.pid.as_deref(),
    ) {
        return true;
    }
    if regex_match(
        selector.get("vendorName").and_then(Value::as_str),
        device.vendor_name.as_deref(),
    ) || regex_match(
        selector.get("productName").and_then(Value::as_str),
        device.product_name.as_deref(),
    ) {
        return true;
    }
    if selector.get("bus").and_then(Value::as_u64) == device.bus.map(u64::from)
        && selector.get("port").and_then(Value::as_u64) == device.root_port.map(u64::from)
        && selector.get("bus").is_some()
        && selector.get("port").is_some()
    {
        return true;
    }
    let class = selector.get("deviceClass").and_then(Value::as_u64);
    if class.is_some() && class == device.device_class.map(u64::from) {
        let subclass = selector.get("deviceSubclass").and_then(Value::as_u64);
        let protocol = selector.get("deviceProtocol").and_then(Value::as_u64);
        return subclass.is_none_or(|v| Some(v) == device.device_subclass.map(u64::from))
            && protocol.is_none_or(|v| Some(v) == device.device_protocol.map(u64::from));
    }
    device.interfaces.iter().any(|interface| {
        let class = selector.get("interfaceClass").and_then(Value::as_u64);
        let subclass = selector.get("interfaceSubclass").and_then(Value::as_u64);
        let protocol = selector.get("interfaceProtocol").and_then(Value::as_u64);
        class.is_some()
            && class == interface.class.map(u64::from)
            && subclass.is_none_or(|v| Some(v) == interface.subclass.map(u64::from))
            && protocol.is_none_or(|v| Some(v) == interface.protocol.map(u64::from))
    })
}

fn pci_selector_matches(selector: &Value, device: &PciDevice) -> bool {
    if !enabled(selector) {
        return false;
    }
    if case_eq(
        selector.get("address").and_then(Value::as_str),
        Some(&device.address),
    ) {
        return true;
    }
    if hex(selector.get("vendorId").and_then(Value::as_str)) == device.vendor_id
        && hex(selector.get("deviceId").and_then(Value::as_str)) == device.device_id
        && selector.get("vendorId").is_some()
        && selector.get("deviceId").is_some()
    {
        return true;
    }
    let class = selector.get("deviceClass").and_then(Value::as_u64);
    if class.is_some() && class == device.class.map(u64::from) {
        let subclass = selector.get("deviceSubclass").and_then(Value::as_u64);
        let prog_if = selector.get("deviceProgIf").and_then(Value::as_u64);
        return subclass.is_none_or(|v| Some(v) == device.subclass.map(u64::from))
            && prog_if.is_none_or(|v| Some(v) == device.prog_if.map(u64::from));
    }
    false
}

fn rule_matches<T, F>(rule: &Value, device: &T, selector: F) -> bool
where
    F: Fn(&Value, &T) -> bool,
{
    if !enabled(rule) {
        return false;
    }
    let allowed = rule
        .get("allow")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| selector(item, device)));
    let denied = rule
        .get("deny")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| selector(item, device)));
    allowed && !denied
}

impl Config {
    fn usb_selector_matches(&self, selector: &Value, device: &UsbDevice) -> bool {
        if usb_selector_matches(selector, device) {
            return true;
        }
        let has_valid_interface = device
            .interfaces
            .iter()
            .any(|interface| !matches!(interface.class, None | Some(0 | 0xff)));
        let Some(pattern) = (!has_valid_interface)
            .then(|| selector.get("driverPath").and_then(Value::as_str))
            .flatten()
        else {
            return false;
        };
        let Ok(regex) = RegexBuilder::new(&format!("^(?:{pattern})"))
            .case_insensitive(true)
            .build()
        else {
            return false;
        };
        device
            .driver_paths
            .iter()
            .cloned()
            .chain(
                device
                    .modaliases
                    .iter()
                    .flat_map(|modalias| self.module_paths(modalias)),
            )
            .any(|driver| regex.is_match(&driver))
    }

    fn module_paths(&self, modalias: &str) -> Vec<String> {
        if let Some(cached) = self
            .driver_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(modalias).cloned())
        {
            return cached;
        }
        let modules = Command::new(&self.general.modprobe)
            .args(["-R", modalias])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let paths = modules
            .lines()
            .filter_map(|module| {
                let output = Command::new(&self.general.modinfo)
                    .args(["-n", module])
                    .output()
                    .ok()?;
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            })
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();
        if let Ok(mut cache) = self.driver_cache.lock() {
            cache.insert(modalias.to_owned(), paths.clone());
        }
        paths
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = serde_json::from_str(&input)
            .with_context(|| format!("failed to parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.vms.iter().any(|vm| vm.vm_type != "crosvm") {
            bail!("ghaf-device-manager supports only Crosvm VMs");
        }
        let mut names = HashMap::new();
        for vm in &self.vms {
            if vm.name.is_empty() || vm.socket.is_empty() {
                bail!("VM name and socket must not be empty");
            }
            if names.insert(&vm.name, &vm.socket).is_some() {
                bail!("duplicate VM name {}", vm.name);
            }
        }
        for transport in &self.general.api.transports {
            if !matches!(transport.as_str(), "unix" | "tcp" | "vsock") {
                bail!("unsupported API transport {transport}");
            }
        }
        if self.general.api.transports.iter().any(|item| item == "tcp")
            && self.general.api.port > u16::MAX.into()
        {
            bail!("TCP API port must be at most {}", u16::MAX);
        }
        Ok(())
    }

    pub(crate) fn vm(&self, name: &str) -> Result<&Vm> {
        self.vms
            .iter()
            .find(|vm| vm.name == name)
            .with_context(|| format!("VM {name} is not found in the configuration"))
    }

    #[must_use]
    pub(crate) fn usb_rule(&self, device: &UsbDevice) -> Option<RuleMatch> {
        self.usb_passthrough.iter().find_map(|rule| {
            let target_vm = string(rule, "targetVm");
            let allowed_vms = string_list(rule, "allowedVms");
            (rule_matches(rule, device, |selector, device| {
                self.usb_selector_matches(selector, device)
            }) && (target_vm.is_some() || !allowed_vms.is_empty()))
            .then(|| RuleMatch {
                target_vm,
                allowed_vms,
                skip_on_suspend: bool_value(rule, "skipOnSuspend"),
                iommu_add_all: false,
                iommu_skip_if_shared: false,
                crosvm_use_root_bus: false,
                tag: string(rule, "tag"),
                order: 0,
            })
        })
    }

    pub(crate) fn pci_rule(&self, device: &PciDevice) -> Option<RuleMatch> {
        let mut order = 0;
        for rule in &self.pci_passthrough {
            if !enabled(rule) {
                continue;
            }
            let mut found = false;
            for allow in rule
                .get("allow")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                order += 1;
                if pci_selector_matches(allow, device) {
                    found = true;
                    break;
                }
            }
            if rule
                .get("deny")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|deny| pci_selector_matches(deny, device))
            {
                found = false;
            }
            let target_vm = string(rule, "targetVm");
            if found && target_vm.is_some() {
                return Some(RuleMatch {
                    target_vm,
                    allowed_vms: Vec::new(),
                    skip_on_suspend: bool_value(rule, "skipOnSuspend"),
                    iommu_add_all: bool_value(rule, "pciIommuAddAll"),
                    iommu_skip_if_shared: bool_value(rule, "pciIommuSkipIfShared"),
                    crosvm_use_root_bus: bool_value(rule, "crosvmUseRootBus"),
                    tag: string(rule, "tag"),
                    order,
                });
            }
        }
        None
    }

    #[must_use]
    pub(crate) fn has_pci_rules(&self, vm: &str) -> bool {
        self.pci_passthrough
            .iter()
            .any(|rule| enabled(rule) && rule.get("targetVm").and_then(Value::as_str) == Some(vm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{PciDevice, UsbInterface};

    fn usb() -> UsbDevice {
        UsbDevice {
            sys_name: "1-2.3".into(),
            device_node: Some("/dev/bus/usb/001/004".into()),
            bus: Some(1),
            port: Some(4),
            root_port: Some(2),
            vid: Some("046d".into()),
            pid: Some("c52b".into()),
            vendor_name: Some("Logitech".into()),
            product_name: Some("Receiver".into()),
            serial: None,
            removable: Some("removable".into()),
            device_class: Some(0),
            device_subclass: Some(0),
            device_protocol: Some(0),
            interfaces_text: Some(":030102:".into()),
            interfaces: vec![UsbInterface {
                class: Some(3),
                subclass: Some(1),
                protocol: Some(2),
            }],
            driver_paths: Vec::new(),
            modaliases: Vec::new(),
        }
    }

    #[test]
    fn usb_rule_matches_vid_pid_and_allowed_vms() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "usbPassthrough": [{
                "allowedVms": ["gui-vm", "admin-vm"],
                "tag": "input",
                "allow": [{"vendorId": "046D", "productId": "C52B"}]
            }],
            "vms": []
        }))
        .unwrap();
        let rule = config.usb_rule(&usb()).unwrap();
        assert_eq!(rule.allowed_vms, ["gui-vm", "admin-vm"]);
        assert_eq!(rule.tag.as_deref(), Some("input"));
    }

    #[test]
    fn deny_overrides_allow() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "usbPassthrough": [{
                "targetVm": "gui-vm",
                "allow": [{"interfaceClass": 3}],
                "deny": [{"vendorName": "Logi.*"}]
            }]
        }))
        .unwrap();
        assert!(config.usb_rule(&usb()).is_none());
    }

    #[test]
    fn driver_path_matches_devices_without_a_valid_interface() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "usbPassthrough": [{
                "targetVm": "net-vm",
                "allow": [{"driverPath": ".*/kernel/drivers/net/usb/.*"}]
            }]
        }))
        .unwrap();
        let mut device = usb();
        device.interfaces[0].class = Some(0xff);
        device.driver_paths = vec![
            "/run/current-system/kernel-modules/lib/modules/kernel/drivers/net/usb/cdc_ether.ko.xz"
                .into(),
        ];
        assert_eq!(
            config.usb_rule(&device).unwrap().target_vm.as_deref(),
            Some("net-vm")
        );
    }

    #[test]
    fn pci_order_is_the_matching_allow_position() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "pciPassthrough": [
                {
                    "targetVm": "other-vm",
                    "allow": [
                        {"address": "0000:00:00.0"},
                        {"address": "0000:00:01.0"}
                    ]
                },
                {
                    "targetVm": "audio-vm",
                    "allow": [
                        {"address": "0000:00:02.0"},
                        {"address": "0000:00:1f.3"}
                    ]
                }
            ]
        }))
        .unwrap();
        let device = PciDevice {
            address: "0000:00:1f.3".into(),
            driver: None,
            vendor_id: Some(0x8086),
            device_id: Some(0x51ca),
            vendor_id_text: Some("8086".into()),
            device_id_text: Some("51ca".into()),
            vendor_name: None,
            device_name: None,
            class: Some(4),
            subclass: Some(1),
            prog_if: Some(0),
            pci_subsystem_vendor_id: None,
            pci_subsystem_id: None,
        };
        let rule = config.pci_rule(&device).unwrap();
        assert_eq!(rule.order, 4);
        assert_eq!(rule.target_vm.as_deref(), Some("audio-vm"));
    }
}
