// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, broadcast};
use tracing::warn;

use crate::{
    config::{Config, RuleMatch},
    crosvm::{CommandRunner, Crosvm, bind_vfio},
    device::{PciDevice, UsbDevice, iommu_group, scan_pci, scan_usb},
    state::{State, UsbPortBinding},
};

#[derive(Clone, Debug, Default)]
pub struct Selector {
    pub device_node: Option<String>,
    pub bus: Option<u32>,
    pub port: Option<u32>,
    pub vid: Option<String>,
    pub pid: Option<String>,
    pub address: Option<String>,
    pub did: Option<String>,
    pub tag: Option<String>,
}

pub struct DeviceManager<R: CommandRunner> {
    pub config: Arc<Config>,
    crosvm: Crosvm<R>,
    state: Mutex<State>,
    operation: Mutex<()>,
    usb_root: PathBuf,
    pci_root: PathBuf,
    notifications: broadcast::Sender<Value>,
    observed_usb: Mutex<HashMap<String, UsbDevice>>,
    observed_pci: Mutex<HashMap<String, PciDevice>>,
    pending_usb_selection: Mutex<HashSet<String>>,
}

impl<R: CommandRunner> DeviceManager<R> {
    pub fn new(config: Config, runner: R) -> Result<Self> {
        Self::with_roots(
            config,
            runner,
            PathBuf::from("/sys/bus/usb/devices"),
            PathBuf::from("/sys/bus/pci/devices"),
        )
    }

