// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{os::fd::AsRawFd, sync::Arc, thread, time::Duration};

use anyhow::Result;
use clap::Parser;
use ghaf_device_manager::{Config, DeviceManager, ProcessRunner, api};
use tokio::sync::mpsc;
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Crosvm device manager for Ghaf")]
struct Args {
    #[arg(short, long)]
    config: String,
    #[arg(short = 'a', long = "attach-connected")]
    attach_connected: bool,
    #[arg(short, long)]
    debug: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        error!(error = %format!("{error:#}"), "ghaf-device-manager failed");
        eprintln!("ERROR {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let filter = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .without_time()
        .init();
    let config = Config::load(&args.config)?;
    let manager = Arc::new(DeviceManager::new(config, ProcessRunner)?);
    if args.attach_connected
        && let Err(error) = manager.reconcile().await
    {
        warn!(%error, "initial device reconciliation will be retried");
    }
    let (events, mut receiver) = mpsc::channel::<()>(8);
    spawn_udev_monitor(events)?;
    let reconcile = Arc::clone(&manager);
    let event_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                event = receiver.recv() => {
                    if event.is_none() {
                        break;
                    }
                }
                _ = interval.tick() => {}
            }
            while receiver.try_recv().is_ok() {}
            if let Err(error) = reconcile.reconcile().await {
                warn!(%error, "device reconciliation failed");
            }
        }
    });
    tokio::select! {
        result = api::serve(Arc::clone(&manager)) => result?,
        _ = tokio::signal::ctrl_c() => {},
    }
    event_task.abort();
    Ok(())
}

fn spawn_udev_monitor(sender: mpsc::Sender<()>) -> Result<()> {
    thread::Builder::new()
        .name("udev-monitor".into())
        .spawn(move || {
            let socket = match udev::MonitorBuilder::new().and_then(udev::MonitorBuilder::listen) {
                Ok(socket) => socket,
                Err(error) => {
                    eprintln!("ERROR failed to start udev monitor: {error}");
                    return;
                }
            };
            let mut descriptor = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: descriptor points to one initialized pollfd for the duration of the call.
                if unsafe { libc::poll(&mut descriptor, 1, -1) } <= 0 {
                    continue;
                }
                let relevant = socket.iter().any(|event| {
                    matches!(
                        event.subsystem().and_then(|value| value.to_str()),
                        Some("usb" | "pci")
                    )
                });
                if relevant && sender.blocking_send(()).is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}
