---
title: SoundCraft does not open on Windows
description: What to try when SoundCraft flashes in Task Manager, closes right after starting or never shows a window on Windows 11.
order: 10
verified: 0.5.0
---

Start SoundCraft from PowerShell with the Vulkan graphics backend: run `$env:WGPU_BACKEND = "vulkan"`, then start `soundcraft.exe` from the same window. The steps are below.

## Symptoms

- You start SoundCraft and nothing appears, or it shows in Task Manager for a moment and disappears.
- The window flashes and SoundCraft exits about a tenth of a second later.
- The log file ends with `Parent device is lost`.

## Fix

1. Open PowerShell.
2. Go to the folder that contains `soundcraft.exe`. If you used the installer, that is the `SoundCraft` folder under `Program Files`. If you used the portable zip, it is the folder you unzipped.
3. Run these lines:

        $env:WGPU_BACKEND = "vulkan"
        & .\soundcraft.exe
        Remove-Item Env:WGPU_BACKEND

4. If SoundCraft opens, you can keep starting it this way. The last line restores the default for later launches in that window.

Do not set `WGPU_BACKEND` to `gl`. According to the README it can bring back a driver crash on some systems.

## Why this happens

On Windows, SoundCraft draws with DirectX 12 only. It leaves OpenGL out on purpose, because starting OpenGL can crash some graphics drivers (the README names AMD's `atio6axx.dll`) before the window appears.

Version 0.5.0 also asks DirectX 12 to use a shader compiler file called `dxcompiler.dll`, which SoundCraft does not ship. On some PCs this ends with `Parent device is lost` right after the window appears ([issue 185](https://github.com/storytold/soundcraft/issues/185)). A change that uses the compiler built into Windows instead is merged after 0.5.0 and not in the 0.5.0 files. Until a newer release is out, use the Vulkan steps above.

## Find the log file

The log is `%APPDATA%\SoundCraft\logs\soundcraft.log`. Attach it when you report a problem.

## Still stuck?

Ask in [Discord](https://discord.gg/artcraft) or see [Report a problem](file-a-bug.md). Include your SoundCraft version, your graphics card and the end of the log file.
