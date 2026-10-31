# album and artist identity

how an album or an artist is identified across the app, why the
server's ids live in a per-server index instead of on `SearchItem`, and
what has to change for them to move onto it. read this before touching
`coverart::Key`, `suggest`'s `album_slot`/`artist_slot`, or
`subsonic::IdIndex`.

## the problem names cause

`SearchItem::Song` carries `artist` and `album` as display strings.
Anything joining on those is joining on a name, which loses three
distinct things:

1. **two same-titled albums by one artist collapse.** a 1994 pressing
   and a 2024 reissue both named "Crest" are one slot, so one of them
   can never be suggested and their play coverage is pooled.
2. **a split credit rolls up under neither artist.** a track credited
   "Bladee & Ecco2k" opens a third artist slot that no artist row, no
   album, and no affinity rollup ever joins to.
3. **a compilation's tracks key apart from their album.** each track
   carries its own artist, so `("someone else", "now 42")` is not
   `("various artists", "now 42")`, and the one cover that album has
   was fetched once per track.

the server answers all three: `getAlbumList2`, `getArtists`,
`getAlbum`, `getArtist` and `search3` all report `id`, and songs and
albums report `albumId`/`artistId`.

## why the ids are not on `SearchItem`

they should be. the blocker is mechanical, not architectural: a new
field on an enum struct variant makes every *literal construction* of
that variant a compile error, and Rust has no defaulted fields and no
functional-record-update for enum variants, so "optional and
defaulted" cannot rescue a construction site. `Option<String>` does
not help; the field still has to be named.

several of those literals are in `src/main.rs`, which is being
decomposed into a library plus two binaries in a separate lane, so
touching it means a merge conflict in the one file nobody wants one in.
the same is true of `subsonic::{Song, Album, Artist}` and
`cache::ServerCatalog`: main.rs's own tests construct all of them.

so the ids ride in `subsonic::IdIndex` instead: three maps per server
(`song -> album`, `song -> artist`, `album -> artist`), shared between
every `Client` for that server exactly the way `NAV_CACHES` is, and
reached through the `Arc<Client>` that every remote `SearchItem`
already carries.

### what it costs

- an extra indirection and an `RwLock` read at each join site. a
  hashmap read under a read lock, tens of nanoseconds, against a
  `getCoverArt` round trip or a once-per-menu-open catalog build.
- ~200 bytes per song seen, never evicted. a session that browsed
  every song of a 40k-track library costs single-digit MB, less than
  the corpus rows for the same songs. eviction is deliberately absent:
  dropping an entry would silently move a cover's key.
- **a key can sharpen mid-session.** a song restored from a saved
  queue has no index entry until some response mentions it, so its
  cover key is the name form until then and the id form afterwards.
  the cost is one extra cover fetch and one extra file in
  `~/.cache/wim/covers`; both are disposable. every song that came
  from a response this session is registered by the same parse that
  built it, so this is the queue-restore case only.

### moving them onto `SearchItem`

four edits in main.rs, after which `IdIndex`'s song maps can go (the
`album -> artist` map is still the only home for an album row's artist
id until `SearchItem::Album` carries one too):

| site | change |
| --- | --- |
| `main.rs:179` (`artist_tracks`, `SearchItem::Album` literal) | add `artist_id: artist_album.artist_id` |
| `main.rs:190` (`artist_tracks`, `SearchItem::Song` literal) | add `album_id: s.album_id, artist_id: s.artist_id` |
| `main.rs:7343` (test, `SearchItem::Album`) | add `artist_id: None` |
| `main.rs:131` (`resolve_tracks`, `SearchItem::Artist { id, name, client }`) | add `..` if `Artist` is widened |

plus, if the subsonic row types are widened to carry the ids through
rather than the index: `main.rs:7832` (`subsonic::Song`),
`main.rs:7363`/`7885`/`7999` (`subsonic::Album`),
`main.rs:7862`/`7866`/`7995` (`subsonic::Artist`),
`main.rs:7361`/`7849` (`subsonic::SearchResult`), and
`main.rs:2984` (`cache::ServerCatalog`).

## the rule every join follows

**prefer an id; fall back to the name, but only onto a row that is not
already some other id's; never merge two different ids.**

all three clauses are load-bearing, and the middle one is what keeps
local files working. `suggest` keeps both indices
(`by_album`/`by_album_id`, `by_artist`/`by_artist_id`) and resolves id
first, name second:

- id matches an existing slot: that slot, and the name becomes an
  alias onto it (this is the split-credit fix).
- no id match, name matches a slot with **no** id: adopt it and claim
  it, so an id arriving after the names built the slot does not open a
  second one.
- no id match, name matches a slot with a **different** id: a new
  slot. two same-titled records by one artist, or two bands with one
  name, are two things. the name index keeps pointing at the first,
  since that is all an id-less row has to join on.
- no id at all: the name slot, exactly as before. every local file.

`coverart::Key` is the same preference in one value: `AlbumId` if
known, else `Album { artist, album }`, else the track's own identity.

both sides of the cover keyspace have to try the two forms in the same
order, or an album opened in the browse view stops sharing its cover
with the same album's tracks playing from the player. that is why
`browse::fetch_cover_art` prefers the id even though it always has the
names.

## what is persisted

the `album -> artist` half goes into the ndjson catalog as an optional
`artist_id` on album rows (`cache::CatalogRow`), replayed into the
index by `Corpus::warm_from_cache`, so a cold start joins on ids before
the first sync lands. it is `skip_serializing_if = "Option::is_none"`,
so a server that reports no artist ids writes the bytes it always did
and nobody's catalog is rewritten for a field that is not there.

song links are not persisted, for the same reason song *rows* are not
(see docs/startup-cache.md): nothing indexes songs up front, so there
is no authoritative list to write.
