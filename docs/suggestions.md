# suggestions

the design record for `src/suggest.rs`, the engine behind the
suggestions menu. read this before changing a weight or adding a
generator; the comments in the code point back here rather than
repeating the reasoning inline.

## the honest framing

the ask was a "smart socialmedia-like algorithm" that learns taste from
genre and artist. the mechanism behind every such feed is three stages,
and that is exactly what this is:

1. **candidate generation**: cheap, independent generators each propose
   rows for one specific reason.
2. **scoring**: every candidate gets a number, comparable across
   generators.
3. **diversification**: the top-scoring rows are re-ordered and capped
   so the list is varied rather than ten rows of the same artist.

what it is *not*: collaborative filtering. "people who liked x also
liked y" needs other people's listening data, and there is none here;
this is one person's library and one person's play history. pretending
otherwise would mean inventing a signal. what the app genuinely has is

- the user's own play history (`src/cache.rs`: play count, distinct
  play days, last-played timestamp), and
- the structure of the library (artists, albums, tracks, genre tags).

every example in the original request ("i listen to an artist a lot and
omit one album", "omit one song from an album", "the one metal artist i
listen to least", "a song i play every day") is the same shape: a
**coverage gap weighted by affinity**. that is the model, and for a
personal library it is the right one.

the one cross-artist signal available is `similarArtist` from
`getArtistInfo2`, which navidrome proxies from last.fm. it was already
being fetched for artist biographies and thrown away; it is now parsed
and used (`subsonic::ArtistInfo`). that is somebody else's
collaborative data, arriving for free.

## affinity

one number per track, rolled up to album, artist, and genre. two
components.

**frequency.** distinct calendar days, not raw play count:

```
frequency = 1.0 * distinct_days + 0.25 * ln(1 + play_count)
```

ten plays in one evening is one day of interest; coming back to a track
on another day is what actually says you like it. this is the same
reasoning the auto-download threshold already uses (`cache.rs`'s
`play_days`), so the two features agree about what "likes it" means.
raw play count still enters, logarithmically, as a tiebreak between
tracks with equal day counts. the test `distinct_days_beat_raw_play_count`
pins the ordering: 3 days / 3 plays beats 1 day / 10 plays.

**recency.** exponential decay on `last_played_secs`, half-life **30
days**:

```
score = frequency * 0.5 ^ (age_days / 30)
```

30 days means last month's listening counts half, and last year's
counts about 1/4000th. the menu therefore tracks what the user is into
*now* without ever hard-forgetting anything; a year-old obsession
drops out of the affinity ranking but is precisely what the
rediscovery generator then picks up, because that generator reads the
*undecayed* frequency instead.

an explicit star adds a flat `1.0` to frequency, and a never-played
track decays by nothing (there is no evidence of staleness to apply).

**rollups are sums, not means.** "how much do you listen to this
artist" is a total. this does bias toward artists with large
catalogues; the alternative biases toward artists with one ep, which
is worse, and the per-artist cap in stage 3 limits the damage either
way.

**genre comes only from the server's tags.** `getAlbum`/`search3`
responses carry `genre` plus a multi-valued `genres` array, both now
parsed (`subsonic::parse_genres`); an untagged library gets no
genre-based suggestions at all, and a badly tagged one gets bad ones.
that is a real limitation with no local fix.

## the generators

each emits candidates tagged with a machine-readable `Reason`, plus a
reason string built from the real numbers that made it fire. the reason
string is not decoration: an explained row reads as smart, an
unexplained one reads as random, and the generator already knows why it
fired, so it is free.

| reason | precondition | scores on |
| --- | --- | --- |
| `DailyRotation` | >= 4 distinct days, and distinct days / days since first play >= 0.5 | day rate times day count |
| `AlbumGap` | artist affinity >= 1.0, album coverage <= 0.34, and the album exists as a row we can open | artist affinity times the gap |
| `TrackGap` | album coverage >= 0.5 but below 1.0 | album affinity times coverage |
| `AlbumNeighbour` | unplayed track adjacent on the tracklist to a played one (needs track order, so a full album fetch) | the neighbour's affinity |
| `GenreUnderExplored` | genre affinity >= 2.0 and at least two owned artists in it | genre affinity times how far the least-played artist sits below the most-played |
| `Rediscovery` | undecayed frequency >= 2.0 and last played >= 60 days ago | frequency times staleness |
| `StarredUnplayed` | starred, zero plays | the artist's affinity |
| `SimilarArtist` | in `similarArtist` for a top-affinity artist, owned, never played | seed affinity, discounted by rank |
| `PopularUnplayed` | in `getTopSongs` for a top-affinity artist, owned, never played | artist affinity, discounted by rank |

`AlbumGap` and `TrackGap` split the coverage axis at 0.5 on purpose:
below that you do not know the album well enough for one specific track
to be a glaring hole, so the whole album is the suggestion, and
`AlbumNeighbour` covers that half at track level instead. without the
split the two generators propose the same rows and one of them always
loses the dedup pass for no reason.

`PopularUnplayed` is what earns `getTopSongs` its place: coverage alone
cannot tell which of the forty tracks you have never played is the one
everybody else considers essential.

### daily rotation is not discovery

"maybe you see i listen to a song every day, suggest me that for quick
access" is a different request from the rest, and it is handled
differently: the rate is measured over days since the track's *first*
recorded play through to today, so a track played on 14 straight days
ending six months ago scores 14/180 and drops out, while one played on
10 of the last 10 days scores 1.0. the window is bounded in practice by
`cache::MAX_TRACKED_DAYS` (64 days of history kept per track).

## scoring

scores are normalised **within each generator** before the pools merge:

```
score = generator_weight * raw / max_raw_in_this_generator
```

without this, whichever generator happens to produce the largest raw
numbers wins the menu for reasons that have nothing to do with how good
its suggestions are. after normalisation every generator's best
candidate scores exactly its weight, so the weights alone decide the
relative standing of the generators. they live in
`suggest::GeneratorWeights` and nowhere else:

| weight | value | why |
| --- | --- | --- |
| `daily_rotation` | 1.00 | quick access to what is already in rotation is the most reliably useful row on the screen |
| `album_gap` | 0.95 | the headline request |
| `track_gap` | 0.90 | same idea, finer grained |
| `starred_unplayed` | 0.85 | explicit stated interest, so a near-certain hit |
| `rediscovery` | 0.80 | strong, but the user may have moved on deliberately |
| `similar_artist` | 0.75 | external signal, weaker than the user's own behaviour |
| `genre_under_explored` | 0.70 | only as good as the server's genre tags |
| `popular_unplayed` | 0.65 | other people's taste, not this user's |
| `album_neighbour` | 0.60 | adjacency on a tracklist is a real but faint reason |

a pool where every raw score is zero (for instance starred tracks by
artists with no history at all) still gets placed, at half weight, so
an explicit signal is never silently dropped.

the same track can be both a track gap and an album neighbour; after
the merge, duplicates are collapsed to their highest-scoring reason, so
no row ever appears in two sections.

## diversification

non-negotiable: a list of ten rows from one artist is a failure even if
every row scores well. greedy selection with hard caps and mmr-style
penalties:

- hard caps, **across the whole menu** rather than per section: 3 rows
  per artist, 2 per album, 6 per section, 36 rows total.
- multiplicative penalty per already-selected row sharing an
  attribute: artist `0.45`, album `0.3`, genre `0.8` (genre capped at
  four repeats, since a single-genre library would otherwise penalise
  itself into nothing).

the caps are the guarantee and the penalties are the shaping: the
penalty is what makes the *second* row by an artist you love rank below
a first row by a different one, instead of only kicking in at the cap.
`one_artist_cannot_fill_the_menu` builds a thirty-track one-artist
library where every row is a legitimate gap and asserts the menu comes
back with at most three rows.

`Suggestion::score` keeps the pre-penalty value, so the number stays
comparable across sections; diversification reorders rows, it does not
rewrite their scores.

## output shape

grouped sections, not one flat list, because reason-labelled groups are
how every app that does this well presents it:

```rust
Suggestions { sections: Vec<Section> }
Section    { title: String, reason: Reason, items: Vec<Suggestion> }
Suggestion { item: SearchItem, reason_text: String, score: f32 }
```

sections arrive strongest-first and already capped, so the menu renders
without recomputing anything. `item` is a `SearchItem`, so the existing
enter-key handling plays or opens a row with no new code path.

## cold start

a new user has no history, and every scored generator would be
guessing, so with zero total plays the engine takes a different path
entirely: starred items first (real stated interest), then a
**stratified sample** across artists, one row per artist, round-robin
over a seeded artist order. picking the globally first n rows would
show three albums by whichever artist sorts first, every single time.

the shuffle is seeded (splitmix64 plus fisher-yates, hand-rolled so the
sequence is pinned by the seed alone and not by a crate version), and
the host passes the seed in. `Request::new` defaults it to the current
day number, which means the cold-start screen changes once a day and is
stable within a day.

the same path is the empty-screen guard: if a library has history but
no gaps worth showing, the menu falls back to it rather than rendering
nothing.

## determinism

`now_secs` and `seed` are parameters. nothing in the engine reads a
clock or a thread rng, hash iteration order is never allowed to leak
into an ordering (every sort has a total tiebreak on a stable key), and
the single impure input (the network) is injected through the
`Enrichment` trait. the whole engine is therefore unit-testable against
fixed timestamps, which is the only way the decay and rate maths can be
asserted at all; this codebase has already been bitten by untestable
clock reads (see `cache::record_play`'s `today` parameter).

play-history fixtures in the tests are built by deserialising an
`index.toml`, because `CacheIndex::record_play` stamps the wall clock
and so cannot produce backdated history.

## limitations

- **no collaborative signal.** covered above. `similarArtist` and
  `getTopSongs` are the only outside opinions available, both
  last.fm-proxied by the server, both optional.
- **genre is only as good as the server's tags.** untagged library, no
  genre suggestions.
- **albums and artists are joined by name, not id.** `SearchItem`
  carries `artist` and `album` as display strings and no ids, so the
  catalog joins on a trimmed, lowercased name. two distinct albums with
  the same name by the same artist collapse into one, and a track
  credited "Bladee & Ecco2k" does not roll up under either. fixing this
  properly means widening `SearchItem::Song` with `album_id`/`artist_id`,
  which is a change to every construction site.
- **album coverage needs a full album fetch to be exact.** without one,
  coverage is measured against the tracks that happen to be indexed,
  which is why `AlbumGap` (which tolerates knowing nothing about an
  album's tracks) is separate from `TrackGap` (which does not).
- **local files have no history at all.** `cache.rs` deliberately never
  indexes them, so local music only ever appears as cold-start
  material.
- **the genre rollup uses a track's first tag for diversification.** a
  multi-genre track counts toward every genre's affinity but only
  penalises repeats on its first tag, which keeps the penalty cheap.

## host wiring

the engine is pure; the app has to feed it.

```rust
let mut catalog = suggest::Catalog::from_corpus(&app.corpus);
catalog.ingest_album(album_name, &songs, &client);   // optional, richer
let out = suggest::suggest(&suggest::Request {
    catalog: &catalog,
    cache: &cache_index,
    starred: &starred_keys,     // remote song ids + local favorite paths
    now_secs,
    seed,
    enrichment: &app.enrichment,
    tuning: &suggest::Tuning::default(),
});
```

`Catalog::from_corpus` is enough to open the menu with no network
traffic at all. three things make it better, all of them background
fetches the host owns:

- `getAlbum` per album, fed to `Catalog::ingest_album`: real album
  coverage, genre, and track order (so `TrackGap` and `AlbumNeighbour`
  can fire at all). the artist view already fetches exactly this
  (`Client::stream_artist_tracks`), so the natural wiring is to keep
  what it fetches instead of dropping it.
- `getArtistInfo2` per top artist, fed to
  `EnrichmentCache::add_artist_info`: enables `SimilarArtist`.
- `getTopSongs` per top artist, fed to
  `EnrichmentCache::add_top_songs`: enables `PopularUnplayed`.

`suggest::enrichment_targets` returns which artists are worth fetching,
top-affinity first, and `suggest::album_targets` the albums, so the host
does not have to guess; `EnrichmentCache` remembers what has already
been fetched (`needs_similar`, `needs_top_songs`), including the
negative answer. nothing blocks: whatever has not arrived yet simply
means those two generators do not fire this time.

### what the host actually does

`main.rs`'s `spawn_background_enrichment` is the fetcher, shaped like
`spawn_background_sync` and `spawn_background_search`: jobs out over one
channel, answers back over another, no network call anywhere near the
render or input path. the main thread picks the targets (it owns the
catalog and the play history) and folds each answer back in, then
re-runs the engine **once per batch of arrivals**, never per frame.

the work is bounded by construction rather than by a timer:

- one job is in flight at a time, and the next is only picked once the
  last reported `JobDone`, so one keypress costs one job.
- a job is at most `ENRICH_ARTIST_TARGETS` (8) artists, which is
  `similar_seed_artists` plus headroom for the ranking shifting, times
  at most two calls each, and `ENRICH_ALBUMS_PER_JOB` (12) albums.
- `album_targets` only offers albums the user has actually played
  something from and whose tracklist is still unknown. that is the real
  bound on a large library: the rest is `AlbumGap`'s job and needs no
  fetch at all.
- warming happens only while the menu is open. these are network calls
  for one feature; nobody who never presses `R` should pay for them.

`EnrichmentCache` persists to `~/.cache/wim/enrichment.toml` (cache dir,
not state: reconstructable server output, same reasoning as `cache.rs`),
so the two artist-level generators are warm from the first open of the
next session. the album tracklists are deliberately *not* persisted;
they are bulky, they are re-derivable in a bounded number of calls, and
the host instead keeps one long-lived `Catalog` across menu opens so a
session never re-fetches what it already has. an album is asked about at
most once per session either way, which is what stops a failed fetch (or
a compilation whose songs are credited to a different artist than the
album is) from being retried in a loop.

while a job is in flight the menu titles itself "N so far" rather than
"N" (`ui::suggestions::SuggestionsView::enriching`), because a thin list
that looks final is worse than a thin list that says it is still
filling.
