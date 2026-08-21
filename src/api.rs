// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    os::unix::fs::{PermissionsExt, chown},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, UnixListener},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
use tracing::{info, warn};

use crate::{
    config::ApiTransport,
    crosvm::CommandRunner,
    manager::DeviceManager,
    protocol::{
        Action, PciListResponse, Response, ResponsePayload, UsbListResponse, VmmArgsResponse,
    },
    unix_ids::{group_id, user_id},
};

const MAX_MESSAGE: usize = 1024 * 1024;

/// Concurrent API connections served at once, across all transports. Clients
/// beyond this wait in the kernel backlog rather than each costing the daemon a
/// file descriptor, a `MAX_MESSAGE` buffer and a notification subscription.
const MAX_CONNECTIONS: usize = 64;

/// Applied after a failed `accept`, so a listener that keeps failing (out of
/// file descriptors, for instance) cannot spin the CPU.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// `accept` failures are usually transient and caller-induced: an aborted
/// handshake, or the descriptor table being full. Dropping the listener on one
/// of those would let any client that can reach a transport stop the daemon, so
/// they are logged and retried instead.
async fn accept_failed(transport: &str, error: &std::io::Error) {
    warn!(transport, %error, "failed to accept API connection");
    tokio::time::sleep(ACCEPT_BACKOFF).await;
}

pub async fn serve<R: CommandRunner + 'static>(manager: Arc<DeviceManager<R>>) -> Result<()> {
    if !manager.config.general.api.enable {
        std::future::pending::<()>().await;
        return Ok(());
    }
    let api = &manager.config.general.api;
    if api.transports.is_empty() {
        std::future::pending::<()>().await;
        return Ok(());
    }
    let transports = api.transports.clone();
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();
    for transport in transports {
        let manager = manager.clone();
        let connections = Arc::clone(&connections);
        match transport {
            ApiTransport::Unix => {
                let path = &api.unix_socket;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if path.exists() {
                    fs::remove_file(path)?;
                }
                let listener = UnixListener::bind(path)
                    .with_context(|| format!("failed to bind Unix socket {}", path.display()))?;
                configure_unix_socket(
                    path,
                    api.unix_socket_user.as_deref(),
                    api.unix_socket_group.as_deref(),
                    api.unix_socket_mode.as_deref(),
                )?;
                info!(path = %path.display(), "API listening on Unix socket");
                tasks.spawn(async move {
                    loop {
                        let permit = Arc::clone(&connections).acquire_owned().await?;
                        match listener.accept().await {
                            Ok((stream, _)) => spawn_connection(&manager, stream, permit),
                            Err(error) => accept_failed("unix", &error).await,
                        }
                    }
                });
            }
            ApiTransport::Tcp => {
                let port = api.port.map_or(Ok(api.tcp_port), TryFrom::try_from)?;
                let address = format!("{}:{}", api.host, port);
                let listener = TcpListener::bind(&address)
                    .await
                    .with_context(|| format!("failed to bind TCP address {}:{}", api.host, port))?;
                info!(%address, "API listening on TCP");
                tasks.spawn(async move {
                    loop {
                        let permit = Arc::clone(&connections).acquire_owned().await?;
                        match listener.accept().await {
                            Ok((stream, _)) => spawn_connection(&manager, stream, permit),
                            Err(error) => accept_failed("tcp", &error).await,
                        }
                    }
                });
            }
            ApiTransport::Vsock => {
                let port = api.vsock_port;
                let allowed = api.allowed_cids.clone();
                let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
                    .with_context(|| format!("failed to bind VSOCK port {port}"))?;
                info!(port, "API listening on VSOCK");
                tasks.spawn(async move {
                    loop {
                        let permit = Arc::clone(&connections).acquire_owned().await?;
                        let (stream, address) = match listener.accept().await {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                accept_failed("vsock", &error).await;
                                continue;
                            }
                        };
                        if !allowed.is_empty() && !allowed.contains(&address.cid()) {
                            warn!(cid = address.cid(), "rejected VSOCK client");
                            continue;
                        }
                        spawn_connection(&manager, stream, permit);
                    }
                });
            }
        }
    }
    match tasks.join_next().await {
        Some(result) => result??,
        None => bail!("no API transports configured"),
    }
    Ok(())
}

/// Serves one accepted client, holding `permit` until the connection closes so
/// the slot is only returned once the resources really are.
fn spawn_connection<R, S>(manager: &Arc<DeviceManager<R>>, stream: S, permit: OwnedSemaphorePermit)
where
    R: CommandRunner + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let manager = Arc::clone(manager);
    tokio::spawn(async move {
        connection(manager, stream).await;
        drop(permit);
    });
}

