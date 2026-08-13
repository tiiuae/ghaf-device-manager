// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ghaf_device_manager::client::{Transport, listen, request, running_in_vm};
use serde_json::{Map, Value, json};

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
struct UsbSelector {
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
        selector: UsbSelector,
        #[arg(long)]
        vm: Option<String>,
    },
    Detach {
        #[command(flatten)]
        selector: UsbSelector,
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
struct PciSelector {
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
        selector: PciSelector,
        #[arg(long)]
        vm: Option<String>,
    },
    Detach {
        #[command(flatten)]
        selector: PciSelector,
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

fn add_optional(map: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
}

fn usb_message(action: &str, selector: &UsbSelector) -> Result<Map<String, Value>> {
    if selector.device_node.is_none()
        && selector.bus.zip(selector.port).is_none()
        && selector.vid.as_ref().zip(selector.pid.as_ref()).is_none()
        && selector.tag.is_none()
    {
        bail!("You must specify either --devnode or --bus and --port or --vid and --pid or --tag");
    }
    let mut map = Map::from_iter([("action".into(), json!(action))]);
    add_optional(&mut map, "device_node", &selector.device_node);
    add_optional(&mut map, "vid", &selector.vid);
    add_optional(&mut map, "pid", &selector.pid);
    add_optional(&mut map, "tag", &selector.tag);
    if let Some(bus) = selector.bus {
        map.insert("bus".into(), json!(bus));
    }
    if let Some(port) = selector.port {
        map.insert("port".into(), json!(port));
    }
    Ok(map)
}

fn pci_message(action: &str, selector: &PciSelector) -> Result<Map<String, Value>> {
    if selector.address.is_none()
        && selector.vid.as_ref().zip(selector.did.as_ref()).is_none()
        && selector.tag.is_none()
    {
        bail!("You must specify either --address or --vid and --did or --tag");
    }
    let mut map = Map::from_iter([("action".into(), json!(action))]);
    add_optional(&mut map, "address", &selector.address);
    add_optional(&mut map, "vid", &selector.vid);
    add_optional(&mut map, "did", &selector.did);
    add_optional(&mut map, "tag", &selector.tag);
    Ok(map)
}

fn list_message(action: &str, connected: bool, disconnected: bool, tag: &Option<String>) -> Value {
    let disconnected = if connected {
        Some(false)
    } else if disconnected {
        Some(true)
    } else {
        None
    };
    json!({"action": action, "disconnected": disconnected, "tag": tag})
}

fn vm_message(action: &str, vm: &Option<String>) -> Value {
    json!({"action": action, "vm": vm})
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
    message: Value,
    deadline: Duration,
) -> Result<Value> {
    let start = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        match request(transport, message.clone(), remaining).await {
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
    if matches!(cli.command, Command::Listen) {
        return listen(&transport, |message| println!("{message}"))
            .await
            .context("notification listener failed");
    }
    let (message, deadline, output) = match &cli.command {
        Command::Usb { action } => match action {
            UsbAction::Attach { selector, vm } => {
                let mut message = usb_message("usb_attach", selector)?;
                add_optional(&mut message, "vm", vm);
                (Value::Object(message), Duration::from_secs(10), None)
            }
            UsbAction::Detach { selector } => (
                Value::Object(usb_message("usb_detach", selector)?),
                Duration::from_secs(10),
                None,
            ),
            UsbAction::List {
                connected,
                disconnected,
                short,
                tag,
            } => (
                list_message("usb_list", *connected, *disconnected, tag),
                Duration::from_secs(10),
                Some(("usb", *short)),
            ),
            UsbAction::Suspend { vm } => {
                (vm_message("usb_suspend", vm), Duration::from_secs(10), None)
            }
            UsbAction::Resume { vm } => {
                (vm_message("usb_resume", vm), Duration::from_secs(10), None)
            }
        },
        Command::Pci { action } => match action {
            PciAction::Attach { selector, vm } => {
                let mut message = pci_message("pci_attach", selector)?;
                add_optional(&mut message, "vm", vm);
                (Value::Object(message), Duration::from_secs(10), None)
            }
            PciAction::Detach { selector } => (
                Value::Object(pci_message("pci_detach", selector)?),
                Duration::from_secs(10),
                None,
            ),
            PciAction::List {
                connected,
                disconnected,
                short,
                tag,
            } => (
                list_message("pci_list", *connected, *disconnected, tag),
                Duration::from_secs(10),
                Some(("pci", *short)),
            ),
            PciAction::Suspend { vm } => {
                (vm_message("pci_suspend", vm), Duration::from_secs(10), None)
            }
            PciAction::Resume { vm } => {
                (vm_message("pci_resume", vm), Duration::from_secs(10), None)
            }
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
            json!({
                "action": "vmm_args",
                "vm": vm,
                "qemu_bus_prefix": qemu_bus_prefix,
                "qemu_bus_start_index": qemu_bus_start_index,
                "require_pci": require_pci,
            }),
            {
                if !timeout.is_finite() || *timeout < 0.0 {
                    bail!("VMM argument timeout must be a finite non-negative number");
                }
                Duration::from_secs_f64(*timeout)
            },
            Some(("vmm", true)),
        ),
        Command::Listen => unreachable!(),
    };
    let response = ensure_ok(if matches!(output, Some(("vmm", _))) {
        request_with_retry(&transport, message, deadline).await?
    } else {
        request(&transport, message, deadline).await?
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
