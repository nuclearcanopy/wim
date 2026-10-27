# audio quality

measurements behind `src/eq.rs`. everything below was measured with
ffmpeg 8.1.2 (the same libavfilter mpv 0.41 links) on 48 kHz sine
tones and pink noise, and cross-checked end to end through the real
`wim` binary with a shimmed `mpv --ao=pcm`.

## the flat eq was not coloring the sound

reported symptom: "absurd amounts of bass" on a navidrome stream that
sounds correct in the web client. the eq chain was the prime suspect,
since `build_af_filter` used to emit ten `equalizer` stages
unconditionally, even with every gain at 0 dB. it measured innocent:

| path | residual vs dry signal |
| --- | --- |
| ffmpeg, flat 10-band chain + astats tap | -89.8 dB rms (-77.5 dB peak) |
| ffmpeg, astats tap alone | -172.3 dB rms (same as an `anull` control) |
| mpv json-ipc, exact app sequence | -95.9 dB rms |
| real `wim` binary, flat config | -77.7 dB rms, flat within 0.02 dB per octave band |

the chain also does not renegotiate anything: mpv reports
`44100Hz stereo 2ch floatp` in and out with the graph inserted, with
no `aresample`/downmix stage added, and a capture through a pipewire
null sink (so the real ao path, resampling 44.1 -> 48 kHz) matched the
unfiltered capture to within 0.02 dB in every octave band. `astats`
with `reset=1` is bit-transparent, so the level meter tap stays.

ten identity biquads are still not free (that -77 dB of float rounding
residue is the cascade, not the tap), so a flat eq now emits only the
`astats` stage. that is a correctness guarantee rather than a fix for
the reported symptom.

## band interaction was real

the bands are correct in isolation: a single +12 dB request at 31 Hz
measured +12.00 dB at 31 Hz, and `width_type=o:width=1` puts the -6 dB
points an octave apart as intended. the defect was that ten
1-octave-wide peaking filters centered an octave apart sum, so the
realized curve sat above the requested one:

| request | realized before | realized after |
| --- | --- | --- |
| +6 dB at 31 Hz ("bass boost" band 1) | +7.05 dB | +5.90 dB |
| +5 dB at 62 Hz | +6.70 dB | +4.83 dB |
| +3 dB at 125 Hz | +4.31 dB | +2.85 dB |
| +1 dB at 250 Hz | +1.77 dB | +0.91 dB |
| 0 dB at 500 Hz | +0.33 dB | -0.04 dB |
| +6 dB on all ten bands | +7.41 to +9.27 dB at centers | +5.72 to +6.39 dB |
| +12 dB at 31 Hz only, measured at 62 Hz | +2.51 dB | +0.05 dB |
| same, measured at 125 Hz | +0.52 dB | +0.09 dB |

so every boost preset was ~1.3 dB hotter in the bass than its own
declared numbers, and a fully pushed eq overshot by up to +3.3 dB.

`compensated_gains` fixes this by solving the band interaction instead
of passing the requested curve straight through. the spill of one
band onto the centers `d` octaves away is fixed by the geometry (it
does not depend on frequency or sample rate), measured as
`BAND_SPILL = [1.0, 0.208, 0.0433, 0.0100, 0.0024]` by driving one
band at +12 dB and reading the octave centers either side of it. that
makes a symmetric, diagonally dominant 10x10 toeplitz matrix; solving
`spill * filter_gains = requested_gains` gives the filter gains. a
single +12 dB request becomes +12.54 dB on its own band and -2.61 dB
on its neighbour, which is what cancels the smear.

the bands are deliberately left 1 octave wide. narrowing them reduces
overlap but ripples badly between centers; with all ten bands at +6 dB,
`width=0.7` measured +4.2 to +7.8 dB and `width=0.5` measured +2.6 to
+6.9 dB, against +4.7 to +6.4 dB for the compensated 1-octave chain.

## the stream wim asked for was not pinned

`stream_url` used to send `/rest/stream?id=...&c=wim` with no `format`
and no `maxBitRate`, leaving the encode entirely to navidrome. that is
not the same decision the web client gets:

- navidrome auto-registers a player per (client name, user), so `wim`
  and the web ui are two separate player records with their own
  transcoding profile and max bit rate.
- a player with max bit rate 0 and no profile is served raw, but one
  with a cap and no forced format falls back to
  `DefaultDownsamplingFormat` (mp3 192 kbps; the other shipped
  profiles are opus 128 and aac 256).
- the web ui goes through the transcode-decision endpoint instead, and
  deliberately ignores the player's forced format because it may not
  be browser-compatible.

so a server-side profile on the `wim` player produces exactly the
reported asymmetry: a lossy, band-limited re-encode in wim and the
original in the browser, from one server and one track. the cache makes
it stick, since `download.rs` saves whatever `stream_url` returns.

`stream_url` now sends `format=raw` unless `transcode_format` or
`max_bitrate_kbps` is set, which is the subsonic idiom for "do not
transcode" and matches what the browser ends up playing. this was not
confirmed against the user's server (no credentials), so it is the
leading candidate, not a measured root cause. cached copies downloaded
before this change are still whatever the server sent; clearing the
cache dir forces a re-fetch.

## ruled out

- the eq chain, flat or boosted (above).
- the mpv-side volume path. there are no volume keybinds; `base_volume`
  is read back off mpv (100 with no persisted volume, which is mpv's
  own default, i.e. no digital gain) and every `set_volume` call passes
  `base_volume` or `base_volume * t` for `t` in 0..1, so nothing can
  exceed unity and clip. `replaygain=track` is on, which attenuates.
- partial downloads being played as if complete. `download.rs` writes
  to a `.part` sibling and renames on success, and
  `CacheIndex::cached_path` only ever stats the final name.
- the output path: no `~/.config/mpv/mpv.conf`, easyeffects' output
  chain is a bypassed near-flat equalizer (its bass-heavy curve is on
  the `[sie]` microphone chain), and both pipewire sinks are plain
  `front-left,front-right`.

## still open

the reported bass was not reproduced locally. two more things worth
knowing while hunting it:

- the `e` key (`cycle_eq`) and the eq menu's j/k rows both apply a
  preset to the running mpv immediately, but only the eq menu's esc
  path writes `eq_preset`/`eq_gains` back to the config. one `e` press
  moves flat -> "bass boost" and leaves `eq_preset = "flat"` on disk,
  so the config file is not evidence of what is actually playing.
- mpv survives an unclean `wim` exit (sigkill, closed terminal). the
  child is only asked to quit from `Drop for Mpv`. a surviving mpv that
  is still playing, plus resume-from-`persistence` loading the same
  track at the same position into a fresh mpv, gives two overlapping
  copies: measured +5.6 dB at 20-45 Hz and +4.1 dB at 45-90 Hz against
  +3.0 dB broadband at a 3 ms offset, i.e. a bass tilt plus comb
  filtering. `PR_SET_PDEATHSIG` on the spawned child would close this.
- `Mpv::spawn` does not pass `--no-config`, so a `~/.config/mpv/`
  appearing later would silently start affecting playback. empty on
  this machine, so not the active cause, but worth pinning.

the one command that would settle it: play the offending track in wim,
then `curl -sI '<the stream url wim built>'` and compare
`Content-Type`/`Content-Length` against the same request with
`&c=NavidromeUI`. different values mean the server is handing the two
clients different encodes.
