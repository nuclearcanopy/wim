# async open

opening an album, playlist or artist used to block the main thread on
the network: `get_album` inline on the enter key, and for an artist a
`getArtist` plus a `getAlbum` per album. the whole tui stopped for that
round trip, warm cache ~375ms, cold or on a slow link seconds. every
frame and every keystroke waited on it.

`browse::Loader` replaces that. `open` returns the view to show *now*
(the title and subtitle are already in the `SearchItem` that was
picked) with `loading: true`, and the fetches run on worker threads
that report back over one channel. `main`'s loop drains it with
`Loader::poll` once a pass, next to the subtitle, sync and enrichment
drains.

## what arrives when

- album: one `getAlbum`, so tracks and year land together and
  `loading` clears there. the cover is a second round trip and rides
  in behind them, so a slow cover never holds up the track list.
- playlist: one `getPlaylistSongs`, same shape, no cover.
- artist: `subsonic::stream_artist_tracks`. `getArtist` answers first
  and carries every album name, so the ALBUMS column fills after one
  round trip; then one `getAlbum` per album streams back and the SONGS
  column grows album by album. the bio (navidrome proxies it from
  last.fm, slow) rides the same channel. `loading` clears when the
  stream ends.

per-album rather than one lump at the end because the events already
exist and carry `index`, the album's slot in the chronological list.
completions race, so `Loader` keeps one bucket per album and rebuilds
`tracks` from them in album order; the list reads chronologically
however the fetches finish.

## the stale-reply guard

one generation counter, in the loader, shared with its workers.

- every `open` bumps it, so a reply from the previous open is stale the
  moment a new one starts.
- `invalidate` bumps it too, and is called wherever `browse_view` is
  closed or replaced by a view the loader did not open: esc/q, and the
  starred listing (keyboard and shellsong's `view starred`).
- `poll` drops any reply whose generation is not the current one, and
  drops everything when no view is open at all.

so a late reply can neither reopen a view the user closed nor overwrite
a newer open. the counter is the only authority; replies carry the
generation they were requested under, nothing is matched by album id
(the same album opened twice is two different opens).

workers also check the counter before each send and stop when it has
moved. for an artist that matters: dropping the loader's end of
`stream_artist_tracks`'s channel is what stops the album fetches still
queued behind the current one, so navigating away costs at most one
in-flight request per worker rather than the whole catalog.

quitting is the same mechanism from the other end: the receiver dies
with the process, sends fail, workers return.

## one way in

`main::open_browse` is the only function that opens one, so the two
enter paths (a search result through `dispatch_action`, and an album
picked out of the artist profile's ALBUMS column) and anything the
pointer grows later cannot drift apart. the mouse today selects rows
and never activates them (see docs/input-precedence.md); when it does,
it produces the same `Action::Play` the keyboard does and lands here
with no second code path, the way a click on a transport control
already reuses `activate_transport`.

## while it is loading

the view is live, not a spinner: j/k move, esc closes, enter plays from
the selected row. enter plays what has arrived so far, deliberately;
the header reads `title · loading...` for exactly as long as that is
not the whole list, and an empty pane says `loading tracks...` rather
than `(no tracks)` (`browse::pane_message`, same vocabulary as
`main::loading_message`).

a failed open leaves the view up and empty with the error on the
status line, where it used to mean no view appeared at all. the view
carrying the failed title is more informative than a toast alone.
