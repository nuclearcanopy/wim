# Video playback

Wim is a native music player. Video playback is handed to mpv's normal
graphical player window; Wim does not render video frames in the terminal.

The shared mpv process is started with its GPU video output and
`hwdec=auto-safe`: mpv prefers a hardware decoder it considers safe and falls
back to software decoding when a codec or driver path is unavailable. This is a
best-effort default, not proof that a particular file is hardware-decoded; check
mpv's playback statistics/logs on the target machine when diagnosing performance.
Music items set
`vid=no`, so embedded cover art does not open a window. Video items set
`vid=auto`, allowing mpv to create its ordinary window and own video, audio,
fullscreen, and playback rendering. Wim continues to provide queue and
transport controls over mpv's IPC connection.

While a video is selected, Wim's player workspace explicitly identifies the
separate mpv window and presents a **LYRIC SYNC** button (clickable; `t` by
default) to open subtitle controls. It also advertises the configured key to
open/focus mpv and the configured fullscreen key. Playback continues in mpv
while Wim's subtitle-sync preview is open. This is a separate window, not a
picture-in-picture or always-on-top guarantee. The mpv window title uses Wim's
media label rather than the raw source filename.
TorBox's source picker intentionally continues to show the actual filename so
the user can choose between releases. Common season/episode release patterns
are converted to neutral labels such as `Silo Episode 5 Season 3`; episode
titles are not inferred from release-name suffixes.

No terminal graphics protocol or video-specific Wim setting is required.
