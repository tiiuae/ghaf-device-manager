// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ghaf_device_manager::{
    client::{Transport, listen, request, running_in_vm},
    protocol::{Action, PciSelector, UsbSelector},
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TransportKind {
    Unix,
    Tcp,
    Vsock,
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

#[derive(Clone, Debug, Args)]
struct UsbSelectorArgs {
    #[arg(long = "devnode")]
    device_node: Option<String>,
    #[arg(long)]
    bus: Option<u32>,
    #[arg(long)]
    port: Option<u32>,
    #[arg(long)]
    vid: Option<String>,
    #[arg(long)]
    pid: Option<String>,
    #[arg(long)]
    tag: Option<String>,
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

#[derive(Clone, Debug, Args)]
struct PciSelectorArgs {
    #[arg(long)]
    address: Option<String>,
    #[arg(long)]
    vid: Option<String>,
    #[arg(long)]
    did: Option<String>,
    #[arg(long)]
    tag: Option<String>,
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

fn ensure_ok(response: Value) -> Result<Value> {
    if response.get("result").and_then(Value::as_str) == Some("failed") {
        bail!(
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("device-manager request failed")
                .to_owned()
        );
    }
    Ok(response)
}

fn print_usb(response: &Value, short: bool) {
    for device in response
        .get("usb_devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        println!(
            "{}:{} {} {}",
            text(device, "vid"),
            text(device, "pid"),
            text(device, "vendor_name"),
            text(device, "product_name")
        );
        if !short {
            print_details(device);
        }
    }
}

fn print_pci(response: &Value, short: bool) {
    for device in response
        .get("pci_devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        println!(
            "{} {}:{} {} {}",
            text(device, "address"),
            text(device, "vid"),
            text(device, "did"),
            text(device, "vendor_name"),
            text(device, "device_name")
        );
        if !short {
            print_details(device);
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("None")
        .to_owned()
}

fn print_details(value: &Value) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let value = value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string());
            println!("  {key:<16}: {value}");
        }
        println!();
    }
}

impl From<UsbSelectorArgs> for UsbSelector {
    fn from(value: UsbSelectorArgs) -> Self {
        match value {
            UsbSelectorArgs {
                device_node: Some(device_node),
                bus: None,
                port: None,
                vid: None,
                pid: None,
                tag: None,
            } => Self::DeviceNode { device_node },
            UsbSelectorArgs {
                device_node: None,
                bus: Some(bus),
                port: Some(port),
                vid: None,
                pid: None,
                tag: None,
            } => Self::BusPort { bus, port },
            UsbSelectorArgs {
                device_node: None,
                bus: None,
                port: None,
                vid: Some(vid),
                pid: Some(pid),
                tag: None,
            } => Self::VidPid { vid, pid },
            UsbSelectorArgs {
                device_node: None,
                bus: None,
                port: None,
                vid: None,
                pid: None,
                tag: Some(tag),
            } => Self::Tag { tag },
            _ => {
                panic!("invalid USB selector; clap should have enforced mutually exclusive fields")
            }
        }
    }
}

impl From<PciSelectorArgs> for PciSelector {
    fn from(value: PciSelectorArgs) -> Self {
        match value {
            PciSelectorArgs {
                address: Some(address),
                vid: None,
                did: None,
                tag: None,
            } => Self::Address { address },
            PciSelectorArgs {
                address: None,
                vid: Some(vid),
                did: Some(did),
                tag: None,
            } => Self::VidDid { vid, did },
            PciSelectorArgs {
                address: None,
                vid: None,
                did: None,
                tag: Some(tag),
            } => Self::Tag { tag },
            _ => {
                panic!("invalid PCI selector; clap should have enforced mutually exclusive fields")
            }
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
        TransportKind::Tcp => Transport::Tcp {
            host: cli.host.clone(),
            port: cli.net_port,
        },
        TransportKind::Vsock => Transport::Vsock {
            cid: cli.cid,
            port: cli.net_port,
        },
    };
    if matches!(transport, Transport::Tcp { .. }) && cli.net_port > u16::MAX.into() {
        bail!("TCP port must be at most {}", u16::MAX);
    }
    Ok(transport)
}

async fn request_with_retry(
    transport: &Transport,
    message: &Action,
    deadline: Duration,
) -> Result<Value> {
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
                Some(("usb", short)),
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
                Some(("pci", short)),
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
            Some(("vmm", true)),
        ),
        Command::Listen => unreachable!(),
    };
    let response = ensure_ok(if matches!(output, Some(("vmm", _))) {
        request_with_retry(&transport, &message, deadline).await?
    } else {
        request(&transport, &message, deadline).await?
    })?;
    match output {
        Some(("usb", short)) => print_usb(&response, short),
        Some(("pci", short)) => print_pci(&response, short),
        Some(("vmm", _)) => {
            let args = response
                .get("vmm_args")
                .and_then(Value::as_array)
                .context("response has no vmm_args")?;
            if args
                .iter()
                .filter_map(Value::as_str)
                .any(|argument| argument.chars().any(char::is_whitespace))
            {
                bail!("VMM arguments containing whitespace are not supported");
            }
            print!(
                "{}",
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        _ => {}
    }
    Ok(())
}
