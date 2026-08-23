// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use async_trait::async_trait;
use ghaf_device_manager::{
    Action, CommandRunner, Config, DeviceManager, PciSelector, Response, Selector, UsbSelector,
    api,
    client::{Transport, request},
    crosvm::Output,
};
use serde_json::json;

#[derive(Debug)]
struct NoCommands;

#[async_trait]
impl CommandRunner for NoCommands {
    async fn run<I, A>(&self, _: &Path, args: I, _: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = A> + Send,
        A: AsRef<OsStr> + Send,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        bail!("unexpected command: {args:?}")
    }
}

#[derive(Debug)]
struct ScriptedCommands {
    outputs: Mutex<VecDeque<Output>>,
}

#[async_trait]
impl CommandRunner for ScriptedCommands {
    async fn run<I, A>(&self, _: &Path, args: I, _: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = A> + Send,
        A: AsRef<OsStr> + Send,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected command: {args:?}"))
    }
}

#[derive(Debug)]
struct RecordingCommands {
    outputs: Arc<Mutex<VecDeque<Output>>>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl CommandRunner for RecordingCommands {
    async fn run<I, A>(&self, _: &Path, args: I, _: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = A> + Send,
        A: AsRef<OsStr> + Send,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.calls.lock().unwrap().push(args.clone());
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected command: {args:?}"))
    }
}

fn write(path: impl AsRef<Path>, value: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

fn usb_fixture(root: &Path) {
    let device = root.join("1-2.3");
    write(device.join("idVendor"), "046d\n");
    write(device.join("idProduct"), "c52b\n");
    write(device.join("busnum"), "1\n");
    write(device.join("devnum"), "4\n");
    write(device.join("manufacturer"), "Logitech\n");
    write(device.join("product"), "USB Receiver\n");
    write(device.join("removable"), "removable\n");
    write(device.join("bDeviceClass"), "00\n");
    write(device.join("bDeviceSubClass"), "00\n");
    write(device.join("bDeviceProtocol"), "00\n");
    let interface = root.join("1-2.3:1.0");
    write(interface.join("bInterfaceClass"), "03\n");
    write(interface.join("bInterfaceSubClass"), "01\n");
    write(interface.join("bInterfaceProtocol"), "02\n");
}

fn pci_fixture(root: &Path) {
    let device = root.join("0000:00:1f.3");
    write(device.join("vendor"), "0x8086\n");
    write(device.join("device"), "0x51ca\n");
    write(device.join("class"), "0x040100\n");
    write(device.join("subsystem_vendor"), "0x1028\n");
    write(device.join("subsystem_device"), "0x0b00\n");
    write(device.join("driver_override"), "");
    write(root.parent().unwrap().join("drivers_probe"), "");
}

fn shared_iommu_group_fixture(root: &Path) {
    let sibling = root.join("0000:00:1f.0");
    write(sibling.join("vendor"), "0x8086\n");
    write(sibling.join("device"), "0x5182\n");
    write(sibling.join("class"), "0x060100\n");
    write(sibling.join("subsystem_vendor"), "0x17aa\n");
    write(sibling.join("subsystem_device"), "0x2315\n");
    write(sibling.join("driver_override"), "");

    let group = root.parent().unwrap().join("kernel/iommu_groups/14");
    fs::create_dir_all(group.join("devices")).unwrap();
    for address in ["0000:00:1f.0", "0000:00:1f.3"] {
        symlink(&group, root.join(address).join("iommu_group")).unwrap();
        symlink(root.join(address), group.join("devices").join(address)).unwrap();
    }
}

fn config(state: &Path, socket: &Path, api_socket: Option<&Path>) -> Config {
    serde_json::from_value(json!({
        "usbPassthrough": [{
            "allowedVms": ["gui-vm", "admin-vm"],
            "tag": "input",
            "allow": [{"vendorId": "046d", "productId": "c52b"}]
        }],
        "pciPassthrough": [{
            "targetVm": "audio-vm",
            "tag": "audio",
            "allow": [{"address": "0000:00:1f.3"}]
        }],
        "vms": [
            {"name": "gui-vm", "type": "crosvm", "socket": socket},
            {"name": "admin-vm", "type": "crosvm", "socket": socket},
            {"name": "audio-vm", "type": "crosvm", "socket": socket}
        ],
        "general": {
            "persistency": true,
            "statePath": state,
            "api": {
                "transports": api_socket.map(|_| vec!["unix"]).unwrap_or_default(),
                "unixSocket": api_socket.unwrap_or_else(|| Path::new("/tmp/unused-vhotplug.sock")),
            }
        }
    }))
    .unwrap()
}

fn manager(dir: &tempfile::TempDir, api_socket: Option<&Path>) -> DeviceManager<NoCommands> {
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    usb_fixture(&usb);
    pci_fixture(&pci);
    DeviceManager::with_roots(
        config(
            &dir.path().join("state.json"),
            &dir.path().join("vm.sock"),
            api_socket,
        ),
        NoCommands,
        usb,
        pci,
    )
    .unwrap()
}

#[tokio::test]
async fn usb_list_preserves_widget_fields() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir, None);
    let response = serde_json::to_value(
        api::handle(
            &manager,
            Action::UsbList {
                disconnected: None,
                tag: None,
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(response["result"], "ok");
    let device = &response["usb_devices"][0];
    assert_eq!(device["device_node"], "/dev/bus/usb/001/004");
    assert_eq!(device["product_name"], "USB Receiver");
    assert_eq!(device["allowed_vms"], json!(["gui-vm", "admin-vm"]));
    assert!(device["vm"].is_null());
    assert_eq!(device["portnum"], 2);
}

#[tokio::test]
async fn pci_list_preserves_cli_fields_and_tag_filter() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir, None);
    let response = serde_json::to_value(
        api::handle(
            &manager,
            Action::PciList {
                disconnected: None,
                tag: Some("audio".to_owned()),
            },
        )
        .await,
    )
    .unwrap();
    let device = &response["pci_devices"][0];
    assert_eq!(device["address"], "0000:00:1f.3");
    assert_eq!(device["vid"], "8086");
    assert_eq!(device["did"], "51ca");
    assert_eq!(device["pci_class"], 4);
    assert_eq!(device["allowed_vms"], json!(["audio-vm"]));
}

#[tokio::test]
async fn protocol_returns_legacy_failure_shape() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir, None);
    assert!(serde_json::from_value::<Action>(json!({})).is_err());
    assert!(serde_json::from_value::<Action>(json!({"action": "no_such_action"})).is_err());
    let response =
        serde_json::to_value(api::handle(&manager, Action::EnableNotifications).await).unwrap();
    assert_eq!(response, json!({"result": "ok"}));
}

