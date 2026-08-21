// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ghaf_device_manager::{
    client::{Transport, listen, request, running_in_vm},
    protocol::{
        Action, PciListDevice, PciSelector, Response, ResponsePayload, UsbListDevice, UsbSelector,
    },
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TransportKind {
    Unix,
    Tcp,
    Vsock,
}

#[derive(Clone, Copy, Debug)]
enum OutputKind {
    Usb { short: bool },
    Pci { short: bool },
    Vmm,
}

#[derive(Debug, Parser)]
#[command(about = "Manage Ghaf hotplug devices")]
struct Cli {
    #[arg(short, long)]
    debug: bool,
    #[arg(short = 't', long, value_enum)]
    transport: Option<TransportKind>,
    #[arg(
        short = 'u',
        long = "path",
        default_value = "/var/lib/vhotplug/vhotplug.sock"
    )]
    path: String,
    #[arg(short = 's', long, default_value = "127.0.0.1")]
    host: String,
    #[arg(short = 'p', long = "net-port", default_value_t = 2000)]
    net_port: u32,
    #[arg(short = 'c', long, default_value_t = 2)]
    cid: u32,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Usb {
        #[command(subcommand)]
        action: UsbAction,
    },
    Pci {
        #[command(subcommand)]
        action: PciAction,
    },
    Listen,
    Vmm {
        #[command(subcommand)]
        action: VmmAction,
    },
}

#[derive(Clone, Debug, Args, Default)]
#[group(required = true, multiple = false)]
struct UsbSelectorArgs {
    #[command(flatten)]
    device_node: Option<UsbDeviceNodeArgs>,
    #[command(flatten)]
    bus_port: Option<UsbBusPortArgs>,
    #[command(flatten)]
    vid_pid: Option<UsbVidPidArgs>,
    #[command(flatten)]
    tag: Option<UsbTagArgs>,
}

#[derive(Clone, Debug, Args)]
struct UsbDeviceNodeArgs {
    #[arg(long = "devnode", conflicts_with_all = ["bus", "port", "vid", "pid", "tag"])]
    device_node: String,
}

#[derive(Clone, Debug, Args)]
struct UsbBusPortArgs {
    #[arg(long, conflicts_with_all = ["devnode", "vid", "pid", "tag"])]
    bus: u32,
    #[arg(long, conflicts_with_all = ["devnode", "vid", "pid", "tag"])]
    port: u32,
}

#[derive(Clone, Debug, Args)]
struct UsbVidPidArgs {
    #[arg(long, conflicts_with_all = ["devnode", "bus", "port", "tag"])]
    vid: String,
    #[arg(long, conflicts_with_all = ["devnode", "bus", "port", "tag"])]
    pid: String,
}

#[derive(Clone, Debug, Args)]
struct UsbTagArgs {
    #[arg(long, conflicts_with_all = ["devnode", "bus", "port", "vid", "pid"])]
    tag: String,
}

#[derive(Debug, Subcommand)]
enum UsbAction {
    Attach {
        #[command(flatten)]
        selector: UsbSelectorArgs,
        #[arg(long)]
        vm: Option<String>,
    },
    Detach {
        #[command(flatten)]
        selector: UsbSelectorArgs,
    },
    List {
        #[arg(long, conflicts_with = "disconnected")]
        connected: bool,
        #[arg(long)]
        disconnected: bool,
        #[arg(long)]
        short: bool,
        #[arg(long)]
        tag: Option<String>,
    },
    Suspend {
        #[arg(long)]
        vm: Option<String>,
    },
    Resume {
        #[arg(long)]
        vm: Option<String>,
    },
}

#[derive(Clone, Debug, Args, Default)]
#[group(required = true, multiple = false)]
struct PciSelectorArgs {
    #[command(flatten)]
    address: Option<PciAddressArgs>,
    #[command(flatten)]
    vid_did: Option<PciVidDidArgs>,
    #[command(flatten)]
    tag: Option<PciTagArgs>,
}

#[derive(Clone, Debug, Args)]
struct PciAddressArgs {
    #[arg(long, conflicts_with_all = ["vid", "did", "tag"])]
    address: String,
}

#[derive(Clone, Debug, Args)]
struct PciVidDidArgs {
    #[arg(long, conflicts_with_all = ["address", "tag"])]
    vid: String,
    #[arg(long, conflicts_with_all = ["address", "tag"])]
    did: String,
}

#[derive(Clone, Debug, Args)]
struct PciTagArgs {
    #[arg(long, conflicts_with_all = ["address", "vid", "did"])]
    tag: String,
}

