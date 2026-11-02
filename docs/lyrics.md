# lyrics

Lyrics are fetched off the input/render path. The display prefers
timestamped LRC and follows mpv's observed playback position, including
seeks and pause/resume. Scrolling manually pauses following for three
seconds, then following resumes. Plain text remains available when no
synchronized result exists.

## providers

`lyrics.providers` is an ordered list. The defaults are:

1. `local`: same-basename `.lrc` or `.txt` sidecars, then embedded
   `LYRICS` tags. LRC timestamps are preserved.
2. `opensubsonic`: for Navidrome/OpenSubsonic remote tracks, ask the
   configured server's `getLyricsBySongId` extension for its cached,
   embedded or sidecar lyrics.
3. `lrclib`: public, no-key API; exact metadata lookup when possible,
   then a ranked title/artist search.
4. `musixmatch`: optional API key; requires an account/API plan. Its API
   has licensing, country restrictions, a required view-tracking pixel,
   and mandatory visible attribution. WIM sends the view event when the
   lyrics are shown, displays the returned copyright notice, and does not
   persist Musixmatch lyrics in its cache. Review Musixmatch's current
   terms and restrictions before enabling it.
5. `lyrics-ovh`: public no-key plain-text fallback.

For Navidrome/OpenSubsonic songs, WIM asks the server when
`opensubsonic` is enabled. That can provide synchronized, embedded, or
sidecar lyrics maintained by the server; WIM then falls back to the
other configured providers if no synced server result is available.
Servers without the extension are handled as an ordinary provider miss.

Use a TOML list to change providers, for example:

```toml
[lyrics]
providers = ["local", "lrclib", "lyrics-ovh"]

[lyrics.api_key_files]
musixmatch = "/run/agenix/musixmatch-api-key"
```

API credentials are read from files, never embedded in the config value.
The Home Manager interface is `programs.wim.lyrics.providers` and
`programs.wim.lyrics.apiKeyFiles`, so secret files from agenix/sops-nix
can be referenced without putting key contents in the Nix store.

## cache

Successful non-Musixmatch results are cached as small JSON records in
`$XDG_CACHE_HOME/wim/lyrics` (or the platform cache directory). Cache
entries are keyed by normalized track metadata, local-file modification
time, provider order, and API-key identity; they persist until those
change or the cache directory is cleared. The key itself is never stored
in the cache name or contents. Not-found and failed lookups are not
cached, so a temporary outage or newly-added lyrics can be retried.

The cache is deliberately separate from the downloaded-track cache and
does not participate in media eviction.