#[tokio::test]
async fn reconciliation_defers_instead_of_failing_when_the_vm_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    // `manager()` never creates the control socket, so the VM is not running.
    let manager = manager(&dir, None);
    manager.reconcile().await.unwrap();
    assert!(manager.deferred());
}

#[tokio::test]
async fn reconciliation_still_fails_when_the_vm_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    let socket = dir.path().join("vm.sock");
    usb_fixture(&usb);
    pci_fixture(&pci);
    // Socket present, so a failing Crosvm call is a real error, not deferrable.
    write(&socket, "vm generation");
    let manager = DeviceManager::with_roots(
        config(&dir.path().join("state.json"), &socket, None),
        NoCommands,
        usb,
        pci,
    )
    .unwrap();
    assert!(manager.reconcile().await.is_err());
    assert!(!manager.deferred());
}

#[test]
fn vmm_args_include_crosvm_hotplug_contract() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir, None);
    assert_eq!(
        manager.vmm_args("audio-vm", true).unwrap(),
        vec![
            "--vfio-isolate-hotplug",
            "--vfio",
            "/sys/bus/pci/devices/0000:00:1f.3,iommu=viommu,removable=true",
        ]
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("sys/pci/devices/0000:00:1f.3/driver_override")
        )
        .unwrap(),
        "vfio-pci"
    );
}

#[test]
fn vmm_args_include_all_requested_iommu_group_members() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    usb_fixture(&usb);
    pci_fixture(&pci);
    shared_iommu_group_fixture(&pci);
    let mut config = config(
        &dir.path().join("state.json"),
        &dir.path().join("vm.sock"),
        None,
    );
    config.pci_passthrough[0]["pciIommuAddAll"] = json!(true);
    let manager = DeviceManager::with_roots(config, NoCommands, usb, pci).unwrap();

    assert_eq!(
        manager.vmm_args("audio-vm", true).unwrap(),
        vec![
            "--vfio-isolate-hotplug",
            "--vfio",
            "/sys/bus/pci/devices/0000:00:1f.0,iommu=viommu,removable=true",
            "--vfio",
            "/sys/bus/pci/devices/0000:00:1f.3,iommu=viommu,removable=true",
        ]
    );
}