#[derive(Debug, Subcommand)]
enum PciAction {
    Attach {
        #[command(flatten)]
        selector: PciSelectorArgs,
        #[arg(long)]
        vm: Option<String>,
    },
    Detach {
        #[command(flatten)]
        selector: PciSelectorArgs,
    },
    List {
        #[arg(long, conflicts_with = "disconnected")]
        connected: bool,
        #[arg(long)]
        disconnected: bool,
        #[arg(long)]
        short: bool,
        #[arg(long)]
        tag: Option<String>,
    },
    Suspend {
        #[arg(long)]
        vm: Option<String>,
    },
    Resume {
        #[arg(long)]
        vm: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum VmmAction {
    Args {
        #[arg(long)]
        vm: String,
        #[arg(long)]
        qemu_bus_prefix: Option<String>,
        #[arg(long)]
        qemu_bus_start_index: Option<u32>,
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        #[arg(long)]
        require_pci: bool,
    },
}

fn print_usb(devices: &[UsbListDevice], short: bool) {
    for device in devices {
        println!(
            "{}:{} {} {}",
            opt_text(device.device.vid.as_deref()),
            opt_text(device.device.pid.as_deref()),
            opt_text(device.device.vendor_name.as_deref()),
            opt_text(device.device.product_name.as_deref())
        );
        if !short {
            let details = serde_json::to_value(device).expect("USB list device should serialize");
            print_details(&details);
        }
    }
}

fn print_pci(devices: &[PciListDevice], short: bool) {
    for device in devices {
        println!(
            "{} {}:{} {} {}",
            device.device.address,
            opt_text(device.device.vendor_id_text.as_deref()),
            opt_text(device.device.device_id_text.as_deref()),
            opt_text(device.device.vendor_name.as_deref()),
            opt_text(device.device.device_name.as_deref())
        );
        if !short {
            let details = serde_json::to_value(device).expect("PCI list device should serialize");
            print_details(&details);
        }
    }
}

fn opt_text(value: Option<&str>) -> &str {
    value.unwrap_or("None")
}

fn print_details(value: &Value) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), ToOwned::to_owned);
            println!("  {key:<16}: {value}");
        }
        println!();
    }
}

impl From<UsbSelectorArgs> for UsbSelector {
    fn from(value: UsbSelectorArgs) -> Self {
        match value {
            UsbSelectorArgs {
                device_node: Some(UsbDeviceNodeArgs { device_node }),
                bus_port: None,
                vid_pid: None,
                tag: None,
            } => Self::DeviceNode { device_node },
            UsbSelectorArgs {
                device_node: None,
                bus_port: Some(UsbBusPortArgs { bus, port }),
                vid_pid: None,
                tag: None,
            } => Self::BusPort { bus, port },
            UsbSelectorArgs {
                device_node: None,
                bus_port: None,
                vid_pid: Some(UsbVidPidArgs { vid, pid }),
                tag: None,
            } => Self::VidPid { vid, pid },
            UsbSelectorArgs {
                device_node: None,
                bus_port: None,
                vid_pid: None,
                tag: Some(UsbTagArgs { tag }),
            } => Self::Tag { tag },
            _ => unreachable!("clap should enforce exactly one USB selector"),
        }
    }
}

impl From<PciSelectorArgs> for PciSelector {
    fn from(value: PciSelectorArgs) -> Self {
        match value {
            PciSelectorArgs {
                address: Some(PciAddressArgs { address }),
                vid_did: None,
                tag: None,
            } => Self::Address { address },
            PciSelectorArgs {
                address: None,
                vid_did: Some(PciVidDidArgs { vid, did }),
                tag: None,
            } => Self::VidDid { vid, did },
            PciSelectorArgs {
                address: None,
                vid_did: None,
                tag: Some(PciTagArgs { tag }),
            } => Self::Tag { tag },
            _ => unreachable!("clap should enforce exactly one PCI selector"),
        }
    }
}

fn transport(cli: &Cli) -> Result<Transport> {
    let transport = match cli.transport.unwrap_or_else(|| {
        if running_in_vm() {
            TransportKind::Vsock
        } else {
            TransportKind::Unix
        }
    }) {
        TransportKind::Unix => Transport::Unix {
            path: cli.path.clone(),
        },
        TransportKind::Tcp => {
            let port = cli
                .net_port
                .try_into()
                .context("TCP port must be at most 65535")?;
            Transport::Tcp {
                host: cli.host.clone(),
                port,
            }
        }
        TransportKind::Vsock => Transport::Vsock {
            cid: cli.cid,
            port: cli.net_port,
        },
    };
    Ok(transport)
}

