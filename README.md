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

## Host Driver Binding

The manager decides which USB devices host drivers may bind to, on
deployments with USB rules; a configuration that routes no USB keeps stock
kernel binding untouched. It turns `drivers_autoprobe` off for the USB bus at
startup, then binds drivers to the devices no rule routes and leaves the rest
alone. A routed device is never
bound, because claiming it for a VM detaches the host driver mid-command, and
that wedges some bridge firmware until the device is power cycled.

USB binds in two stages, which is what makes the split possible: the generic
device driver sets the configuration and creates the interfaces, and interface
drivers bind to those. A routed device still gets the first stage, so its
descriptors and rule matching read as they did before. A device bound before
the manager started is released through the driver's own disconnect, which
drains outstanding commands rather than killing them, and a device a VM
already holds through `usbfs` is left alone.

A device that enumerates while a pass is under way is configured before it
is attached: Crosvm reads descriptors once, at attach, and an unconfigured
device would show the guest vendor-specific interfaces for good.

The manager hands binding back when it stops, restoring the attribute and
probing whatever the gate left driverless, so a stopped host behaves as
stock. Devices a VM still holds are left out of that: an interface the guest
has not claimed reads as driverless, and probing a host driver onto one would
put host and guest on the same device at once. A deployment that routes no
USB never writes the attribute at all, on the way in or out, so a host that
keeps `drivers_autoprobe` off for its own reasons stays that way. After a
crash the bus stays gated until the manager restarts, so a routed device can
never bind a host driver through the gap.

A routed device marked permanently disconnected is handed back to the host:
the mark means the device belongs to the host again, so keeping it unbound
would leave it dead on both sides. It stays host-bound until the mark is
lifted. A routed device whose VM is not running stays reserved and unbound,
because it is attached as soon as that VM appears.

A `driverPath` selector matches through a bound driver's path, which the gate
takes off, so the verdict is remembered for as long as the device stays
plugged in: such a device is released once and then left alone, rather than
rebound and released on every pass. Routing by ids, names or classes is still
the more predictable choice.

A rule matching a hub is reported and skipped rather than obeyed: claiming a
hub would collapse everything behind it.

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
