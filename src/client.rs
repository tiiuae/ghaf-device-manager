// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UnixStream},
    time::timeout,
};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_vsock::{VsockAddr, VsockStream};

use crate::protocol::{Action, Response};

const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Transport {
    Unix { path: String },
    Tcp { host: String, port: u16 },
    Vsock { cid: u32, port: u32 },
}

pub async fn request<T, R>(transport: &Transport, message: &T, deadline: Duration) -> Result<R>
where
    T: Serialize + ?Sized,
    R: DeserializeOwned,
{
    timeout(deadline, async {
        match transport {
            Transport::Unix { path } => exchange(UnixStream::connect(path).await?, message).await,
            Transport::Tcp { host, port } => {
                exchange(TcpStream::connect((host.as_str(), *port)).await?, message).await
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

async fn exchange<S, T, R>(stream: S, message: &T) -> Result<R>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: Serialize + ?Sized,
    R: DeserializeOwned,
{
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_MESSAGE));
    framed.send(serde_json::to_string(message)?).await?;
    let response = framed
        .next()
        .await
        .context("API connection closed by remote")??;
    serde_json::from_str::<R>(&response).context("invalid JSON in API response")
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
                TcpStream::connect((host.as_str(), *port)).await?,
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
        .send(serde_json::to_string(&Action::EnableNotifications)?)
        .await?;
    let response = framed
        .next()
        .await
        .context("API connection closed by remote")??;
    match serde_json::from_str::<Response>(&response)? {
        Response::Ok { .. } => {}
        Response::Failed { error } => bail!("failed to enable notifications: {error}"),
    }
    while let Some(message) = framed.next().await {
        callback(serde_json::from_str(&message?)?);
    }
    bail!("API connection closed by remote")
}

#[must_use]
pub fn running_in_vm() -> bool {
    match vsock::get_local_cid() {
        Ok(cid) => cid != u32::MAX && cid != 2 && cid != 1,
        Err(_) => false,
    }
}
