// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{fs::File, os::fd::AsRawFd, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UnixStream},
    time::timeout,
};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_vsock::{VsockAddr, VsockStream};

const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Transport {
    Unix { path: String },
    Tcp { host: String, port: u32 },
    Vsock { cid: u32, port: u32 },
}

pub async fn request(transport: &Transport, message: Value, deadline: Duration) -> Result<Value> {
    timeout(deadline, async {
        match transport {
            Transport::Unix { path } => exchange(UnixStream::connect(path).await?, message).await,
            Transport::Tcp { host, port } => {
                exchange(
                    TcpStream::connect((host.as_str(), *port as u16)).await?,
                    message,
                )
                .await
            }
            Transport::Vsock { cid, port } => {
                exchange(
                    VsockStream::connect(VsockAddr::new(*cid, *port)).await?,
                    message,
                )
                .await
            }
        }
    })
    .await
    .context("device-manager request timed out")?
}

async fn exchange<S>(stream: S, message: Value) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_MESSAGE));
    framed.send(message.to_string()).await?;
    let response = framed
        .next()
        .await
        .context("API connection closed by remote")??;
    serde_json::from_str(&response).context("invalid JSON in API response")
}

pub async fn listen<F>(transport: &Transport, mut callback: F) -> Result<()>
where
    F: FnMut(Value),
{
    match transport {
        Transport::Unix { path } => {
            listen_stream(UnixStream::connect(path).await?, &mut callback).await
        }
        Transport::Tcp { host, port } => {
            listen_stream(
                TcpStream::connect((host.as_str(), *port as u16)).await?,
                &mut callback,
            )
            .await
        }
        Transport::Vsock { cid, port } => {
            listen_stream(
                VsockStream::connect(VsockAddr::new(*cid, *port)).await?,
                &mut callback,
            )
            .await
        }
    }
}

async fn listen_stream<S, F>(stream: S, callback: &mut F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(Value),
{
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_MESSAGE));
    framed
        .send(serde_json::json!({"action": "enable_notifications"}).to_string())
        .await?;
    let response = framed
        .next()
        .await
        .context("API connection closed by remote")??;
    let response: Value = serde_json::from_str(&response)?;
    if response.get("result").and_then(Value::as_str) != Some("ok") {
        bail!("failed to enable notifications: {response}");
    }
    while let Some(message) = framed.next().await {
        callback(serde_json::from_str(&message?)?);
    }
    bail!("API connection closed by remote")
}

nix::ioctl_read!(get_local_cid, 7, 0xb9, u32);

pub fn running_in_vm() -> bool {
    let Ok(file) = File::open("/dev/vsock") else {
        return false;
    };
    let mut cid = 0u32;
    // SAFETY: the ioctl writes one u32 into the valid pointer supplied here.
    if unsafe { get_local_cid(file.as_raw_fd(), &mut cid) }.is_err() {
        return false;
    }
    cid != u32::MAX && cid != 2 && cid != 1
}
