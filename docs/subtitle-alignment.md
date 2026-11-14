# Subtitle alignment

## Local speech-based subtitle alignment

While video plays, Wim's player workspace says that playback is in a separate
mpv window and shows a **LYRIC SYNC** action (`t` by default); click the action
or press `t` to open subtitle controls. mpv owns the picture and playback while
Wim remains the control surface. The subtitle menu shows model readiness and
alignment progress. Press `a` to align the selected text subtitle against
speech in the selected audio track, `p` to load the proposed timing into mpv,
then `y` to accept. Acceptance is disabled until a preview has loaded
successfully. `d` discards a suggestion; `c` cancels an active job or preview;
`n` cycles subtitle tracks. The menu reports the alignment stage, proposed offset, clock-rate
scale, number of matched cues, and a confidence estimate. Weak or inconsistent
matches are refused rather than applied automatically. Manual subtitle delay
remains available, and the source subtitle file is never overwritten. Accepted
results are stored under the user's cache directory and loaded as another mpv
subtitle track.

Recognition runs locally through `whisper.cpp`; FFmpeg extracts up to six
short samples from the video and selected audio stream. No audio, transcript,
or signed stream URL is sent to a network service. This first implementation
intentionally supports local video files and readable text subtitle tracks
(external SRT/VTT/ASS or extractable embedded text tracks). Image-based tracks
and network streams are not supported yet. Temporary audio and rejected
previews are removed after the task; if Wim is forcibly killed, its uniquely
named temporary work directory may need manual cleanup from
`$XDG_CACHE_HOME/wim/subtitle-align` (usually `~/.cache/wim/subtitle-align`).

No model is downloaded automatically. With Home Manager, point the option at a
local Whisper model file; this also installs the FFmpeg and whisper.cpp runtime
tools:

```nix
programs.wim.whisperModelFile = "/home/you/.local/share/whisper/ggml-base.en.bin";
```

Without Home Manager, install `ffmpeg` and `whisper.cpp`, then set
`whisper_model_file` in `~/.config/wim/config.toml`. A model must already exist
at that path. `base.en` is a reasonable English starting point; other model
sizes trade speed and memory for recognition quality.
