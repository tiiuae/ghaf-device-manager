# SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
# SPDX-License-Identifier: Apache-2.0
{
  lib,
  rustPlatform,
  pkg-config,
  systemd,
}:
rustPlatform.buildRustPackage {
  pname = "ghaf-device-manager";
  version = "0.1.0";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ systemd ];

  postInstall = ''
    ln -s ghaf-device "$out/bin/vhotplugcli"
  '';

  meta = {
    description = "Crosvm device hotplug manager for Ghaf";
    homepage = "https://github.com/tiiuae/ghaf-device-manager";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "ghaf-device-manager";
  };
}
