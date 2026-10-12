---
title: Install SoundCraft
description: Download SoundCraft for Windows 11, macOS or Linux from GitHub, or install it with ArtCraft Launcher.
order: 10
verified: 0.5.0
---

Download the file for your system from the [latest release](https://github.com/storytold/soundcraft/releases/latest) and install it. In the file names, `<version>` stands for the version number, for example `0.5.0`. The release also lists a checksum for every file in `SHA256SUMS.txt`.

## Windows 11

1. Open the [latest release](https://github.com/storytold/soundcraft/releases/latest).
2. Download `soundcraft-<version>-windows-x64.msi`. It fits most PCs (Intel and AMD, 64-bit).
3. Run the file and follow the installer. It installs for all users, so Windows may ask for administrator approval.
4. Start **SoundCraft** from the Start menu.

The release also has an `arm64` installer for Snapdragon and other Arm PCs. To run SoundCraft without installing, download `soundcraft-<version>-windows-x64-portable.zip` instead, unzip it and run `soundcraft.exe`.

If the window never appears, see [SoundCraft does not open on Windows](../troubleshooting/soundcraft-does-not-open-on-windows.md).

## macOS

1. Download `soundcraft-<version>-macos-universal.dmg`. It works on Apple silicon and Intel Macs. It is signed and notarized.
2. Open it and drag SoundCraft to Applications.

## Linux

Pick one file from the release. Replace `x86_64` with `aarch64` on Arm.

| File                                         | Notes                                                        |
|----------------------------------------------|--------------------------------------------------------------|
| `soundcraft-<version>-linux-x86_64.AppImage` | Runs on most distributions. Make it executable, then run it. |
| `soundcraft-<version>-linux-x86_64.deb`      | Debian and Ubuntu.                                           |
| `soundcraft-<version>-linux-x86_64.rpm`      | Fedora, RHEL and openSUSE.                                   |
| `soundcraft-<version>-linux-x86_64.flatpak`  | Sandboxed. Install with `flatpak install --user <file>`.     |
| `soundcraft-<version>-linux-x86_64.tar.gz`   | Unpack anywhere.                                             |

The release also has a RISC-V tarball (`soundcraft-<version>-linux-riscv64.tar.gz`) and a FreeBSD tarball (`soundcraft-<version>-freebsd-x86_64.tar.gz`).

## In a browser

`soundcraft-web-<version>.zip` is a static site that runs in a modern browser. Host it on any static web server.

## Install with ArtCraft Launcher

ArtCraft Launcher lists SoundCraft, so you can install and update it from there. See the [ArtCraft Launcher docs](https://getartcraft.com/docs/craft-launcher).

## Still stuck?

Ask in [Discord](https://discord.gg/artcraft) or open an issue in the [issue tracker](https://github.com/storytold/soundcraft/issues).