async fn connection<R, S>(manager: Arc<DeviceManager<R>>, stream: S)
where
    R: CommandRunner + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_MESSAGE));
    let mut notifications = manager.subscribe();
    let mut enabled = false;
    loop {
        if enabled {
            tokio::select! {
                line = framed.next() => {
                    if !handle_line(&manager, &mut framed, line, &mut enabled).await {
                        break;
                    }
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(value) => {
                            if framed.send(value.to_string()).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (),
                        Err(_) => break,
                    }
                }
            }
        } else {
            let line = framed.next().await;
            if !handle_line(&manager, &mut framed, line, &mut enabled).await {
                break;
            }
        }
    }
}

async fn handle_line<R, S>(
    manager: &DeviceManager<R>,
    framed: &mut Framed<S, LinesCodec>,
    line: Option<Result<String, tokio_util::codec::LinesCodecError>>,
    notifications: &mut bool,
) -> bool
where
    R: CommandRunner,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(line) = line else {
        return false;
    };
    let response = match line {
        Ok(line) => match serde_json::from_str::<Action>(&line) {
            Ok(message) => {
                if matches!(&message, Action::EnableNotifications) {
                    *notifications = true;
                }
                handle(manager, message).await
            }
            Err(error) => Response::failed(format!("Invalid API message: {error}")),
        },
        Err(error) => Response::failed(format!("Invalid API message: {error}")),
    };
    // `Response` is a plain string/array structure, so encoding cannot actually
    // fail; dropping the connection is still a better answer than a panic.
    let Ok(encoded) = serde_json::to_string(&response) else {
        return false;
    };
    framed.send(encoded).await.is_ok()
}

pub async fn handle<R: CommandRunner>(manager: &DeviceManager<R>, message: Action) -> Response {
    handle_inner(manager, message).await.into()
}

async fn handle_inner<R: CommandRunner>(
    manager: &DeviceManager<R>,
    message: Action,
) -> Result<ResponsePayload> {
    match message {
        Action::EnableNotifications => Ok(ResponsePayload::empty()),
        Action::UsbList { disconnected, tag } => Ok(ResponsePayload::UsbList(UsbListResponse {
            usb_devices: manager.usb_list(disconnected, tag.as_deref()).await?,
        })),
        Action::UsbAttach { selector, vm } => {
            let selector: crate::manager::Selector = selector.into();
            manager.attach_usb(&selector, vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::UsbDetach { selector } => {
            let selector: crate::manager::Selector = selector.into();
            manager.detach_usb(&selector, true).await?;
            Ok(ResponsePayload::empty())
        }
        Action::UsbSuspend { vm } => {
            manager.suspend_usb(vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::UsbResume { vm } => {
            manager.resume_usb(vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::PciList { disconnected, tag } => Ok(ResponsePayload::PciList(PciListResponse {
            pci_devices: manager.pci_list(disconnected, tag.as_deref()).await?,
        })),
        Action::PciAttach { selector, vm } => {
            let selector: crate::manager::Selector = selector.into();
            manager.attach_pci(&selector, vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::PciDetach { selector } => {
            let selector: crate::manager::Selector = selector.into();
            manager.detach_pci(&selector, true).await?;
            Ok(ResponsePayload::empty())
        }
        Action::PciSuspend { vm } => {
            manager.suspend_pci(vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::PciResume { vm } => {
            manager.resume_pci(vm.as_deref()).await?;
            Ok(ResponsePayload::empty())
        }
        Action::VmmArgs {
            vm, require_pci, ..
        } => Ok(ResponsePayload::VmmArgs(VmmArgsResponse {
            vmm_args: manager.vmm_args(&vm, require_pci)?,
        })),
    }
}

/// `bind` publishes the socket under the process umask, which a unit file is
/// free to set to `0000`. Narrowing it to owner-only first means the socket is
/// never reachable by anyone the configuration did not name; ownership is
/// applied before `unixSocketMode` widens it again.
fn configure_unix_socket(
    path: &Path,
    user: Option<&str>,
    group: Option<&str>,
    mode: Option<&str>,
) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let uid = user.map(user_id).transpose()?;
    let gid = group.map(group_id).transpose()?;
    if uid.is_some() || gid.is_some() {
        chown(path, uid, gid)?;
    }
    if let Some(mode) = mode {
        let mode = u32::from_str_radix(mode, 8).context("invalid unixSocketMode")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}
