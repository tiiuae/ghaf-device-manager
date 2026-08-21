// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, bail};
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

/// Maximum number of rendered characters kept for a single device-supplied
/// field value. Longer values are cut and marked with [`TRUNCATION_MARKER`].
const MAX_FIELD_CHARS: usize = 256;

/// Marker appended when [`escape_field`] shortens a value, so a shortened
/// rendering is always visibly distinct from a short legitimate name.
const TRUNCATION_MARKER: &str = "...";

/// Characters that must never reach the operator's terminal verbatim: C0
/// controls, `DEL` and the C1 controls (which include the single character CSI
/// `U+009B`), the bidirectional overrides and isolates, and the backslash
/// itself so that the escaping stays injective.
fn must_escape(character: char) -> bool {
    character.is_control()
        || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\\')
}

fn push_escaped(character: char, output: &mut String) {
    if must_escape(character) {
        output.extend(character.escape_debug());
    } else {
        output.push(character);
    }
}

/// Renders untrusted text so that it cannot drive the terminal it is printed
/// to. Offending characters are escaped rather than dropped, so two different
/// values never render identically; every printable character, including
/// non-ASCII text such as accented or CJK names, is passed through unchanged.
fn escape_control(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        push_escaped(character, &mut escaped);
    }
    escaped
}

/// [`escape_control`] for a single device-supplied field, additionally capped
/// at [`MAX_FIELD_CHARS`] rendered characters so that one device cannot push a
/// listing off the screen with a very long name.
fn escape_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut rendered = 0;
    for character in value.chars() {
        let start = escaped.len();
        push_escaped(character, &mut escaped);
        rendered += escaped[start..].chars().count();
        if rendered > MAX_FIELD_CHARS {
            escaped.truncate(start);
            escaped.push_str(TRUNCATION_MARKER);
            break;
        }
    }
    escaped
}

/// Renders the fatal error line. The chain is escaped but never shortened:
/// `{error:#}` reports the root cause last, so truncating it would drop the
/// part of a failure an operator needs to act on.
fn error_line(error: &Error) -> String {
    format!("ERROR {}", escape_control(&format!("{error:#}")))
}

fn print_usb(devices: &[UsbListDevice], short: bool) {
    for device in devices {
        println!(
            "{}:{} {} {}",
            escape_field(opt_text(device.device.vid.as_deref())),
            escape_field(opt_text(device.device.pid.as_deref())),
            escape_field(opt_text(device.device.vendor_name.as_deref())),
            escape_field(opt_text(device.device.product_name.as_deref()))
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
            escape_field(&device.device.address),
            escape_field(opt_text(device.device.vendor_id_text.as_deref())),
            escape_field(opt_text(device.device.device_id_text.as_deref())),
            escape_field(opt_text(device.device.vendor_name.as_deref())),
            escape_field(opt_text(device.device.device_name.as_deref()))
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
            // Strings are device supplied and get the field cap; anything else
            // is a JSON rendering the daemon produced (numbers, booleans, the
            // configured VM list) and is only escaped, never shortened.
            let value = value
                .as_str()
                .map_or_else(|| escape_control(&value.to_string()), escape_field);
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
        eprintln!("{}", error_line(&error));
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    const BIDI: [char; 9] = [
        '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
        '\u{2068}', '\u{2069}',
    ];

    fn is_dangerous(character: char) -> bool {
        character.is_control() || BIDI.contains(&character)
    }

    #[test]
    fn escapes_ansi_escape_sequences() {
        let rendered = escape_field("\u{1b}[2K\u{1b}[1;1HTrusted Keyboard");
        assert_eq!(rendered, "\\u{1b}[2K\\u{1b}[1;1HTrusted Keyboard");
        assert!(!rendered.chars().any(is_dangerous));
    }

    #[test]
    fn escapes_a_bare_carriage_return() {
        assert_eq!(escape_field("hostile\rspoofed"), "hostile\\rspoofed");
    }

    #[test]
    fn escapes_delete_and_c1_controls() {
        assert_eq!(escape_field("a\u{7f}b"), "a\\u{7f}b");
        assert_eq!(escape_field("a\u{9b}2Kb"), "a\\u{9b}2Kb");
        assert_eq!(escape_field("a\u{85}b"), "a\\u{85}b");
        assert_eq!(escape_field("a\u{0}b"), "a\\0b");
    }

    #[test]
    fn escapes_bidi_overrides_and_isolates() {
        for character in BIDI {
            let rendered = escape_field(&format!("vendor{character}name"));
            assert_eq!(rendered, format!("vendor\\u{{{:x}}}name", character as u32));
            assert!(!rendered.chars().any(is_dangerous));
        }
    }

    #[test]
    fn leaves_printable_text_unchanged() {
        for name in [
            "Logitech USB Receiver",
            "Café Keyboard – ünïcödé",
            "キーボード 键盘",
            "e\u{301}",
        ] {
            assert_eq!(escape_field(name), name);
            assert_eq!(escape_control(name), name);
        }
    }

    #[test]
    fn escaping_is_injective_against_literal_backslashes() {
        let literal = "ACME\\u{1b}[31m";
        let control = "ACME\u{1b}[31m";
        assert_ne!(escape_control(literal), escape_control(control));
        assert_eq!(escape_control(literal), "ACME\\\\u{1b}[31m");
        assert_eq!(escape_control(control), "ACME\\u{1b}[31m");
    }

    #[test]
    fn never_emits_a_control_or_bidi_character() {
        for scalar in 0..=0x0010_ffff_u32 {
            let Some(character) = char::from_u32(scalar) else {
                continue;
            };
            let mut rendered = String::new();
            push_escaped(character, &mut rendered);
            assert!(
                !rendered.chars().any(is_dangerous),
                "{scalar:#x} rendered as {rendered:?}"
            );
        }
    }

    #[test]
    fn caps_long_field_values_and_marks_the_cut() {
        let rendered = escape_field(&"A".repeat(MAX_FIELD_CHARS * 2));
        assert!(rendered.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            rendered.chars().count(),
            MAX_FIELD_CHARS + TRUNCATION_MARKER.chars().count()
        );
        assert_ne!(rendered, escape_field(&"A".repeat(MAX_FIELD_CHARS)));
        assert!(!escape_field("Logitech USB Receiver").contains(TRUNCATION_MARKER));
    }

    #[test]
    fn truncation_never_splits_an_escape_sequence() {
        let rendered = escape_field(&"\u{1b}".repeat(MAX_FIELD_CHARS));
        let escaped = rendered.matches("\\u{1b}").count();
        assert_eq!(
            rendered,
            format!("{}{TRUNCATION_MARKER}", "\\u{1b}".repeat(escaped))
        );
        assert!(!rendered.chars().any(is_dangerous));
    }

    #[test]
    fn error_line_escapes_the_chain_without_truncating_it() {
        let context = "usb device 1-4.4 could not be attached to gui-vm because the crosvm control socket did not answer in time";
        let error = anyhow!("Crosvm USB port contains a differ\u{1b}[2Kent device")
            .context(context)
            .context(context)
            .context(context);
        let line = error_line(&error);
        assert!(line.starts_with("ERROR "));
        assert!(line.chars().count() > MAX_FIELD_CHARS);
        assert!(!line.contains(TRUNCATION_MARKER));
        assert!(line.ends_with("Crosvm USB port contains a differ\\u{1b}[2Kent device"));
        assert!(!line.chars().any(is_dangerous));
    }
}