async fn request_with_retry(
    transport: &Transport,
    message: &Action,
    deadline: Duration,
) -> Result<Response> {
    let start = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        match request(transport, message, remaining).await {
            Ok(response) => return Ok(response),
            Err(_) if start.elapsed() < deadline => {
                tokio::time::sleep(
                    Duration::from_secs(1).min(deadline.saturating_sub(start.elapsed())),
                )
                .await;
            }
            Err(error) => {
                return Err(error).context("timed out waiting for ghaf-device-manager");
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("ERROR {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let transport = transport(&cli)?;
    if matches!(&cli.command, Command::Listen) {
        return listen(&transport, |message| println!("{message}"))
            .await
            .context("notification listener failed");
    }
    let command = cli.command;
    let (message, deadline, output) = match command {
        Command::Usb { action } => match action {
            UsbAction::Attach { selector, vm } => (
                Action::UsbAttach {
                    selector: selector.into(),
                    vm,
                },
                Duration::from_secs(10),
                None,
            ),
            UsbAction::Detach { selector } => (
                Action::UsbDetach {
                    selector: selector.into(),
                },
                Duration::from_secs(10),
                None,
            ),
            UsbAction::List {
                connected,
                disconnected,
                short,
                tag,
            } => (
                Action::UsbList {
                    disconnected: if connected {
                        Some(false)
                    } else if disconnected {
                        Some(true)
                    } else {
                        None
                    },
                    tag,
                },
                Duration::from_secs(10),
                Some(OutputKind::Usb { short }),
            ),
            UsbAction::Suspend { vm } => (Action::UsbSuspend { vm }, Duration::from_secs(10), None),
            UsbAction::Resume { vm } => (Action::UsbResume { vm }, Duration::from_secs(10), None),
        },
        Command::Pci { action } => match action {
            PciAction::Attach { selector, vm } => (
                Action::PciAttach {
                    selector: selector.into(),
                    vm,
                },
                Duration::from_secs(10),
                None,
            ),
            PciAction::Detach { selector } => (
                Action::PciDetach {
                    selector: selector.into(),
                },
                Duration::from_secs(10),
                None,
            ),
            PciAction::List {
                connected,
                disconnected,
                short,
                tag,
            } => (
                Action::PciList {
                    disconnected: if connected {
                        Some(false)
                    } else if disconnected {
                        Some(true)
                    } else {
                        None
                    },
                    tag,
                },
                Duration::from_secs(10),
                Some(OutputKind::Pci { short }),
            ),
            PciAction::Suspend { vm } => (Action::PciSuspend { vm }, Duration::from_secs(10), None),
            PciAction::Resume { vm } => (Action::PciResume { vm }, Duration::from_secs(10), None),
        },
        Command::Vmm {
            action:
                VmmAction::Args {
                    vm,
                    qemu_bus_prefix,
                    qemu_bus_start_index,
                    timeout,
                    require_pci,
                },
        } => (
            Action::VmmArgs {
                vm,
                qemu_bus_prefix,
                qemu_bus_start_index,
                require_pci,
            },
            {
                if !timeout.is_finite() || timeout < 0.0 {
                    bail!("VMM argument timeout must be a finite non-negative number");
                }
                Duration::from_secs_f64(timeout)
            },
            Some(OutputKind::Vmm),
        ),
        Command::Listen => unreachable!(),
    };
    let response = if matches!(output, Some(OutputKind::Vmm)) {
        request_with_retry(&transport, &message, deadline).await?
    } else {
        request(&transport, &message, deadline).await?
    }
    .try_into()?;
    match output {
        Some(OutputKind::Usb { short }) => match response {
            ResponsePayload::UsbList(payload) => print_usb(&payload.usb_devices, short),
            other => bail!("unexpected response payload: {other:?}"),
        },
        Some(OutputKind::Pci { short }) => match response {
            ResponsePayload::PciList(payload) => print_pci(&payload.pci_devices, short),
            other => bail!("unexpected response payload: {other:?}"),
        },
        Some(OutputKind::Vmm) => {
            let ResponsePayload::VmmArgs(payload) = response else {
                bail!("unexpected response payload");
            };
            let args = payload.vmm_args;
            if args
                .iter()
                .any(|argument| argument.chars().any(char::is_whitespace))
            {
                bail!("VMM arguments containing whitespace are not supported");
            }
            print!("{}", args.join(" "));
        }
        _ => {}
    }
    Ok(())
}
