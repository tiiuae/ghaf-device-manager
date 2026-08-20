// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::device::{PciDevice, UsbDevice};
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
#[serde(rename_all = "snake_case", tag = "result")]
pub enum Response {
    Ok {
        #[serde(flatten)]
        payload: ResponsePayload,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    Empty(EmptyResponse),
    UsbList(UsbListResponse),
    PciList(PciListResponse),
    VmmArgs(VmmArgsResponse),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EmptyResponse;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsbListResponse {
    pub usb_devices: Vec<UsbListDevice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PciListResponse {
    pub pci_devices: Vec<PciListDevice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmmArgsResponse {
    pub vmm_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsbListDevice {
    #[serde(flatten)]
    pub device: UsbDevice,
    pub allowed_vms: Vec<String>,
    pub vm: Option<String>,
    pub disconnected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PciListDevice {
    #[serde(flatten)]
    pub device: PciDevice,
    pub allowed_vms: Vec<String>,
    pub vm: Option<String>,
    pub disconnected: bool,
}

impl ResponsePayload {
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::Empty(EmptyResponse)
    }
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

impl Response {
    #[must_use]
    pub(crate) fn ok(payload: ResponsePayload) -> Self {
        Self::Ok { payload }
    }

    pub(crate) fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
        }
    }
}

impl TryInto<ResponsePayload> for Response {
    type Error =  anyhow::Error;

    fn try_into(self) -> Result<ResponsePayload, Self::Error> {
        match self {
            Self::Ok { payload } => Ok(payload),
            Self::Failed { error } => Err(anyhow::anyhow!(error)),
        }
    }
}

impl From<Result<ResponsePayload>> for Response {
    fn from(result: Result<ResponsePayload>) -> Self {
        match result {
            Ok(payload) => Self::ok(payload),
            Err(error) => Self::failed(error.to_string()),
        }
    }
}
