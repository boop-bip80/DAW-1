---
title: Record a track
description: Record-enable an audio track in SoundCraft and record from your computer's input device.
order: 30
verified: 0.6.1
---

Click the red record-enable button in the track header, then press the Record button or `Ctrl+Space` (`Cmd+Space` on macOS). Press Stop (`Space`) to finish.

## Steps

1. Add an audio track if you have none. See [Create a session and add tracks](create-a-session-and-add-tracks.md).
2. In the track's header, click the red round button. Its tooltip is **Record enable**. With the track selected, `Shift+R` does the same.
3. Optional: click the **I** button in the header to hear the input through the track (**Input monitoring**).
4. Press `Ctrl+Space` (`Cmd+Space` on macOS), or click the Record button in the transport controls.
5. Play or sing. SoundCraft records from the current playhead.
6. Press `Space` to stop. The status line reports how many clips were recorded.

If no track is record-enabled, SoundCraft does not record and says: Record-enable a track first (the red button in its header).

SoundCraft writes each take to a WAV file in the session's `Audio Files` folder while you record, so a crash during a take keeps the audio recorded up to that point. Until you first save the session, the takes go to the `Autosave` folder in SoundCraft's settings folder.

## Punch in and loop recording

- Select a time range first and SoundCraft records into that selection and trims the take back to it.
- Turn on **Options > Pre/Post-Roll** to start recording a little before the selection and run on after it.
- Turn on **Options > Loop Record** with a selection to record the range over and over, one take each pass.

## Which input is used

SoundCraft records from your system's default input device. The **Playback Engine** window (**Setup > Playback Engine...**) shows the input device once recording has opened it. Choosing a different audio interface or driver inside SoundCraft is not available yet.

## If nothing is recorded

The window message **Cannot record:** is followed by the reason. Include it when you [report a problem](../troubleshooting/file-a-bug.md).
