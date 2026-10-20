# Infinity notification sound

`infinity-notification.wav` is an original synthesized three-note chime made for this project.
It contains no Xbox, game or third-party recording. It follows this repository's GPL-3.0-or-later
license. Regenerate exactly with `python native/overlay/tools/generate_chime.py` (standard library).

800 ms, mono PCM, 48 kHz, signed 16-bit; peak 0.62. The overlay plays it at 65% gain when a toast
first appears. It is embedded in the signed overlay DLL; no network or loose audio file is read.

Playback follows [Microsoft's XAudio2 initialization guidance](https://learn.microsoft.com/en-us/windows/win32/xaudio2/how-to--initialize-xaudio2)
on a dedicated COM-initialized worker. The system XAudio2 DLL is loaded explicitly; no WinMM
import competes with ExtraOptions. Missing audio support leaves visual notifications operational.
