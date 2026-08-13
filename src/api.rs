// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::CString,
    fs,
    os::unix::fs::{PermissionsExt, chown},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
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
    manager::{DeviceManager, request_selector},
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
        Ok(line) => match serde_json::from_str::<Value>(&line) {
            Ok(Value::Object(message)) => {
                if message.get("action").and_then(Value::as_str) == Some("enable_notifications") {
                    *notifications = true;
                }
                handle(manager, message).await
            }
            Ok(_) => json!({"result": "failed", "error": "API message must be a JSON object"}),
            Err(error) => json!({"result": "failed", "error": format!("Invalid JSON: {error}")}),
        },
        Err(error) => json!({"result": "failed", "error": format!("Invalid API message: {error}")}),
    };
    framed.send(response.to_string()).await.is_ok()
}

pub async fn handle<R: CommandRunner>(
    manager: &DeviceManager<R>,
    message: Map<String, Value>,
) -> Value {
    match handle_inner(manager, &message).await {
        Ok(value) => value,
        Err(error) => json!({"result": "failed", "error": error.to_string()}),
    }
}

async fn handle_inner<R: CommandRunner>(
    manager: &DeviceManager<R>,
    message: &Map<String, Value>,
) -> Result<Value> {
    let action = message
        .get("action")
        .and_then(Value::as_str)
        .context("No action specified")?;
    let selector = request_selector(message);
    let vm = message.get("vm").and_then(Value::as_str);
    let disconnected = message.get("disconnected").and_then(Value::as_bool);
    let tag = message.get("tag").and_then(Value::as_str);
    match action {
        "enable_notifications" => Ok(json!({"result": "ok"})),
        "usb_list" => {
            Ok(json!({"result": "ok", "usb_devices": manager.usb_list(disconnected, tag).await?}))
        }
        "usb_attach" => {
            manager.attach_usb(&selector, vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "usb_detach" => {
            manager.detach_usb(&selector, true).await?;
            Ok(json!({"result": "ok"}))
        }
        "usb_suspend" => {
            manager.suspend_usb(vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "usb_resume" => {
            manager.resume_usb(vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "pci_list" => {
            Ok(json!({"result": "ok", "pci_devices": manager.pci_list(disconnected, tag).await?}))
        }
        "pci_attach" => {
            manager.attach_pci(&selector, vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "pci_detach" => {
            manager.detach_pci(&selector, true).await?;
            Ok(json!({"result": "ok"}))
        }
        "pci_suspend" => {
            manager.suspend_pci(vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "pci_resume" => {
            manager.resume_pci(vm).await?;
            Ok(json!({"result": "ok"}))
        }
        "vmm_args" => {
            let vm = vm.context("VM name is required")?;
            let require_pci = message
                .get("require_pci")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(json!({"result": "ok", "vmm_args": manager.vmm_args(vm, require_pci)?}))
        }
        _ => Ok(json!({"result": "failed", "error": format!("Unknown message: {action}")})),
    }
}

fn configure_unix_socket(
    path: &str,
    user: Option<&str>,
    group: Option<&str>,
    mode: Option<&str>,
) -> Result<()> {
    let uid = match user {
        Some(user) => Some(user_id(user)?),
        None => None,
    };
    let gid = match group {
        Some(group) => Some(group_id(group)?),
        None => None,
    };
    if uid.is_some() || gid.is_some() {
        chown(path, uid, gid)?;
    }
    if let Some(mode) = mode {
        let mode = u32::from_str_radix(mode, 8).context("invalid unixSocketMode")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn user_id(name: &str) -> Result<u32> {
    let name = CString::new(name)?;
    // SAFETY: getpwnam returns a process-owned record valid until the next libc lookup.
    let record = unsafe { libc::getpwnam(name.as_ptr()) };
    if record.is_null() {
        bail!("unknown user {}", name.to_string_lossy());
    }
    // SAFETY: null was checked and we copy the scalar field immediately.
    Ok(unsafe { (*record).pw_uid })
}

fn group_id(name: &str) -> Result<u32> {
    let name = CString::new(name)?;
    // SAFETY: getgrnam returns a process-owned record valid until the next libc lookup.
    let record = unsafe { libc::getgrnam(name.as_ptr()) };
    if record.is_null() {
        bail!("unknown group {}", name.to_string_lossy());
    }
    // SAFETY: null was checked and we copy the scalar field immediately.
    Ok(unsafe { (*record).gr_gid })
}
