// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use nix::unistd::{Gid, Uid, chown};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, warn};

use crate::{
    config::{Config, RuleMatch},
    crosvm::{CommandRunner, Crosvm, bind_vfio},
    device::{PciDevice, UsbDevice, iommu_group, scan_pci, scan_usb},
    protocol::{PciListDevice, UsbListDevice},
    state::{State, UsbPortBinding},
    unix_ids::{group_id, user_id},
    usb_gate,
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
    /// Routing verdict per persistent id, kept because a `driverPath` rule
    /// only matches while a driver is bound. See `usb_routing`.
    usb_routing: Mutex<HashMap<String, bool>>,
    /// Devices already reported as an unroutable hub, so the reconcile loop
    /// says it once rather than every pass.
    reported_usb_hubs: Mutex<HashSet<String>>,
    deferred: AtomicBool,
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
        let state = State::load(config.general.persistency, &config.general.state_path);
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
            usb_routing: Mutex::new(HashMap::new()),
            reported_usb_hubs: Mutex::new(HashSet::new()),
            deferred: AtomicBool::new(false),
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

    /// The bus directory holding the device root, only where the kernel
    /// exposes the attribute: a temporary root in a test never provides it.
    fn usb_bus(&self) -> Option<&Path> {
        let bus = self.usb_root.parent()?;
        bus.join(usb_gate::AUTOPROBE).exists().then_some(bus)
    }

    /// Reports `false` where there is nothing to gate: no bus, or no enabled
    /// USB rules. The reopen below deliberately skips the rules guard, so a
    /// gate left closed by a crash reopens even after the rules are removed.
    pub fn close_usb_gate(&self) -> Result<bool> {
        let Some(bus) = self.usb_bus().filter(|_| self.config.routes_usb()) else {
            return Ok(false);
        };
        usb_gate::close(bus)?;
        Ok(true)
    }

    /// Restoring the attribute covers only future hotplug, so everything the
    /// gate left driverless is probed back as well. Devices a VM still holds
    /// are left alone: an interface a guest has not claimed reads as
    /// driverless, and probing a host driver onto it would put host and guest
    /// on the same device at once.
    ///
    /// A deployment that routes no USB never closed the gate and never probes
    /// anything back, so a host that deliberately keeps autoprobe off stays
    /// that way.
    pub async fn open_usb_gate(&self) -> Result<()> {
        let Some(bus) = self.usb_bus().filter(|_| self.config.routes_usb()) else {
            return Ok(());
        };
        if !usb_gate::is_closed(bus) {
            return Ok(());
        }
        let held = self.usb_held_by_vms().await;
        usb_gate::open(bus)?;
        for name in usb_gate::device_names(&self.usb_root)? {
            if held.contains(&name) {
                debug!(device = %name, "leaving a VM-held USB device unbound");
                continue;
            }
            if usb_gate::driver(&self.usb_root, &name).is_none()
                && let Err(error) = usb_gate::probe(bus, &name)
            {
                warn!(%error, device = %name, "failed to hand a USB device back");
            }
            let interfaces = match usb_gate::interfaces(&self.usb_root, &name) {
                Ok(interfaces) => interfaces,
                Err(error) => {
                    warn!(%error, device = %name, "failed to list interfaces");
                    continue;
                }
            };
            for interface in interfaces {
                if usb_gate::driver(&self.usb_root, &interface).is_none()
                    && let Err(error) = usb_gate::probe(bus, &interface)
                {
                    warn!(%error, interface = %interface, "failed to hand an interface back");
                }
            }
        }
        Ok(())
    }

    /// The sys names of devices state records as attached to a VM. Keyed by
    /// device node, the same key `attach_one_usb` writes.
    async fn usb_held_by_vms(&self) -> HashSet<String> {
        let Ok(devices) = scan_usb(&self.usb_root) else {
            return HashSet::new();
        };
        let state = self.state.lock().await;
        devices
            .into_iter()
            .filter(|device| {
                device
                    .device_node
                    .as_deref()
                    .is_some_and(|node| state.usb_vms.contains_key(node))
            })
            .map(|device| device.sys_name)
            .collect()
    }

    /// Whether a rule routes this device, remembered across passes.
    ///
    /// A `driverPath` rule only matches while a driver is bound, and the gate
    /// takes that driver off: read fresh every pass, such a device would
    /// alternate between routed and not, and the bind/unbind cycle that
    /// follows is what wedges the firmware this gate exists to protect. Once
    /// routed, a device stays routed for as long as it is plugged in.
    async fn usb_routing(&self, devices: &[UsbDevice]) -> HashSet<String> {
        let mut cache = self.usb_routing.lock().await;
        let state = self.state.lock().await;
        let mut routed = HashSet::new();
        let mut seen = HashMap::new();
        for device in devices {
            let id = device.persistent_id();
            let matched = self.config.usb_rule(device).is_some()
                || cache.get(&id).copied().unwrap_or_default();
            seen.insert(id.clone(), matched);
            // A device handed back to the host on purpose is no longer
            // routed: keeping it unbound would leave it dead on both sides.
            if matched && !state.disconnected(&id) {
                routed.insert(device.sys_name.clone());
            }
        }
        // Unplugged devices forget their verdict, so a replug re-decides.
        *cache = seen;
        routed
    }

    /// Probing every device first is what creates the interfaces the rules
    /// are matched against, so a routed device reads as it did ungated.
    async fn apply_usb_gate(&self) -> Result<()> {
        let Some(bus) = self.usb_bus() else {
            return Ok(());
        };
        if !self.config.routes_usb() {
            return Ok(());
        }
        // Re-asserted every pass: a failed second daemon start reopens the
        // gate on its way out, and nothing else would close it again.
        usb_gate::close(bus)?;
        for name in usb_gate::device_names(&self.usb_root)? {
            if usb_gate::driver(&self.usb_root, &name).is_none()
                && let Err(error) = usb_gate::probe(bus, &name)
            {
                warn!(%error, device = %name, "failed to configure a USB device");
            }
        }
        let routed = self.usb_routing(&scan_usb(&self.usb_root)?).await;
        for name in usb_gate::device_names(&self.usb_root)? {
            let is_routed = routed.contains(&name);
            let interfaces = match usb_gate::interfaces(&self.usb_root, &name) {
                Ok(interfaces) => interfaces,
                Err(error) => {
                    warn!(%error, device = %name, "failed to list interfaces");
                    continue;
                }
            };
            for interface in interfaces {
                let driver = usb_gate::driver(&self.usb_root, &interface);
                let outcome = match (is_routed, driver.as_deref()) {
                    (true, Some(driver)) if usb_gate::releasable(driver) => {
                        usb_gate::release(&self.usb_root, &interface)
                    }
                    (false, None) => usb_gate::probe(bus, &interface),
                    _ => Ok(()),
                };
                if let Err(error) = outcome {
                    warn!(%error, interface = %interface, "failed to settle a host driver");
                }
            }
        }
        Ok(())
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
    ) -> Result<Vec<UsbListDevice>> {
        let state = self.state.lock().await;
        let mut output = Vec::new();
        for (device, rule) in self.usb_devices()? {
            let is_disconnected = state.disconnected(&device.persistent_id());
            if disconnected.is_some_and(|wanted| wanted != is_disconnected)
                || tag.is_some_and(|wanted| rule.tag.as_deref() != Some(wanted))
            {
                continue;
            }
            let device_node = device.device_node.clone();
            output.push(UsbListDevice {
                device,
                allowed_vms: allowed_vms(&rule),
                vm: device_node
                    .as_deref()
                    .and_then(|node| state.usb_vms.get(node).cloned()),
                disconnected: is_disconnected,
            });
        }
        Ok(output)
    }

    pub(crate) async fn pci_list(
        &self,
        disconnected: Option<bool>,
        tag: Option<&str>,
    ) -> Result<Vec<PciListDevice>> {
        let state = self.state.lock().await;
        let mut output = Vec::new();
        for (device, rule) in self.pci_devices()? {
            let is_disconnected = state.disconnected(&device.persistent_id());
            if disconnected.is_some_and(|wanted| wanted != is_disconnected)
                || tag.is_some_and(|wanted| rule.tag.as_deref() != Some(wanted))
            {
                continue;
            }
            let address = device.address.clone();
            output.push(PciListDevice {
                device,
                allowed_vms: allowed_vms(&rule),
                vm: state.pci_vms.get(&address).cloned(),
                disconnected: is_disconnected,
            });
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
        aggregate(&failures)
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
        let mut attachment_changed = current_vm.as_deref() != Some(vm_name.as_str());
        if current_vm.as_deref().is_some_and(|name| name != vm_name) {
            self.detach_one_usb(device, false).await?;
        }
        // A claim detaches any bound host driver mid-command, which wedges
        // some bridge firmware for good: take drivers off first, as bind_vfio
        // does for PCI. A usbfs binding is a VM's own claim and stays.
        for interface in usb_gate::interfaces(&self.usb_root, &device.sys_name)
            .with_context(|| format!("cannot claim {}", device.sys_name))?
        {
            match usb_gate::driver(&self.usb_root, &interface).as_deref() {
                // Reported rather than raised: an error here would fail every
                // reconcile, pinning the loop at its retry interval for as
                // long as the misconfigured rule stands.
                Some(usb_gate::HUB) => {
                    if self.reported_usb_hubs.lock().await.insert(id.clone()) {
                        warn!(
                            device = %device.sys_name,
                            %interface,
                            "skipping USB device: it is a hub, and a claim would collapse its subtree"
                        );
                    }
                    return Ok(());
                }
                Some(driver) if driver != usb_gate::CLAIM => {
                    usb_gate::release(&self.usb_root, &interface)
                        .with_context(|| format!("cannot claim {}", device.sys_name))?;
                }
                _ => {}
            }
        }
        let known = {
            let state = self.state.lock().await;
            state
                .persistent
                .crosvm_usb_ports
                .get(&device.sys_name)
                .cloned()
        };
        let generation = socket_generation(&vm.socket).unwrap_or_default();
        let live = self.crosvm.usb_list(&vm.socket).await?;
        let stale_port = known
            .as_ref()
            .filter(|binding| binding.vm == vm_name && binding.socket_generation == generation)
            .and_then(|binding| {
                live.iter()
                    .find(|item| {
                        item.0 == binding.port
                            && Some(&item.1) == binding.vid.as_ref()
                            && Some(&item.2) == binding.pid.as_ref()
                    })
                    .map(|item| item.0)
            });
        let port = if let Some(binding) = known.filter(|binding| {
            binding.vm == vm_name
                && binding.socket_generation == generation
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
                attachment_changed = true;
                let port = self.crosvm.usb_attach(&vm.socket, device_node).await?;
                if let Some(stale_port) = stale_port.filter(|stale| *stale != port) {
                    self.crosvm.usb_detach(&vm.socket, stale_port).await?;
                }
                port
            }
        } else {
            attachment_changed = true;
            let port = self.crosvm.usb_attach(&vm.socket, device_node).await?;
            if let Some(stale_port) = stale_port.filter(|stale| *stale != port) {
                self.crosvm.usb_detach(&vm.socket, stale_port).await?;
            }
            port
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
                socket_generation: generation,
                vid: device.vid.clone(),
                pid: device.pid.clone(),
                serial: device.serial.clone(),
            },
        );
        state.save()?;
        drop(state);
        self.pending_usb_selection.lock().await.remove(&id);
        if attachment_changed {
            self.notify(json!({"event": "usb_attached", "usb_device": device, "vm": vm_name}));
        }
        Ok(())
    }

    pub(crate) async fn detach_usb(&self, selector: &Selector, permanent: bool) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_usb(selector)?;
        let mut failures = Vec::new();
        for (device, _) in devices {
            if let Err(error) = self.detach_one_usb(&device, permanent).await {
                failures.push(format!("{}: {error}", device.sys_name));
            }
        }
        aggregate(&failures)
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
            if !vm.socket.exists() {
                warn!(
                    vm = %vm_name,
                    socket = %vm.socket.display(),
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

    pub(crate) async fn attach_pci(
        &self,
        selector: &Selector,
        requested_vm: Option<&str>,
    ) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_pci(selector)?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if let Err(error) = self.attach_one_pci(&device, &rule, requested_vm).await {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(&failures)
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
        let live = self
            .crosvm
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
        let mut attachment_changed = {
            let state = self.state.lock().await;
            attach
                .iter()
                .any(|address| state.pci_vms.get(address).map(String::as_str) != Some(vm_name))
        };
        for address in &attach {
            let path = format!("/sys/bus/pci/devices/{address}");
            if live.contains(&path) {
                continue;
            }
            attachment_changed = true;
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
        if attachment_changed {
            self.notify(json!({"event": "pci_attached", "pci_device": device, "vm": vm_name}));
        }
        Ok(())
    }

    pub(crate) async fn detach_pci(&self, selector: &Selector, permanent: bool) -> Result<()> {
        let _operation = self.operation.lock().await;
        let devices = self.selected_pci(selector)?;
        let mut failures = Vec::new();
        for (device, rule) in devices {
            if let Err(error) = self.detach_one_pci(&device, &rule, permanent).await {
                failures.push(format!("{}: {error}", device.address));
            }
        }
        aggregate(&failures)
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
            if vm.socket.exists() {
                for address in &detach {
                    self.crosvm.vfio_remove(&vm.socket, address).await?;
                }
            } else {
                warn!(
                    vm = %vm_name,
                    socket = %vm.socket.display(),
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

    pub(crate) async fn suspend_usb(&self, vm: Option<&str>) -> Result<()> {
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
        aggregate(&failures)
    }

    pub(crate) async fn suspend_pci(&self, vm: Option<&str>) -> Result<()> {
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
        aggregate(&failures)
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
                if self.vm_stopped(target.as_deref()) {
                    self.deferred.store(true, Ordering::Relaxed);
                    debug!(device = %device.sys_name, "deferring USB attach: VM not running");
                } else {
                    failures.push(format!("{}: {error}", device.sys_name));
                }
            }
        }
        aggregate(&failures)
    }

    pub(crate) async fn resume_pci(&self, vm: Option<&str>) -> Result<()> {
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
                if self.vm_stopped(rule.target_vm.as_deref()) {
                    self.deferred.store(true, Ordering::Relaxed);
                    debug!(device = %device.address, "deferring PCI attach: VM not running");
                } else {
                    failures.push(format!("{}: {error}", device.address));
                }
            }
        }
        aggregate(&failures)
    }

    /// Whether the last reconcile skipped work because a VM was not running.
    pub fn deferred(&self) -> bool {
        self.deferred.load(Ordering::Relaxed)
    }

    /// A control socket exists only while its VM runs. Every microvm@ service is
    /// ordered after this manager, so absent sockets are normal during startup.
    /// Checked only after a failed attach, so real Crosvm errors still surface.
    fn vm_stopped(&self, vm_name: Option<&str>) -> bool {
        let Some(name) = vm_name else {
            return false;
        };
        let Ok(vm) = self.config.vm(name) else {
            return false;
        };
        !Path::new(&vm.socket).exists()
    }

    pub async fn reconcile(&self) -> Result<()> {
        self.deferred.store(false, Ordering::Relaxed);
        if let Err(error) = self.apply_usb_gate().await {
            warn!(%error, "failed to settle host USB drivers");
        }
        self.observe().await?;
        let mut failures = Vec::new();
        if let Err(error) = self.resume_pci(None).await {
            failures.push(format!("PCI: {error}"));
        }
        if let Err(error) = self.resume_usb(None).await {
            failures.push(format!("USB: {error}"));
        }
        aggregate(&failures)
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
                let binding = self
                    .state
                    .lock()
                    .await
                    .persistent
                    .crosvm_usb_ports
                    .get(key)
                    .cloned();
                if let Some(binding) = binding {
                    let vm = self.config.vm(&binding.vm)?;
                    if vm.socket.exists()
                        && binding.socket_generation
                            == socket_generation(&vm.socket).unwrap_or_default()
                    {
                        let live = self.crosvm.usb_list(&vm.socket).await?;
                        if live.iter().any(|item| {
                            item.0 == binding.port
                                && Some(&item.1) == binding.vid.as_ref()
                                && Some(&item.2) == binding.pid.as_ref()
                        }) {
                            self.crosvm.usb_detach(&vm.socket, binding.port).await?;
                        }
                    }
                }
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

fn aggregate<T: std::borrow::Borrow<str>>(failures: &[T]) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

fn socket_generation(path: &Path) -> Option<String> {
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
    let uid = user
        .map(|name| user_id(name).with_context(|| format!("unknown ACPI table user {name}")))
        .transpose()?
        .map(Uid::from_raw);
    let gid = group
        .map(|name| group_id(name).with_context(|| format!("unknown ACPI table group {name}")))
        .transpose()?
        .map(Gid::from_raw);
    chown(path, uid, gid)?;
    Ok(())
}