#[tokio::test]
async fn unix_wire_protocol_is_newline_delimited_json() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("api.sock");
    let manager = Arc::new(manager(&dir, Some(&socket)));
    let server = tokio::spawn(api::serve(Arc::clone(&manager)));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let response: Response = request(
        &Transport::Unix {
            path: socket.to_string_lossy().into_owned(),
        },
        &Action::UsbList {
            disconnected: None,
            tag: None,
        },
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(response["result"], "ok");
    assert_eq!(response["usb_devices"][0]["product_name"], "USB Receiver");
    server.abort();
}

#[tokio::test]
async fn api_socket_is_not_reachable_beyond_its_owner_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("api.sock");
    let manager = Arc::new(manager(&dir, Some(&socket)));
    let server = tokio::spawn(api::serve(Arc::clone(&manager)));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "unconfigured API socket must stay owner-only");
    server.abort();
}

#[test]
fn protocol_serialization_matches_previous_wire_shape() {
    assert_eq!(
        serde_json::to_value(Action::EnableNotifications).unwrap(),
        json!({"action": "enable_notifications"})
    );
    assert_eq!(
        serde_json::to_value(Action::UsbAttach {
            selector: UsbSelector::DeviceNode {
                device_node: "/dev/bus/usb/001/004".to_owned(),
            },
            vm: Some("gui-vm".to_owned()),
        })
        .unwrap(),
        json!({
            "action": "usb_attach",
            "device_node": "/dev/bus/usb/001/004",
            "vm": "gui-vm"
        })
    );
    assert_eq!(
        serde_json::to_value(Action::PciList {
            disconnected: Some(true),
            tag: Some("audio".to_owned()),
        })
        .unwrap(),
        json!({
            "action": "pci_list",
            "disconnected": true,
            "tag": "audio"
        })
    );
    assert_eq!(
        serde_json::to_value(Action::PciAttach {
            selector: PciSelector::Address {
                address: "0000:00:1f.3".to_owned(),
            },
            vm: None,
        })
        .unwrap(),
        json!({
            "action": "pci_attach",
            "address": "0000:00:1f.3",
            "vm": null
        })
    );
    assert_eq!(
        serde_json::to_value(Action::VmmArgs {
            vm: "audio-vm".to_owned(),
            qemu_bus_prefix: None,
            qemu_bus_start_index: Some(3),
            require_pci: true,
        })
        .unwrap(),
        json!({
            "action": "vmm_args",
            "vm": "audio-vm",
            "qemu_bus_prefix": null,
            "qemu_bus_start_index": 3,
            "require_pci": true
        })
    );
}

#[test]
fn qemu_vm_is_rejected() {
    let config: Config = serde_json::from_value(json!({
        "vms": [{"name": "gui-vm", "type": "qemu", "socket": "/run/qemu.sock"}]
    }))
    .unwrap();
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "ghaf-device-manager supports only Crosvm VMs"
    );
}

