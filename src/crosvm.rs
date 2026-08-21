// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::{process::Command, time::timeout};

#[derive(Clone, Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run<I, A>(&self, program: &Path, args: I, deadline: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = A> + Send,
        A: AsRef<OsStr> + Send;
}

#[derive(Debug, Default)]
pub struct ProcessRunner;

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run<I, A>(&self, program: &Path, args: I, deadline: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = A> + Send,
        A: AsRef<OsStr> + Send,
    {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to execute {}", program.display()))?;
        let output = timeout(deadline, child.wait_with_output())
            .await
            .with_context(|| format!("{} command timed out", program.display()))??;
        Ok(Output {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub struct Crosvm<R: CommandRunner> {
    binary: PathBuf,
    runner: R,
    deadline: Duration,
}

impl<R: CommandRunner> Crosvm<R> {
    pub(crate) fn new(binary: impl Into<PathBuf>, runner: R) -> Self {
        Self {
            binary: binary.into(),
            runner,
            deadline: Duration::from_secs(10),
        }
    }

    async fn command<'a, I>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = &'a OsStr>,
        I::IntoIter: Clone + Send,
    {
        let args = args.into_iter();
        let command_args = std::iter::once(o("--no-syslog")).chain(args.clone());
        let output = self
            .runner
            .run(&self.binary, command_args, self.deadline)
            .await?;
        if output.status != 0 {
            bail!(
                "Crosvm command {:?} failed with code {}: {}",
                args.map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                output.status,
                if output.stderr.trim().is_empty() {
                    output.stdout.trim()
                } else {
                    output.stderr.trim()
                }
            );
        }
        Ok(output.stdout)
    }

    pub async fn usb_list(&self, socket: impl AsRef<Path>) -> Result<Vec<(u8, String, String)>> {
        let socket = socket.as_ref();
        let output = self
            .command([o("usb"), o("list"), socket.as_os_str()])
            .await?;
        let fields = output.split_whitespace().collect::<Vec<_>>();
        if fields.first() != Some(&"devices") || (fields.len() - 1) % 3 != 0 {
            bail!("malformed Crosvm USB list response: {}", output.trim());
        }
        fields[1..]
            .chunks_exact(3)
            .map(|chunk| {
                let port = chunk[0].parse::<u8>().context("invalid Crosvm USB port")?;
                Ok((port, chunk[1].to_owned(), chunk[2].to_owned()))
            })
            .collect()
    }

    pub async fn usb_attach(&self, socket: impl AsRef<Path>, device_node: &str) -> Result<u8> {
        let socket = socket.as_ref();
        let before = self.usb_list(socket).await?;
        for attempt in 0..6 {
            let output = self
                .command([
                    o("usb"),
                    o("attach"),
                    o("00:00:00:00"),
                    o(device_node),
                    socket.as_os_str(),
                ])
                .await?;
            let fields = output.split_whitespace().collect::<Vec<_>>();
            if fields.first() == Some(&"ok") {
                if let Some(port) = fields.get(1).and_then(|value| value.parse().ok()) {
                    return Ok(port);
                }
                let after = self.usb_list(socket).await?;
                let old = before.iter().map(|item| item.0).collect::<Vec<_>>();
                let added = after
                    .into_iter()
                    .filter(|item| !old.contains(&item.0))
                    .map(|item| item.0)
                    .collect::<Vec<_>>();
                return match added.as_slice() {
                    [port] => Ok(*port),
                    _ => bail!("Crosvm attached USB but did not identify one new port"),
                };
            }
            if fields.first() != Some(&"no_available_port") {
                bail!("unexpected Crosvm USB attach response: {}", output.trim());
            }
            if attempt < 5 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        bail!("Crosvm USB attach timed out waiting for a free port")
    }

    pub async fn usb_detach(&self, socket: impl AsRef<Path>, port: u8) -> Result<()> {
        let socket = socket.as_ref();
        let port = port.to_string();
        let output = self
            .command([o("usb"), o("detach"), o(port.as_str()), socket.as_os_str()])
            .await?;
        if output.split_whitespace().next() != Some("ok") {
            bail!("unexpected Crosvm USB detach response: {}", output.trim());
        }
        Ok(())
    }

    pub async fn vfio_list(&self, socket: impl AsRef<Path>) -> Result<Vec<String>> {
        let socket = socket.as_ref();
        let output = self
            .command([o("vfio"), o("list"), socket.as_os_str()])
            .await?;
        let fields = output.split_whitespace().collect::<Vec<_>>();
        if fields.first() != Some(&"devices") {
            bail!("malformed Crosvm VFIO list response: {}", output.trim());
        }
        let valid = regex::Regex::new(
            r"^/sys/bus/pci/devices/[0-9A-Fa-f]{4}:[0-9A-Fa-f]{2}:[0-9A-Fa-f]{2}\.[0-7]$",
        )?;
        if fields[1..].iter().any(|entry| !valid.is_match(entry)) {
            bail!("unexpected entry in Crosvm VFIO list response");
        }
        Ok(fields[1..]
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect())
    }

    pub async fn vfio_add(&self, socket: impl AsRef<Path>, address: &str) -> Result<()> {
        let socket = socket.as_ref();
        let path = format!("/sys/bus/pci/devices/{address}");
        if self.vfio_list(socket).await?.contains(&path) {
            return Ok(());
        }
        self.command([o("vfio"), o("add"), o(path.as_str()), socket.as_os_str()])
            .await?;
        for attempt in 0..6 {
            if self.vfio_list(socket).await?.contains(&path) {
                return Ok(());
            }
            if attempt < 5 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        bail!("Crosvm accepted VFIO add but {path} did not appear")
    }

    pub async fn vfio_remove(&self, socket: impl AsRef<Path>, address: &str) -> Result<()> {
        let socket = socket.as_ref();
        let path = format!("/sys/bus/pci/devices/{address}");
        if !self.vfio_list(socket).await?.contains(&path) {
            return Ok(());
        }
        self.command([o("vfio"), o("remove"), o(path.as_str()), socket.as_os_str()])
            .await?;
        for attempt in 0..6 {
            if !self.vfio_list(socket).await?.contains(&path) {
                return Ok(());
            }
            if attempt < 5 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        bail!("Crosvm accepted VFIO remove but {path} is still attached")
    }
}

pub(crate) fn bind_vfio(address: &str, root: &Path) -> Result<()> {
    let device = root.join(address);
    let current_driver = fs_driver(&device);
    if current_driver.as_deref() == Some("vfio-pci") {
        return Ok(());
    }
    if current_driver.is_some() {
        std::fs::write(device.join("driver/unbind"), address)
            .with_context(|| format!("failed to unbind PCI device {address}"))?;
    }
    std::fs::write(device.join("driver_override"), "vfio-pci")
        .with_context(|| format!("failed to set driver_override for {address}"))?;
    std::fs::write(
        root.parent()
            .context("invalid PCI sysfs root")?
            .join("drivers_probe"),
        address,
    )
    .with_context(|| format!("failed to probe vfio-pci for {address}"))?;
    Ok(())
}

fn fs_driver(device: &Path) -> Option<String> {
    std::fs::read_link(device.join("driver"))
        .ok()?
        .file_name()?
        .to_str()
        .map(ToOwned::to_owned)
}

fn o(text: &str) -> &OsStr {
    OsStr::new(text)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeRunner {
        outputs: Mutex<Vec<Output>>,
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run<I, A>(&self, _: &Path, _: I, _: Duration) -> Result<Output>
        where
            I: IntoIterator<Item = A> + Send,
            A: AsRef<OsStr> + Send,
        {
            Ok(self.outputs.lock().unwrap().remove(0))
        }
    }

    struct CapturingRunner {
        args: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl CommandRunner for CapturingRunner {
        async fn run<I, A>(&self, _: &Path, args: I, _: Duration) -> Result<Output>
        where
            I: IntoIterator<Item = A> + Send,
            A: AsRef<OsStr> + Send,
        {
            self.args.lock().unwrap().push(
                args.into_iter()
                    .map(|arg| arg.as_ref().to_string_lossy().into_owned())
                    .collect(),
            );
            Ok(Output {
                status: 0,
                stdout: "devices".into(),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn control_commands_disable_crosvm_syslog() {
        let args = Arc::new(Mutex::new(Vec::new()));
        Crosvm::new(
            "crosvm",
            CapturingRunner {
                args: Arc::clone(&args),
            },
        )
        .usb_list("/run/vm.sock")
        .await
        .unwrap();

        assert_eq!(
            *args.lock().unwrap(),
            vec![vec![
                "--no-syslog".to_owned(),
                "usb".to_owned(),
                "list".to_owned(),
                "/run/vm.sock".to_owned(),
            ]]
        );
    }

    #[tokio::test]
    async fn vfio_add_verifies_live_state() {
        let runner = FakeRunner {
            outputs: Mutex::new(vec![
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
            ]),
        };
        Crosvm::new("crosvm", runner)
            .vfio_add("/run/vm.sock", "0000:00:1f.3")
            .await
            .unwrap();
    }
}
