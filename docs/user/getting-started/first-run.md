---
title: First run
description: What you see when SoundCraft starts, how to open the demo song, and where the shortcuts and the log file are.
order: 20
verified: 0.5.0
---

To look around without recording anything, choose **File > New...**, tick **Start from the demo session** and click **Create**. The demo song is made by SoundCraft itself.

## Open the demo session

1. Choose **File > New...**.
2. In the **New Session** window, set a **Name** and a **Sample Rate**.
3. Tick **Start from the demo session**.
4. Click **Create**.

Without that box ticked, **Create** starts an empty session. The window title bar's right side shows the session name and whether you are in the Edit or the Mix window.

## Switch between Edit and Mix

Use **Window > Mix** and **Window > Edit**, or press `Ctrl+=` on Windows and Linux (`Cmd+=` on macOS). The Edit window is where you arrange and cut clips. The Mix window shows one channel strip per track.

## Shortcuts

On Windows and Linux the menus show `Ctrl` in place of the Mac `Cmd` key. Open **Setup > Keyboard Shortcuts...** to see the full list. Two to start with: `Space` plays and stops, and `Ctrl+Space` records.

## Save your work

Choose **File > Save Session**. The first time, the **Save Session As** window asks for a path; sessions use the `.scraft` extension. SoundCraft also asks **Save changes to “name” before closing?** when you close a session with unsaved changes.

## Where the log file is

SoundCraft writes a log file that helps when reporting a problem. On Windows it is `%APPDATA%\SoundCraft\logs\soundcraft.log`. The previous run's log is kept as `soundcraft.1.log`.

## Next

- [Create a session and add tracks](../guides/create-a-session-and-add-tracks.md)
- [Import audio](../guides/import-audio.md)
- [Record a track](../guides/record-a-track.md)