#[tokio::test]
async fn multi_vm_usb_requests_a_selection_without_running_crosvm() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir, None);
    let mut notifications = manager.subscribe();
    manager.resume_usb(None).await.unwrap();
    let notification = notifications.recv().await.unwrap();
    assert_eq!(notification["event"], "usb_select_vm");
    assert_eq!(notification["allowed_vms"], json!(["gui-vm", "admin-vm"]));
    manager.resume_usb(None).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), notifications.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn selected_usb_attach_completes_and_updates_legacy_list_fields() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    usb_fixture(&usb);
    pci_fixture(&pci);
    let manager = DeviceManager::with_roots(
        config(
            &dir.path().join("state.json"),
            &dir.path().join("vm.sock"),
            None,
        ),
        ScriptedCommands {
            outputs: Mutex::new(VecDeque::from([
                Output {
                    status: 0,
                    stdout: "devices".into(),
                    stderr: String::new(),
                },
                Output {
                    status: 0,
                    stdout: "devices".into(),
                    stderr: String::new(),
                },
                Output {
                    status: 0,
                    stdout: "ok 3".into(),
                    stderr: String::new(),
                },
            ])),
        },
        usb,
        pci,
    )
    .unwrap();
    let selector = Selector {
        vid: Some("046d".into()),
        pid: Some("c52b".into()),
        ..Default::default()
    };
    tokio::time::timeout(
        Duration::from_secs(1),
        manager.attach_usb(&selector, Some("gui-vm")),
    )
    .await
    .unwrap()
    .unwrap();
    let list = manager.usb_list(Some(false), None).await.unwrap();
    assert_eq!(list[0].vm.as_deref(), Some("gui-vm"));
    assert_eq!(
        fs::read_to_string(dir.path().join("state.json")).unwrap(),
        concat!(
            "{\n",
            "  \"selected_vms\": {\n",
            "    \"usb-046d:c52b:None\": \"gui-vm\"\n",
            "  },\n",
            "  \"disconnected_devices\": [],\n",
            "  \"crosvm_usb_ports\": {\n",
            "    \"1-2.3\": {\n",
            "      \"vm\": \"gui-vm\",\n",
            "      \"port\": 3,\n",
            "      \"socket_generation\": \"\",\n",
            "      \"vid\": \"046d\",\n",
            "      \"pid\": \"c52b\",\n",
            "      \"serial\": null\n",
            "    }\n",
            "  }\n",
            "}\n"
        )
    );
}

