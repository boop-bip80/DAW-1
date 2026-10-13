---
title: Can I choose my audio interface?
description: SoundCraft plays and records through your system's default devices and has no audio device picker or ASIO support yet.
order: 30
verified: 0.6.1
---

Not yet. SoundCraft plays through the default output device and records from the default input device. There is no device or buffer size setting, and no ASIO support on Windows.

## What to do

Set your interface as the default playback and recording device in your system's sound settings, then start SoundCraft. The **Playback Engine** window (**Setup > Playback Engine...**) shows the output device, the device rate and the input device in use. The window also has a **Reconnect audio device** button.

Choosing an audio interface and buffer size is on the project's list of things still missing. A request for an audio driver option is open as [issue 213](https://github.com/storytold/soundcraft/issues/213).
