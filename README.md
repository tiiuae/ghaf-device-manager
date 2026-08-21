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

## Security Model

The API is unauthenticated on every transport. Access control is therefore
entirely a deployment concern:

- **Unix** — the only transport with an access check. The socket is created
  owner-only and then narrowed or widened by `unixSocketUser`,
  `unixSocketGroup` and `unixSocketMode`. Grant it to the applet's group, not
  to everyone.
- **VSOCK** — reachable by any guest that can open a VSOCK connection to the
  host. Set `general.api.allowedCids`; an empty list allows every CID.
- **TCP** — plaintext and unauthenticated. Keep `host` on the loopback address.

A caller is not bound to a VM. Any client that reaches a transport may name any
`vm` in a request, so it can attach or detach devices belonging to another VM,
suspend every VM's devices, or mark a device permanently disconnected. It can
also make the host rebind PCI devices to `vfio-pci` through `vmm args`.
Configuration rules bound *which* devices are reachable, not *who* may ask.

## Development

```console
nix develop --command cargo test --all-targets
nix develop --command cargo clippy --all-targets --all-features -- -D warnings
nix develop --command cargo fmt --all -- --check
nix develop --command cargo audit
nix develop --command reuse lint
nix fmt -- --check flake.nix nix/package.nix
nix build .#default -L
```

`nix build` packages the git tree, so a new file must be at least `git add`ed
before it will compile there.
