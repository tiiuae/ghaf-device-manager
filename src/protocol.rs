// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::manager::Selector;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    EnableNotifications,
    UsbList {
        disconnected: Option<bool>,
        tag: Option<String>,
    },
    UsbAttach {
        #[serde(flatten)]
        selector: UsbSelector,
        vm: Option<String>,
    },
    UsbDetach {
        #[serde(flatten)]
        selector: UsbSelector,
    },
    UsbSuspend {
        vm: Option<String>,
    },
    UsbResume {
        vm: Option<String>,
    },
    PciList {
        disconnected: Option<bool>,
        tag: Option<String>,
    },
    PciAttach {
        #[serde(flatten)]
        selector: PciSelector,
        vm: Option<String>,
    },
    PciDetach {
        #[serde(flatten)]
        selector: PciSelector,
    },
    PciSuspend {
        vm: Option<String>,
    },
    PciResume {
        vm: Option<String>,
    },
    VmmArgs {
        vm: String,
        qemu_bus_prefix: Option<String>,
        qemu_bus_start_index: Option<u32>,
        require_pci: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UsbSelector {
    DeviceNode { device_node: String },
    BusPort { bus: u32, port: u32 },
    VidPid { vid: String, pid: String },
    Tag { tag: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PciSelector {
    Address { address: String },
    VidDid { vid: String, did: String },
    Tag { tag: String },
}

impl From<UsbSelector> for Selector {
    fn from(value: UsbSelector) -> Self {
        match value {
            UsbSelector::DeviceNode { device_node } => Self {
                device_node: Some(device_node),
                ..Self::default()
            },
            UsbSelector::BusPort { bus, port } => Self {
                bus: Some(bus),
                port: Some(port),
                ..Self::default()
            },
            UsbSelector::VidPid { vid, pid } => Self {
                vid: Some(vid),
                pid: Some(pid),
                ..Self::default()
            },
            UsbSelector::Tag { tag } => Self {
                tag: Some(tag),
                ..Self::default()
            },
        }
    }
}

impl From<PciSelector> for Selector {
    fn from(value: PciSelector) -> Self {
        match value {
            PciSelector::Address { address } => Self {
                address: Some(address),
                ..Self::default()
            },
            PciSelector::VidDid { vid, did } => Self {
                vid: Some(vid),
                did: Some(did),
                ..Self::default()
            },
            PciSelector::Tag { tag } => Self {
                tag: Some(tag),
                ..Self::default()
            },
        }
    }
}