#[tokio::test]
async fn reconciliation_reuses_live_binding_and_replaces_it_for_a_new_socket() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    let state = dir.path().join("state.json");
    let socket = dir.path().join("vm.sock");
    usb_fixture(&usb);
    pci_fixture(&pci);
    write(&socket, "first generation");

    let mut config = config(&state, &socket, None);
    config.pci_passthrough.clear();
    config.usb_passthrough[0]["targetVm"] = json!("gui-vm");
    let outputs = Arc::new(Mutex::new(VecDeque::from([
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 3".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices 3 046d c52b".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 4".into(),
            stderr: String::new(),
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let manager = DeviceManager::with_roots(
        config,
        RecordingCommands {
            outputs: Arc::clone(&outputs),
            calls: Arc::clone(&calls),
        },
        usb,
        pci,
    )
    .unwrap();
    let mut notifications = manager.subscribe();

    let (first, queued) = tokio::join!(manager.reconcile(), manager.reconcile());
    first.unwrap();
    queued.unwrap();
    let attach_calls = || {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args.windows(2).any(|pair| pair == ["usb", "attach"]))
            .count()
    };
    assert_eq!(attach_calls(), 1);
    let first_notifications =
        std::iter::from_fn(|| notifications.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        first_notifications
            .iter()
            .filter(|notification| notification["event"] == "usb_attached")
            .count(),
        1
    );
    let first_state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    let first_binding = &first_state["crosvm_usb_ports"]["1-2.3"];
    assert_eq!(first_binding["port"], 3);
    assert_ne!(first_binding["socket_generation"], "");

    fs::rename(&socket, dir.path().join("old-vm.sock")).unwrap();
    write(&socket, "second generation");
    manager.reconcile().await.unwrap();

    assert_eq!(attach_calls(), 2);
    assert_eq!(notifications.recv().await.unwrap()["event"], "usb_attached");
    assert!(notifications.try_recv().is_err());
    assert!(outputs.lock().unwrap().is_empty());
    let second_state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    let second_binding = &second_state["crosvm_usb_ports"]["1-2.3"];
    assert_eq!(second_binding["port"], 4);
    assert_ne!(
        second_binding["socket_generation"],
        first_binding["socket_generation"]
    );
}

#[tokio::test]
async fn reconciliation_detaches_replaced_usb_port_in_the_same_vm() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    let state = dir.path().join("state.json");
    let socket = dir.path().join("vm.sock");
    usb_fixture(&usb);
    pci_fixture(&pci);
    write(&socket, "one generation");

    let mut config = config(&state, &socket, None);
    config.pci_passthrough.clear();
    config.usb_passthrough[0]["targetVm"] = json!("gui-vm");
    let outputs = Arc::new(Mutex::new(VecDeque::from([
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 3".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices 3 046d c52b".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices 3 046d c52b".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 4".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 3".into(),
            stderr: String::new(),
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let manager = DeviceManager::with_roots(
        config,
        RecordingCommands {
            outputs: Arc::clone(&outputs),
            calls: Arc::clone(&calls),
        },
        usb.clone(),
        pci,
    )
    .unwrap();

    manager.reconcile().await.unwrap();
    write(usb.join("1-2.3/serial"), "000001\n");
    manager.reconcile().await.unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|args| args.windows(2).any(|pair| pair == ["usb", "attach"]))
            .count(),
        2
    );
    assert!(calls.iter().any(|args| {
        args.windows(3)
            .any(|triple| triple == ["usb", "detach", "3"])
    }));
    assert!(outputs.lock().unwrap().is_empty());
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(state["crosvm_usb_ports"]["1-2.3"]["port"], 4);
    assert_eq!(state["crosvm_usb_ports"]["1-2.3"]["serial"], "000001");
}

#[tokio::test]
async fn reconciliation_detaches_usb_port_before_reattaching_a_reenumerated_device() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    let state = dir.path().join("state.json");
    let socket = dir.path().join("vm.sock");
    usb_fixture(&usb);
    pci_fixture(&pci);
    write(&socket, "one generation");

    let mut config = config(&state, &socket, None);
    config.pci_passthrough.clear();
    config.usb_passthrough[0]["targetVm"] = json!("gui-vm");
    let outputs = Arc::new(Mutex::new(VecDeque::from([
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 3".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices 3 046d c52b".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 3".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok 4".into(),
            stderr: String::new(),
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let manager = DeviceManager::with_roots(
        config,
        RecordingCommands {
            outputs: Arc::clone(&outputs),
            calls: Arc::clone(&calls),
        },
        usb.clone(),
        pci,
    )
    .unwrap();

    manager.reconcile().await.unwrap();
    fs::remove_dir_all(usb.join("1-2.3")).unwrap();
    fs::remove_dir_all(usb.join("1-2.3:1.0")).unwrap();
    manager.reconcile().await.unwrap();
    usb_fixture(&usb);
    manager.reconcile().await.unwrap();

    let calls = calls.lock().unwrap();
    let usb_actions = calls
        .iter()
        .filter_map(|args| {
            args.windows(2)
                .find(|pair| pair[0] == "usb" && matches!(pair[1].as_str(), "attach" | "detach"))
                .map(|pair| pair[1].clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(usb_actions, ["attach", "detach", "attach"]);
    assert!(outputs.lock().unwrap().is_empty());
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(state["crosvm_usb_ports"]["1-2.3"]["port"], 4);
}

#[tokio::test]
async fn reconciliation_does_not_repeat_unchanged_pci_attachment_notification() {
    let dir = tempfile::tempdir().unwrap();
    let usb = dir.path().join("sys/usb");
    let pci = dir.path().join("sys/pci/devices");
    usb_fixture(&usb);
    pci_fixture(&pci);

    let mut config = config(
        &dir.path().join("state.json"),
        &dir.path().join("vm.sock"),
        None,
    );
    config.usb_passthrough.clear();
    let outputs = Arc::new(Mutex::new(VecDeque::from([
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices /sys/bus/pci/devices/0000:00:1f.3".into(),
            stderr: String::new(),
        },
        Output {
            status: 0,
            stdout: "devices /sys/bus/pci/devices/0000:00:1f.3".into(),
            stderr: String::new(),
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let manager = DeviceManager::with_roots(
        config,
        RecordingCommands {
            outputs: Arc::clone(&outputs),
            calls: Arc::clone(&calls),
        },
        usb,
        pci,
    )
    .unwrap();
    let mut notifications = manager.subscribe();

    manager.reconcile().await.unwrap();
    let first_notifications =
        std::iter::from_fn(|| notifications.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        first_notifications
            .iter()
            .filter(|notification| notification["event"] == "pci_attached")
            .count(),
        1
    );

    manager.reconcile().await.unwrap();

    assert!(notifications.try_recv().is_err());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args.windows(2).any(|pair| pair == ["vfio", "add"]))
            .count(),
        1
    );
    assert!(outputs.lock().unwrap().is_empty());
}