    pub fn with_roots(
        config: Config,
        runner: R,
        usb_root: PathBuf,
        pci_root: PathBuf,
    ) -> Result<Self> {
        config.validate()?;
        let state = State::load(config.general.persistency, &config.general.state_path)?;
        let (notifications, _) = broadcast::channel(128);
        let binary = config.general.crosvm.clone();
        Ok(Self {
            config: Arc::new(config),
            crosvm: Crosvm::new(binary, runner),
            state: Mutex::new(state),
            operation: Mutex::new(()),
            usb_root,
            pci_root,
            notifications,
            observed_usb: Mutex::new(HashMap::new()),
            observed_pci: Mutex::new(HashMap::new()),
            pending_usb_selection: Mutex::new(HashSet::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    fn notify(&self, value: Value) {
        let _ = self.notifications.send(value);
    }

    fn usb_devices(&self) -> Result<Vec<(UsbDevice, RuleMatch)>> {
        Ok(scan_usb(&self.usb_root)?
            .into_iter()
            .filter_map(|device| self.config.usb_rule(&device).map(|rule| (device, rule)))
            .collect())
    }

    fn pci_devices(&self) -> Result<Vec<(PciDevice, RuleMatch)>> {
        let mut devices = Vec::new();
        for device in scan_pci(&self.pci_root)? {
            let Some(rule) = self.config.pci_rule(&device) else {
                continue;
            };
            if rule.iommu_skip_if_shared && iommu_group(&device.address, &self.pci_root)?.len() > 1
            {
                continue;
            }
            devices.push((device, rule));
        }
        devices.sort_by(|left, right| {
            left.1
                .order
                .cmp(&right.1.order)
                .then_with(|| left.0.address.cmp(&right.0.address))
        });
        Ok(devices)
    }

    pub async fn usb_list(
        &self,
        disconnected: Option<bool>,
        tag: Option<&str>,
    ) -> Result<Vec<Value>> {
        let state = self.state.lock().await;
        let mut output = Vec::new();
        for (device, rule) in self.usb_devices()? {
            let is_disconnected = state.disconnected(&device.persistent_id());
            if disconnected.is_some_and(|wanted| wanted != is_disconnected)
                || tag.is_some_and(|wanted| rule.tag.as_deref() != Some(wanted))
            {
                continue;
            }
            let mut object = serde_json::to_value(&device)?
                .as_object()
                .cloned()
                .unwrap_or_default();
            object.insert("allowed_vms".into(), json!(allowed_vms(&rule)));
            object.insert(
                "vm".into(),
                state
                    .usb_vms
                    .get(device.device_node.as_deref().unwrap_or_default())
                    .map_or(Value::Null, |vm| json!(vm)),
            );
            object.insert("disconnected".into(), json!(is_disconnected));
            output.push(Value::Object(object));
        }
        Ok(output)
    }

    pub async fn pci_list(
        &self,
        disconnected: Option<bool>,
        tag: Option<&str>,
    ) -> Result<Vec<Value>> {
        let state = self.state.lock().await;
        let mut output = Vec::new();
        for (device, rule) in self.pci_devices()? {
            let is_disconnected = state.disconnected(&device.persistent_id());
            if disconnected.is_some_and(|wanted| wanted != is_disconnected)
                || tag.is_some_and(|wanted| rule.tag.as_deref() != Some(wanted))
            {
                continue;
            }
            let mut object = serde_json::to_value(&device)?
                .as_object()
                .cloned()
                .unwrap_or_default();
            object.insert("allowed_vms".into(), json!(allowed_vms(&rule)));
            object.insert(
                "vm".into(),
                state
                    .pci_vms
                    .get(&device.address)
                    .map_or(Value::Null, |vm| json!(vm)),
            );
            object.insert("disconnected".into(), json!(is_disconnected));
            output.push(Value::Object(object));
        }
        Ok(output)
    }

    fn selected_usb(&self, selector: &Selector) -> Result<Vec<(UsbDevice, RuleMatch)>> {
        let result = self
            .usb_devices()?
            .into_iter()
            .filter(|(device, rule)| {
                selector
                    .device_node
                    .as_deref()
                    .is_some_and(|value| device.device_node.as_deref() == Some(value))
                    || selector.bus.zip(selector.port).is_some_and(|(bus, port)| {
                        device.bus == Some(bus) && device.root_port == Some(port)
                    })
                    || selector
                        .vid
                        .as_deref()
                        .zip(selector.pid.as_deref())
                        .is_some_and(|(vid, pid)| {
                            device
                                .vid
                                .as_deref()
                                .is_some_and(|value| value.eq_ignore_ascii_case(vid))
                                && device
                                    .pid
                                    .as_deref()
                                    .is_some_and(|value| value.eq_ignore_ascii_case(pid))
                        })
                    || selector
                        .tag
                        .as_deref()
                        .is_some_and(|tag| rule.tag.as_deref() == Some(tag))
            })
            .collect::<Vec<_>>();
        if result.is_empty() {
            bail!("no matching USB device found");
        }
        Ok(result)
    }

    fn selected_pci(&self, selector: &Selector) -> Result<Vec<(PciDevice, RuleMatch)>> {
        let result = self
            .pci_devices()?
            .into_iter()
            .filter(|(device, rule)| {
                selector
                    .address
                    .as_deref()
                    .is_some_and(|value| device.address.eq_ignore_ascii_case(value))
                    || selector
                        .vid
                        .as_deref()
                        .zip(selector.did.as_deref())
                        .is_some_and(|(vid, did)| {
                            device
                                .vendor_id_text
                                .as_deref()
                                .is_some_and(|value| value.eq_ignore_ascii_case(vid))
                                && device
                                    .device_id_text
                                    .as_deref()
                                    .is_some_and(|value| value.eq_ignore_ascii_case(did))
                        })
                    || selector
                        .tag
                        .as_deref()
                        .is_some_and(|tag| rule.tag.as_deref() == Some(tag))
            })
            .collect::<Vec<_>>();
        if result.is_empty() {
            bail!("no matching PCI device found");
        }
        Ok(result)
    }

    pub async fn attach_usb(&self, selector: &Selector, selected_vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_usb(selector)?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if let Err(error) = self.attach_one_usb(&device, &rule, selected_vm).await {
                failures.push(format!("{}: {error}", device.sys_name));
            }
        }
        aggregate(failures)
    }

    async fn attach_one_usb(
        &self,
        device: &UsbDevice,
        rule: &RuleMatch,
        requested_vm: Option<&str>,
    ) -> Result<()> {
        let id = device.persistent_id();
        let vm_name = {
            let state = self.state.lock().await;
            requested_vm
                .map(ToOwned::to_owned)
                .or_else(|| rule.target_vm.clone())
                .or_else(|| state.persistent.selected_vms.get(&id).cloned())
                .or_else(|| (rule.allowed_vms.len() == 1).then(|| rule.allowed_vms[0].clone()))
        };
        let Some(vm_name) = vm_name else {
            if self.pending_usb_selection.lock().await.insert(id) {
                self.notify(json!({
                    "event": "usb_select_vm",
                    "usb_device": device,
                    "allowed_vms": rule.allowed_vms,
                }));
            }
            return Ok(());
        };
        if !allowed_vms(rule).iter().any(|allowed| allowed == &vm_name) {
            bail!(
                "VM {vm_name} is not allowed for USB device {}",
                device.sys_name
            );
        }
        let vm = self.config.vm(&vm_name)?;
        let device_node = device
            .device_node
            .as_deref()
            .context("USB device has no device node")?;
        let current_vm = self.state.lock().await.usb_vms.get(device_node).cloned();
        if current_vm.as_deref().is_some_and(|name| name != vm_name) {
            self.detach_one_usb(device, false).await?;
        }
        let known = {
            let state = self.state.lock().await;
            state
                .persistent
                .crosvm_usb_ports
                .get(&device.sys_name)
                .cloned()
        };
        let live = self.crosvm.usb_list(&vm.socket).await?;
        let port = if let Some(binding) = known.filter(|binding| {
            binding.vm == vm_name
                && binding.socket_generation == socket_generation(&vm.socket).unwrap_or_default()
                && binding.vid == device.vid
                && binding.pid == device.pid
                && binding.serial == device.serial
        }) {
            if live.iter().any(|item| {
                item.0 == binding.port
                    && Some(&item.1) == device.vid.as_ref()
                    && Some(&item.2) == device.pid.as_ref()
            }) {
                binding.port
            } else {
                self.crosvm.usb_attach(&vm.socket, device_node).await?
            }
        } else {
            self.crosvm.usb_attach(&vm.socket, device_node).await?
        };
        let mut state = self.state.lock().await;
        state
            .usb_vms
            .insert(device_node.to_owned(), vm_name.clone());
        state.select_vm(&id, &vm_name)?;
        state.set_disconnected(&id, false)?;
        state.persistent.crosvm_usb_ports.insert(
            device.sys_name.clone(),
            UsbPortBinding {
                vm: vm_name.clone(),
                port,
                socket_generation: socket_generation(&vm.socket).unwrap_or_default(),
                vid: device.vid.clone(),
                pid: device.pid.clone(),
                serial: device.serial.clone(),
            },
        );
        state.save()?;
        drop(state);
        self.pending_usb_selection.lock().await.remove(&id);
        self.notify(json!({"event": "usb_attached", "usb_device": device, "vm": vm_name}));
        Ok(())
    }

    pub async fn detach_usb(&self, selector: &Selector, permanent: bool) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_usb(selector)?;
        let mut failures = Vec::new();
        for (device, _) in devices {
            if let Err(error) = self.detach_one_usb(&device, permanent).await {
                failures.push(format!("{}: {error}", device.sys_name));
            }
        }
        aggregate(failures)
    }

    async fn detach_one_usb(&self, device: &UsbDevice, permanent: bool) -> Result<()> {
        let node = device
            .device_node
            .as_deref()
            .context("USB device has no device node")?;
        let (vm_name, binding) = {
            let state = self.state.lock().await;
            (
                state.usb_vms.get(node).cloned().or_else(|| {
                    state
                        .persistent
                        .crosvm_usb_ports
                        .get(&device.sys_name)
                        .map(|binding| binding.vm.clone())
                }),
                state
                    .persistent
                    .crosvm_usb_ports
                    .get(&device.sys_name)
                    .cloned(),
            )
        };
        if let Some(vm_name) = vm_name {
            let vm = self.config.vm(&vm_name)?;
            if !Path::new(&vm.socket).exists() {
                warn!(
                    vm = %vm_name,
                    socket = %vm.socket,
                    "VM socket is absent; clearing stale USB attachment state"
                );
            } else if let Some(binding) = binding.filter(|binding| {
                binding.socket_generation == socket_generation(&vm.socket).unwrap_or_default()
            }) {
                let live = self.crosvm.usb_list(&vm.socket).await?;
                if let Some((_, vid, pid)) = live.iter().find(|item| item.0 == binding.port) {
                    if Some(vid) != device.vid.as_ref() || Some(pid) != device.pid.as_ref() {
                        bail!("Crosvm USB port contains a different device; refusing to detach");
                    }
                    self.crosvm.usb_detach(&vm.socket, binding.port).await?;
                }
            } else {
                let live = self.crosvm.usb_list(&vm.socket).await?;
                let matching = live
                    .iter()
                    .filter(|(_, vid, pid)| {
                        Some(vid) == device.vid.as_ref() && Some(pid) == device.pid.as_ref()
                    })
                    .collect::<Vec<_>>();
                match matching.as_slice() {
                    [item] => self.crosvm.usb_detach(&vm.socket, item.0).await?,
                    [] => {}
                    _ => bail!(
                        "multiple matching USB devices are attached; refusing an ambiguous detach"
                    ),
                }
            }
            self.notify(json!({
                "event": "usb_detached",
                "usb_device": {"device_node": node},
                "vm": vm_name,
            }));
        }
        let mut state = self.state.lock().await;
        state.usb_vms.remove(node);
        state.persistent.crosvm_usb_ports.remove(&device.sys_name);
        state.set_disconnected(&device.persistent_id(), permanent)?;
        state.save()?;
        self.pending_usb_selection
            .lock()
            .await
            .remove(&device.persistent_id());
        Ok(())
    }

    pub async fn attach_pci(&self, selector: &Selector, requested_vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_pci(selector)?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if let Err(error) = self.attach_one_pci(&device, &rule, requested_vm).await {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(failures)
    }

    async fn attach_one_pci(
        &self,
        device: &PciDevice,
        rule: &RuleMatch,
        requested_vm: Option<&str>,
    ) -> Result<()> {
        let target = rule
            .target_vm
            .as_deref()
            .context("PCI rule has no targetVm")?;
        let vm_name = requested_vm.unwrap_or(target);
        if vm_name != target {
            bail!(
                "VM {vm_name} is not allowed for PCI device {}",
                device.address
            );
        }
        let vm = self.config.vm(vm_name)?;
        self.crosvm
            .vfio_list(&vm.socket)
            .await
            .context("VFIO hotplug preflight failed")?;
        let group = iommu_group(&device.address, &self.pci_root)?;
        if rule.iommu_skip_if_shared && group.len() > 1 {
            bail!("IOMMU group for {} is shared", device.address);
        }
        let quarantine = if group.is_empty() {
            vec![device.address.clone()]
        } else {
            group
        };
        for address in &quarantine {
            bind_vfio(address, &self.pci_root)?;
        }
        let attach = if rule.iommu_add_all {
            quarantine.clone()
        } else {
            vec![device.address.clone()]
        };
        for address in &attach {
            self.crosvm.vfio_add(&vm.socket, address).await.with_context(|| {
                format!("failed to attach {address}; its IOMMU group remains quarantined under vfio-pci")
            })?;
        }
        let mut state = self.state.lock().await;
        for address in attach {
            state.pci_vms.insert(address, vm_name.to_owned());
        }
        state.set_disconnected(&device.persistent_id(), false)?;
        drop(state);
        self.notify(json!({"event": "pci_attached", "pci_device": device, "vm": vm_name}));
        Ok(())
    }

    pub async fn detach_pci(&self, selector: &Selector, permanent: bool) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_pci(selector)?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if let Err(error) = self.detach_one_pci(&device, &rule, permanent).await {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(failures)
    }

    async fn detach_one_pci(
        &self,
        device: &PciDevice,
        rule: &RuleMatch,
        permanent: bool,
    ) -> Result<()> {
        let vm_name = {
            let state = self.state.lock().await;
            state
                .pci_vms
                .get(&device.address)
                .cloned()
                .or_else(|| rule.target_vm.clone())
        };
        if let Some(vm_name) = vm_name {
            let vm = self.config.vm(&vm_name)?;
            let group = iommu_group(&device.address, &self.pci_root)?;
            let detach = if rule.iommu_add_all && !group.is_empty() {
                group
            } else {
                vec![device.address.clone()]
            };
            if Path::new(&vm.socket).exists() {
                for address in &detach {
                    self.crosvm.vfio_remove(&vm.socket, address).await?;
                }
            } else {
                warn!(
                    vm = %vm_name,
                    socket = %vm.socket,
                    "VM socket is absent; clearing stale PCI attachment state"
                );
            }
            let mut state = self.state.lock().await;
            for address in detach {
                state.pci_vms.remove(&address);
            }
            drop(state);
            self.notify(json!({"event": "pci_detached", "pci_device": device, "vm": vm_name}));
        }
        self.state
            .lock()
            .await
            .set_disconnected(&device.persistent_id(), permanent)?;
        Ok(())
    }

    pub async fn suspend_usb(&self, vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self
            .usb_devices()?
            .into_iter()
            .filter(|(_, rule)| !rule.skip_on_suspend)
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for (device, _) in devices {
            let attached = self
                .state
                .lock()
                .await
                .usb_vms
                .get(device.device_node.as_deref().unwrap_or_default())
                .cloned();
            if attached
                .as_deref()
                .is_some_and(|name| vm.is_none_or(|wanted| wanted == name))
                && let Err(error) = self.detach_one_usb(&device, false).await
            {
                failures.push(format!("{}: {error}", device.sys_name));
            }
        }
        aggregate(failures)
    }

    pub async fn suspend_pci(&self, vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self
            .pci_devices()?
            .into_iter()
            .filter(|(_, rule)| !rule.skip_on_suspend)
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for (device, rule) in devices {
            let attached = self
                .state
                .lock()
                .await
                .pci_vms
                .get(&device.address)
                .cloned();
            if attached
                .as_deref()
                .is_some_and(|name| vm.is_none_or(|wanted| wanted == name))
                && let Err(error) = self.detach_one_pci(&device, &rule, false).await
            {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(failures)
    }

    pub async fn resume_usb(&self, vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.usb_devices()?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if self
                .state
                .lock()
                .await
                .disconnected(&device.persistent_id())
            {
                continue;
            }
            let target = if let Some(target) = rule.target_vm.clone() {
                Some(target)
            } else {
                self.state
                    .lock()
                    .await
                    .persistent
                    .selected_vms
                    .get(&device.persistent_id())
                    .cloned()
            };
            let in_scope = match (vm, target.as_deref()) {
                (Some(wanted), Some(name)) => wanted == name,
                (Some(_), None) => false,
                (None, _) => true,
            };
            if in_scope
                && let Err(error) = self.attach_one_usb(&device, &rule, target.as_deref()).await
            {
                failures.push(format!("{}: {error}", device.sys_name));
            }
        }
        aggregate(failures)
    }

    pub async fn resume_pci(&self, vm: Option<&str>) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.pci_devices()?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if self
                .state
                .lock()
                .await
                .disconnected(&device.persistent_id())
            {
                continue;
            }
            if rule
                .target_vm
                .as_deref()
                .is_some_and(|name| vm.is_none_or(|wanted| wanted == name))
                && let Err(error) = self
                    .attach_one_pci(&device, &rule, rule.target_vm.as_deref())
                    .await
            {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(failures)
    }

    pub async fn reconcile(&self) -> Result<()> {
        self.observe().await?;
        let mut failures = Vec::new();
        if let Err(error) = self.resume_pci(None).await {
            failures.push(format!("PCI: {error}"));
        }
        if let Err(error) = self.resume_usb(None).await {
            failures.push(format!("USB: {error}"));
        }
        aggregate(failures)
    }

    async fn observe(&self) -> Result<()> {
        let usb = self
            .usb_devices()?
            .into_iter()
            .map(|(device, _)| (device.sys_name.clone(), device))
            .collect::<HashMap<_, _>>();
        let pci = self
            .pci_devices()?
            .into_iter()
            .map(|(device, _)| (device.address.clone(), device))
            .collect::<HashMap<_, _>>();
        let mut observed_usb = self.observed_usb.lock().await;
        for (key, device) in &usb {
            if !observed_usb.contains_key(key) {
                self.notify(json!({"event": "usb_connected", "usb_device": device}));
            }
        }
        for (key, device) in observed_usb.iter() {
            if !usb.contains_key(key) {
                self.notify(json!({
                    "event": "usb_disconnected",
                    "usb_device": {"device_node": device.device_node},
                }));
                let mut state = self.state.lock().await;
                if let Some(node) = &device.device_node {
                    state.usb_vms.remove(node);
                }
                state.persistent.crosvm_usb_ports.remove(key);
                state.save()?;
                self.pending_usb_selection
                    .lock()
                    .await
                    .remove(&device.persistent_id());
            }
        }
        *observed_usb = usb;
        drop(observed_usb);
        let mut observed_pci = self.observed_pci.lock().await;
        for (key, device) in &pci {
            if !observed_pci.contains_key(key) {
                self.notify(json!({"event": "pci_connected", "pci_device": device}));
            }
        }
        for (key, device) in observed_pci.iter() {
            if !pci.contains_key(key) {
                self.notify(json!({
                    "event": "pci_disconnected",
                    "pci_device": {"address": device.address},
                }));
                self.state.lock().await.pci_vms.remove(key);
            }
        }
        *observed_pci = pci;
        Ok(())
    }

    pub fn vmm_args(&self, vm_name: &str, require_pci: bool) -> Result<Vec<String>> {
        self.config.vm(vm_name)?;
        let mut args = Vec::new();
        let devices = self
            .pci_devices()?
            .into_iter()
            .filter(|(_, rule)| rule.target_vm.as_deref() == Some(vm_name))
            .collect::<Vec<_>>();
        if require_pci && !self.config.has_pci_rules(vm_name) {
            bail!("No PCI passthrough rules are configured for VM {vm_name}");
        }
        if require_pci && devices.is_empty() {
            bail!(
                "PCI passthrough is configured for VM {vm_name}, but no matching devices are present"
            );
        }
        if self.config.has_pci_rules(vm_name) {
            args.push("--vfio-isolate-hotplug".into());
        }
        let mut bound_pci = HashSet::new();
        let mut emitted_pci = HashSet::new();
        for (device, rule) in devices {
            let group = iommu_group(&device.address, &self.pci_root)?;
            let quarantine = if group.is_empty() {
                vec![device.address.clone()]
            } else {
                group
            };
            for address in &quarantine {
                if !bound_pci.insert(address.clone()) {
                    continue;
                }
                bind_vfio(address, &self.pci_root)?;
            }
            let passthrough = if rule.iommu_add_all {
                quarantine
            } else {
                vec![device.address.clone()]
            };
            let removable = if rule.crosvm_use_root_bus {
                ""
            } else {
                ",removable=true"
            };
            for address in passthrough {
                if !emitted_pci.insert(address.clone()) {
                    continue;
                }
                args.push("--vfio".into());
                args.push(format!(
                    "/sys/bus/pci/devices/{address},iommu=viommu{removable}"
                ));
            }
        }
        for rule in &self.config.acpi_passthrough {
            if !json_enabled(rule) || rule.get("targetVm").and_then(Value::as_str) != Some(vm_name)
            {
                continue;
            }
            for allow in rule
                .get("allow")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !json_enabled(allow) {
                    continue;
                }
                if let Some(table) = allow.get("acpiTable").and_then(Value::as_str)
                    && Path::new(table).is_file()
                {
                    chown_named(
                        table,
                        allow.get("setUser").and_then(Value::as_str),
                        allow.get("setGroup").and_then(Value::as_str),
                    )?;
                    args.push("--acpi-table".into());
                    args.push(table.into());
                }
            }
        }
        args.extend(evdev_args(&self.config.evdev_passthrough, vm_name)?);
        Ok(args)
    }
}

fn allowed_vms(rule: &RuleMatch) -> Vec<String> {
    rule.target_vm
        .clone()
        .into_iter()
        .chain(rule.allowed_vms.clone())
        .collect()
}

fn aggregate(failures: Vec<String>) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

fn socket_generation(path: &str) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    Some(format!(
        "{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.ctime_nsec()
    ))
}

fn evdev_args(rules: &[Value], vm_name: &str) -> Result<Vec<String>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_subsystem("input")?;
    let mut args = Vec::new();
    for device in enumerator.scan_devices()? {
        let Some(node) = device.devnode().and_then(Path::to_str) else {
            continue;
        };
        if !node.contains("/event")
            || device.property_value("ID_BUS").and_then(|v| v.to_str()) == Some("usb")
        {
            continue;
        }
        let matching = rules.iter().any(|rule| {
            if !json_enabled(rule) || rule.get("targetVm").and_then(Value::as_str) != Some(vm_name)
            {
                return false;
            }
            let allowed = rule
                .get("allow")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|allow| evdev_selector_matches(allow, &device));
            let denied = rule
                .get("deny")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|deny| evdev_selector_matches(deny, &device));
            allowed && !denied
        });
        if matching {
            args.push("--input".into());
            args.push(format!("evdev[path={node}]"));
        }
    }
    Ok(args)
}

