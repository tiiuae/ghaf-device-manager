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
    task::JoinSet,
};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
use tracing::{info, warn};

use crate::{
    crosvm::CommandRunner,
    manager::DeviceManager,
    protocol::{
        Action, PciListResponse, Response, ResponsePayload, UsbListResponse, VmmArgsResponse,
    },
    unix_ids::{group_id, user_id},
};

const MAX_MESSAGE: usize = 1024 * 1024;

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
    let mut tasks = JoinSet::new();
    for transport in transports {
        match transport.as_str() {
            "unix" => {
                let path = api.unix_socket.clone();
                if let Some(parent) = Path::new(&path).parent() {
                    fs::create_dir_all(parent)?;
                }
                if Path::new(&path).exists() {
                    fs::remove_file(&path)?;
                }
                let listener = UnixListener::bind(&path)
                    .with_context(|| format!("failed to bind Unix socket {path}"))?;
                configure_unix_socket(
                    &path,
                    api.unix_socket_user.as_deref(),
                    api.unix_socket_group.as_deref(),
                    api.unix_socket_mode.as_deref(),
                )?;
                info!(%path, "API listening on Unix socket");
                let manager = Arc::clone(&manager);
                tasks.spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await?;
                        tokio::spawn(connection(Arc::clone(&manager), stream));
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
            }
            "tcp" => {
                let address = format!("{}:{}", api.host, api.port);
                let listener = TcpListener::bind(&address)
                    .await
                    .with_context(|| format!("failed to bind TCP address {address}"))?;
                info!(%address, "API listening on TCP");
                let manager = Arc::clone(&manager);
                tasks.spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await?;
                        tokio::spawn(connection(Arc::clone(&manager), stream));
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
            }
            "vsock" => {
                let port = api.port;
                let allowed = api.allowed_cids.clone();
                let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
                    .with_context(|| format!("failed to bind VSOCK port {port}"))?;
                info!(port, "API listening on VSOCK");
                let manager = Arc::clone(&manager);
                tasks.spawn(async move {
                    loop {
                        let (stream, address) = listener.accept().await?;
                        if !allowed.is_empty() && !allowed.contains(&address.cid()) {
                            warn!(cid = address.cid(), "rejected VSOCK client");
                            continue;
                        }
                        tokio::spawn(connection(Arc::clone(&manager), stream));
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
            }
            other => bail!("unsupported API transport {other}"),
        }
    }
    match tasks.join_next().await {
        Some(result) => result??,
        None => bail!("no API transports configured"),
    }
    Ok(())
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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
    framed
        .send(serde_json::to_string(&response).unwrap())
        .await
        .is_ok()
}

pub async fn handle<R: CommandRunner>(manager: &DeviceManager<R>, message: Action) -> Response {
    match handle_inner(manager, message).await {
        Ok(payload) => Response::ok(payload),
        Err(error) => Response::failed(error.to_string()),
    }
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

fn configure_unix_socket(
    path: &str,
    user: Option<&str>,
    group: Option<&str>,
    mode: Option<&str>,
) -> Result<()> {
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
