# startup cache

why the library is on screen at the first frame, what is cached where,
and exactly how stale each of those caches can get. read this before
touching `cache.rs`'s catalog half, `Corpus::reconcile_server`, or
`subsonic.rs`'s navigation cache.

## the problem

the eager navidrome index (`getArtists` + every `getAlbumList2` page +
`getPlaylists`) was refetched from the network on every launch. nothing
blocked the first frame on it (the sync has its own thread), but the
library still was not *there*: the sidebar said "connecting to
navidrome..." and rows streamed in whenever the server answered. on a
30k-album library against an 80ms-rtt server that was 2.5s, and before
the pagination fix 8s.

## three caches

### 1. the persisted server catalog (on disk)

`~/.cache/wim/catalogs/<digest>.ndjson`, one file per server,
`<digest>` = md5 of `username@base_url`.

written by `cache::save_catalog` from whichever background thread just
fetched a catalog, read by `search::spawn_warm_from_cache` before the
first frame.

- **format**: newline-delimited json, one `{"k":"artist"|"album"|
  "playlist", ...}` row per line, with a header line carrying the
  format version, a check digest and a body digest. ndjson rather than
  one json document or toml because the file is tens of thousands of
  rows: it parses a line at a time with no whole-file value tree, one
  corrupt line costs one entry instead of the catalog, and it stays
  readable. measured at 4000 artists + 30000 albums + 120 playlists
  (`cache::tests::catalog_format_size_and_load_time`, release):
  2.4 MB, 19ms to write, 26ms to parse. the same albums as one pretty
  json document would be 3.1 MB. a compact binary encoding would be
  smaller and faster still, at the cost of a dependency and of being
  unreadable when something looks wrong; the parse is already well
  inside the mpv spawn it hides behind.
- **no credentials, nothing identifying**: the header stores a salted
  digest of `username@base_url`, not the key itself, so the file holds
  library rows and digests only. created 0600 inside a 0700 directory
  regardless, because it still maps out someone's library.
- **written atomically**: temp file then rename, so an interrupted
  write cannot leave a truncated catalog.
- **not rewritten when nothing changed**: the header's body digest is
  compared first, so the 45s auto-sync does not rewrite megabytes
  every cycle.
- **swept**: `sweep_foreign_catalogs` drops any catalog not belonging
  to a configured server, on every launch. deliberately not age-based;
  a configured server's catalog is the thing that makes a cold start
  instant, including after a week offline.

**staleness and invalidation.** the warm-loaded catalog is only ever
as current as the last session, which is why loading it is not enough
on its own: `Corpus::reconcile_server` runs on every sync result and
*drops* whatever that server no longer lists, instead of only adding
(`add_subsonic_search_result` is add-only, by design, since it also
serves live `search3` answers). worst case, an artist, album or
playlist deleted while wim was closed is listed until the first sync
of the next launch lands, which is one round trip. the file itself is
rewritten by that same sync, so it never drifts further than that.

songs are deliberately not persisted or reconciled. nothing indexes
them up front (a library's songs can be enormous; see
`subsonic::search3`), so a catalog fetch is not an authoritative song
list and pruning against it would throw away every tracklist an opened
album had taught the corpus. a song the server has deleted can
therefore stay in the corpus until the session ends; playing it fails
the same way it did before any of this.

### 2. navigation responses (in memory)

`getArtist`, `getAlbum`, `getPlaylist`, `getArtistInfo2` and
`getTopSongs` answers, keyed by request, capped at 256 entries per
server. this is what makes expanding an album and then opening it one
`getAlbum` rather than two, and re-entering an artist free.

the cache is shared per `username@base_url` through a process-global
registry (responses and digests only, never a credential), because the
sync thread builds a fresh `Client` every pass while the clients the
corpus hands the ui live for the whole session. that sharing is what
lets the sync invalidate what the ui is reading.

**staleness and invalidation.** two paths:

- the sync is the real one: `all_artists`/`all_albums`/`playlists`
  fingerprint the ids they answered with, and a fingerprint that
  changed drops every cached navigation answer for that server
  immediately.
- a 45s ttl backstops it, pinned to `AUTO_SYNC_INTERVAL`, which covers
  changes the catalog listing cannot see (a track added to an album
  that already existed). **worst case: 45 seconds.**

the catalog endpoints themselves are never cached: they are the
invalidation signal, and the only thing asking for them is the sync,
which wants fresh data. `getStarred2`, `star`/`unstar`, `search3` and
`getCoverArt` are not cached either.

### 3. local tags (on disk, pre-existing)

`library::TagIndex`, `path -> (mtime, tags)`; a warm start is a `stat`
pass with no tag parsing. invalidated per file by mtime in
nanoseconds, so there is no staleness window: a touched file is
re-read. see `library.rs`.

## bounded fan-out

every multi-request path runs concurrently through a bounded pool, not
a thread per item:

- per-album `getAlbum` for an artist: `ALBUM_FETCH_CONCURRENCY` (8),
  via `Client::album_details`. this used to be a thread (and a socket)
  per album, so a 60-release artist opened 60 connections.
- `getAlbumList2` pages: `ALBUM_PAGE_CONCURRENCY` (4) at a time after
  the first page. the cost is a few requests past the end of the
  catalog, which answer empty, and only for libraries over one page.
- local tag reads: a pool sized to `available_parallelism`.
- per-server work (sync, connectivity test, live search): one thread
  per configured server, which is a handful.

## measured

release build, isolated XDG dirs, a mock navidrome with 4000 artists /
30000 albums / 40 playlists at 80ms per request:

| | before | after, no catalog on disk | after, warm |
| --- | --- | --- | --- |
| first frame drawn | 136ms | 126ms | 82ms |
| library on screen | 8056ms | 2484ms | 82ms (at the first frame) |
| requests per launch | 65 | 65 | 65 |

first-frame variance between runs is dominated by mpv's spawn (73ms to
155ms observed); the catalog read is started next to the library scan
and finishes inside it.

the request count is unchanged by design: a warm start still does the
full catalog sync, because that is the reconciliation. of the 65, 61
are `getAlbumList2` pages for 30000 albums, and one is the `ping` that
`fetch_navidrome_catalog` does before fetching anything.

against the in-process mock at 120ms latency
(`cargo test --release -- --ignored --nocapture`):

- artist page, 15 albums: 1937ms serial, 361ms through the bounded
  pool, 0.46ms reopened from the navigation cache (0 requests).
- album catalog, 5000 albums: 1367ms over 11 serial pages, 528ms over
  13 waved ones.
