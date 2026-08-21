// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsbPortBinding {
    pub vm: String,
    pub port: u8,
    pub socket_generation: String,
    pub vid: Option<String>,
    pub pid: Option<String>,
    pub serial: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistentState {
    #[serde(default)]
    pub selected_vms: HashMap<String, String>,
    #[serde(default)]
    pub disconnected_devices: Vec<String>,
    #[serde(default, rename = "crosvm_usb_ports")]
    pub crosvm_usb_ports: HashMap<String, UsbPortBinding>,
}

#[derive(Debug)]
pub struct State {
    pub persistent: PersistentState,
    pub usb_vms: HashMap<String, String>,
    pub pci_vms: HashMap<String, String>,
    enabled: bool,
    path: PathBuf,
}

impl State {
    pub(crate) fn load(enabled: bool, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let persistent = if enabled && path.exists() {
            match fs::read_to_string(&path)
                .with_context(|| format!("failed to read state {}", path.display()))
                .and_then(|input| {
                    serde_json::from_str(&input)
                        .with_context(|| format!("failed to parse state {}", path.display()))
                }) {
                Ok(state) => state,
                Err(error) => {
                    warn!(%error, "discarding invalid persistent state");
                    PersistentState::default()
                }
            }
        } else {
            PersistentState::default()
        };
        Self {
            persistent,
            usb_vms: HashMap::new(),
            pci_vms: HashMap::new(),
            enabled,
            path,
        }
    }

    #[must_use]
    pub(crate) fn disconnected(&self, id: &str) -> bool {
        self.persistent
            .disconnected_devices
            .iter()
            .any(|item| item == id)
    }

    pub(crate) fn set_disconnected(&mut self, id: &str, value: bool) -> Result<()> {
        self.persistent
            .disconnected_devices
            .retain(|item| item != id);
        if value {
            self.persistent.disconnected_devices.push(id.to_owned());
            self.persistent.disconnected_devices.sort();
        }
        self.save()
    }

    pub(crate) fn select_vm(&mut self, id: &str, vm: &str) -> Result<()> {
        self.persistent
            .selected_vms
            .insert(id.to_owned(), vm.to_owned());
        self.save()
    }

    pub(crate) fn save(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let parent = self.path.parent().context("state path has no parent")?;
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("state")
        ));
        // Create the file already private rather than widening it afterwards,
        // so the device inventory is never briefly world-readable.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, &self.persistent)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_preserves_legacy_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(
            &path,
            r#"{"selected_vms":{"usb:1-2":"gui-vm"},"disconnected_devices":["pci:0000:00:1f.3"],"crosvm_usb_ports":{}}"#,
        )
        .unwrap();
        let mut state = State::load(true, &path);
        assert_eq!(state.persistent.selected_vms["usb:1-2"], "gui-vm");
        state.set_disconnected("pci:0000:00:1f.3", false).unwrap();
        let reloaded = State::load(true, &path);
        assert!(reloaded.persistent.disconnected_devices.is_empty());
    }

    #[test]
    fn writes_the_state_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::load(true, &path);
        state.select_vm("usb-046d:c52b:None", "gui-vm").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
