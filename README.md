<!--
SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Ghaf Device Manager

`ghaf-device-manager` manages runtime USB and PCI/VFIO assignment for Ghaf
virtual machines using Crosvm. QEMU targets continue to use
[`vhotplug`](https://github.com/tiiuae/vhotplug).

The daemon intentionally accepts the vhotplug JSON configuration and protocol.
This lets the existing Ghaf USB applet, kill-switch, power services, and VM
startup scripts work without a coordinated userspace migration.

## Commands

- `ghaf-device-manager -a -c /etc/vhotplug.conf` starts the daemon and attaches
  eligible connected devices.
- The daemon reconciles immediately on USB and PCI udev events. After a
  successful reconciliation it performs a 30-second safety scan; failures keep
  a two-second retry interval so VMs that are still starting are found promptly.
- `ghaf-device usb list` uses the native CLI name.
- `vhotplugcli usb list` is the compatibility alias installed by the Nix
  package.

The API defaults remain `/var/lib/vhotplug/vhotplug.sock` on the host and VSOCK
port 2000 for guest tools. The daemon uses only the transports explicitly
enabled in `general.api.transports`.

## Architecture Boundary

The manager controls runtime USB and PCI devices. Static ACPI tables, device
trees, MMIO ranges, IRQs, BPMP, DCE, and platform devices remain declarative
Ghaf and microvm.nix configuration. The `vmm args` action is a compatibility
renderer and does not make those resources hot-pluggable.

## Development

```console
nix develop --command cargo test --all-targets
nix develop --command cargo clippy --all-targets -- -D warnings
nix fmt -- --check .
nix develop --command reuse lint
```
