# Watch media model and provider compatibility

Wim's shared online-media types live in `wim::media`. Catalog metadata
(`MediaItem`, `Season`, `Episode`) is distinct from `Source`,
`SubtitleCandidate`, and `PlaybackIdentity`; an episode returned by a catalog is
not evidence that a playable source exists. Stable playback identities must
not contain temporary signed URLs.

`wim::stremio` provides protocol data shapes and normalization for Stremio-style
catalog previews, metadata, series videos, streams, subtitles, and resource
capability checks. `AddonClient` can fetch manifests and supported resources
from an explicitly supplied HTTPS provider URL. It has a 12-second timeout,
2 MiB response cap, no redirects, and pins DNS answers after rejecting
loopback/private/special-use addresses. Provider URLs with embedded user info,
queries, or fragments are rejected and debug/error text does not include them.
Each configured client serializes its own requests, bounding per-add-on
concurrency to one. In Wim, press `W` to open Watch; the app uses the explicitly
configured `WIM_STREMIO_ADDON_URL` only after that action and makes no provider
request when no URL is configured. The provider receives the search query and
media IDs and can see the user's IP address. No add-on or Cinemeta endpoint is
silently enabled.

Direct `http` and `https` stream shapes are parsed and shown as candidates only
when their authority is valid and not an obvious local/private literal. Press
Enter once on a source to check its DNS answer set; mixed public/private answers
and special-use addresses are rejected. Enter again to play. mpv's HTTP proxy
option is pointed at Wim's loopback-only media relay. The relay resolves each
destination, rejects restricted addresses, and connects directly to one of the
validated socket addresses, so a second DNS lookup cannot rebind the selected
host. Redirects are checked as new proxy connections. Only the exact configured
Navidrome origins are allowed to resolve to private LAN addresses; other mpv
HTTP(S) destinations must resolve wholly public. Torrent `infoHash` sources can
be explicitly submitted to a configured TorBox account after `a` then `y`;
Wim does not play them directly or add them without confirmation. YouTube IDs
remain unsupported. Signed URLs are held in memory only, omitted
from debug output, and excluded from both saved queues and path-based resume
state. Stable source IDs hash provider/media/episode/descriptor metadata, never
the temporary URL.

Episode presentation is spoiler-safe by default through
`Episode::display_label`, `display_overview`, and `display_thumbnail` with the
default `SpoilerPolicy`. Callers must pass an explicit reveal policy before
showing episode titles, descriptions, or thumbnails. Unknown numbering remains
unknown rather than being inferred. `programs.wim.watchRevealSpoilers` (or
`watch_reveal_spoilers` in `config.toml`) persists the default; it is off by
default. In Watch, `r` is a temporary reveal and `R` changes the saved default.

The current UI supports catalog search, metadata and episode browsing, source
discovery, TorBox torrent handoff, DNS preflight, and direct HTTP(S) playback
through the relay. Artwork,
automatic subtitles, persistent watch progress/resume, and shellswim commands
are not integrated yet. Provider configuration remains opt-in and no add-on is
installed by default; compatibility is limited to declared resources and
response shapes Wim supports.