fn json_enabled(value: &Value) -> bool {
    if let Some(disable) = value.get("disable") {
        return !disable.as_bool().unwrap_or(false);
    }
    value.get("enable").and_then(Value::as_bool).unwrap_or(true)
}

fn evdev_selector_matches(selector: &Value, device: &udev::Device) -> bool {
    if !json_enabled(selector) {
        return false;
    }
    let parent = device.parent();
    let name = device
        .attribute_value("name")
        .and_then(|value| value.to_str())
        .or_else(|| {
            parent
                .as_ref()
                .and_then(|parent| parent.attribute_value("name"))
                .and_then(|value| value.to_str())
        })
        .or_else(|| {
            device
                .property_value("NAME")
                .and_then(|value| value.to_str())
        });
    let path_tag = device
        .property_value("ID_PATH_TAG")
        .and_then(|value| value.to_str());
    regex_value(selector.get("name"), name)
        || regex_value(selector.get("pathTag"), path_tag)
        || selector
            .get("property")
            .and_then(Value::as_str)
            .zip(selector.get("value").and_then(Value::as_str))
            .is_some_and(|(property, expected)| {
                device
                    .property_value(property)
                    .and_then(|value| value.to_str())
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
}

fn regex_value(pattern: Option<&Value>, value: Option<&str>) -> bool {
    pattern
        .and_then(Value::as_str)
        .zip(value)
        .is_some_and(|(pattern, value)| {
            regex::RegexBuilder::new(&format!("^(?:{pattern})"))
                .case_insensitive(true)
                .build()
                .is_ok_and(|r| r.is_match(value))
        })
}

fn chown_named(path: &str, user: Option<&str>, group: Option<&str>) -> Result<()> {
    if user.is_none() && group.is_none() {
        return Ok(());
    }
    let uid = match user {
        Some(name) => {
            let name = CString::new(name)?;
            // SAFETY: getpwnam returns a process-owned record and the scalar is copied immediately.
            let record = unsafe { libc::getpwnam(name.as_ptr()) };
            if record.is_null() {
                bail!("unknown ACPI table user {}", name.to_string_lossy());
            }
            // SAFETY: the pointer was checked for null.
            unsafe { (*record).pw_uid }
        }
        None => u32::MAX,
    };
    let gid = match group {
        Some(name) => {
            let name = CString::new(name)?;
            // SAFETY: getgrnam returns a process-owned record and the scalar is copied immediately.
            let record = unsafe { libc::getgrnam(name.as_ptr()) };
            if record.is_null() {
                bail!("unknown ACPI table group {}", name.to_string_lossy());
            }
            // SAFETY: the pointer was checked for null.
            unsafe { (*record).gr_gid }
        }
        None => u32::MAX,
    };
    let path = CString::new(path)?;
    // SAFETY: path is a valid NUL-terminated string; uid/gid use -1 to preserve an owner.
    if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn request_selector(message: &Map<String, Value>) -> Selector {
    Selector {
        device_node: text(message, "device_node"),
        bus: number(message, "bus"),
        port: number(message, "port"),
        vid: text(message, "vid"),
        pid: text(message, "pid"),
        address: text(message, "address"),
        did: text(message, "did"),
        tag: text(message, "tag"),
    }
}

fn text(message: &Map<String, Value>, key: &str) -> Option<String> {
    message
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn number(message: &Map<String, Value>, key: &str) -> Option<u32> {
    message
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}
