// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use clap::Parser;
use ghaf_device_manager::{Config, DeviceManager, ProcessRunner, api};
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const RECONCILE_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const RECONCILE_SAFETY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(about = "Crosvm device manager for Ghaf")]
struct Args {
    #[arg(short, long)]
    config: PathBuf,
    #[arg(short = 'a', long = "attach-connected")]
    attach_connected: bool,
    #[arg(short, long)]
    debug: bool,
}

#[tokio::main]
async fn main() {
    let local = LocalSet::new();
    let result = local.run_until(run()).await;
    if let Err(error) = result {
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
    // Registered before the gate closes: a stop landing anywhere in the
    // startup window must still reach the restore below.
    let shutdown = shutdown()?;
    if manager.close_usb_gate()? {
        info!("host drivers now bind only to USB devices that no rule routes");
    }
    // Every way out of daemon() passes the reopen below, early failures
    // included: exiting with the gate closed would leave USB hotplug dead.
    let outcome = daemon(&args, &manager, shutdown).await;
    // An API connection racing this reopen can release one interface a
    // moment late; the next daemon start probes it back.
    if let Err(error) = manager.open_usb_gate() {
        warn!(%error, "failed to hand USB driver binding back to the kernel");
    }
    outcome
}

async fn daemon(
    args: &Args,
    manager: &Arc<DeviceManager<ProcessRunner>>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // Subscribed before the first scan, so an add event racing that scan
    // queues instead of vanishing into the 30 second safety interval.
    let (events, mut receiver) = mpsc::channel::<()>(8);
    spawn_udev_monitor(events)?;
    let initial_reconcile_succeeded = if args.attach_connected {
        match manager.reconcile().await {
            Ok(()) => !manager.deferred(),
            Err(error) => {
                warn!(%error, "initial device reconciliation will be retried");
                false
            }
        }
    } else {
        false
    };
    let reconcile = Arc::clone(manager);
    let mut event_task = tokio::spawn(async move {
        let mut delay = if initial_reconcile_succeeded {
            RECONCILE_SAFETY_INTERVAL
        } else {
            RECONCILE_RETRY_INTERVAL
        };
        loop {
            tokio::select! {
                event = receiver.recv() => {
                    if event.is_none() {
                        break;
                    }
                }
                () = tokio::time::sleep(delay) => {}
            }
            while receiver.try_recv().is_ok() {}
            delay = match reconcile.reconcile().await {
                Ok(()) if reconcile.deferred() => RECONCILE_RETRY_INTERVAL,
                Ok(()) => RECONCILE_SAFETY_INTERVAL,
                Err(error) => {
                    warn!(%error, "device reconciliation failed");
                    RECONCILE_RETRY_INTERVAL
                }
            };
        }
    });
    let outcome = tokio::select! {
        result = api::serve(Arc::clone(manager)) => result,
        // The reconcile loop is the only thing that ever binds a driver
        // while the gate is closed, so its death is the daemon's death.
        stopped = &mut event_task => Err(match stopped {
            Ok(()) => anyhow!("device reconciliation stopped"),
            Err(error) => anyhow!("device reconciliation panicked: {error}"),
        }),
        () = shutdown => Ok(()),
    };
    event_task.abort();
    // Awaited so no reconcile outlives the gate reopen; skipped when the
    // select arm consumed the handle, since a second poll panics in tokio.
    if !event_task.is_finished() {
        let _ = event_task.await;
    }
    outcome
}

/// SIGTERM and SIGINT both land here. Registration happens at the call,
/// before the gate closes, so a stop during startup still restores it.
fn shutdown() -> Result<impl Future<Output = ()>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    })
}

fn spawn_udev_monitor(sender: mpsc::Sender<()>) -> Result<()> {
    let socket = udev::MonitorBuilder::new()?.listen()?;
    let mut socket = AsyncFd::new(socket)?;
    tokio::task::spawn_local(async move {
        loop {
            let mut guard = match socket.readable_mut().await {
                Ok(guard) => guard,
                Err(error) => {
                    eprintln!("ERROR failed to start udev monitor: {error}");
                    break;
                }
            };
            let relevant = guard.get_inner_mut().iter().any(|event| {
                matches!(
                    event.subsystem().and_then(|value| value.to_str()),
                    Some("usb" | "pci")
                )
            });
            guard.clear_ready();
            if relevant && sender.send(()).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}
