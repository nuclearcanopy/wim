# open questions

Decisions the build loop took rather than blocking for, plus the few
things that genuinely need the maintainer.

**Status is the point of this file.** It previously listed a video
question as "BLOCKING the video items" and asked for a choice between
three subsystems, when later work had already made the whole premise
moot. A stale entry here reads as a debt you owe, which is worse than
no entry at all. So this is split by status, and anything answered says
what answered it.

---

## needs you

### opensubtitles api key

`src/opensubtitles.rs` is built and wired in (search, download,
sub-add, behind `config.opensubtitles_api_key`, off by default), but
the REST endpoint paths and response shapes are a best-effort guess,
not verified against live docs or a real account: there is no key here
and no network access to check with. Treat the request/response shapes
as **unverified** until someone runs it against a real key.

### decoded cover resolution ceiling

`MAX_DECODED_EDGE = 640`. Covers the quadrant grid with margin (a
400x200 cell pane samples 400x400) and the `ratatui-image` path at a
~1920px-wide terminal. On a 4K display with a very large art panel the
`full-color` path may look slightly soft. 1024 would cost 71MB for 24
cached covers instead of 28MB. Pure taste against memory.

### parallel scoring floor

`PARALLEL_SCORING_FLOOR` = 16_000 items before search fans out across
threads, up from 2_000. Measured on 8 cores under load ~10, where
sequential won at 4k for every query shape and at 8k for all but one;
on an idle machine threads would likely win earlier. Chosen to avoid
paying 0.4ms of spawn and join on small libraries, where the whole
sequential search is ~2ms worst case. `bench_parallel_floor` retunes
it.

---

## defaults taken, recorded so they are not re-litigated

### navidrome password storage

It cannot be hashed away: subsonic auth is `md5(password + salt)` with
a fresh salt per request, so the client needs the cleartext for every
call. Options offered were agenix plus `password_file` (supported, and
the only one the home-manager module exposes), opensubsonic
`apiKeyAuthentication` (spec verified, but navidrome support is an
unmerged PR so there is no server to talk to), or app-side hardening.
**Maintainer chose app-side hardening only**: 0600, zeroize on drop,
and a warning when a plaintext password is persisted. Rotation of the
credential that was echoed into transcripts was **declined**.

### mpv `--no-config`

Pinned unconditionally rather than made configurable. wim drives mpv as
a backend and sets its own af chain, replaygain, volume and vo; a user
`mpv.conf` layering on top produces symptoms indistinguishable from a
bug in wim, which happened once already. Honouring a user's mpv.conf
would want an explicit opt-out field, not silent inheritance.

Related, found later by measurement: `--no-terminal` **breaks vo
auto-sizing** (mpv never queries the tty and falls back to 80x25), so
wim pushes the video geometry itself. See `docs/video.md`.

### suggestions menu keybind and scope

`R` for recommendations: `s` and `S` were already shuffle and starred,
and `R` keeps the shift-letter convention the other view-openers use.
Remappable like anything else.

Shipped against local play history only; the network-backed generators
stay quiet until their fetches are wired. Deliberate rather than
blocking the whole menu on network plumbing, and the view carries an
`enriching` flag so a thin list says "so far" rather than presenting
itself as complete.

### mouse: the text-entry tiers do not swallow clicks

Eight of the ten input tiers swallow a click aimed elsewhere; the two
text-entry ones (`editing_query`, `bv.filtering`) deliberately do not.
A keystroke has no location, so a text box must claim every key or lose
some; a click names its target and cannot leak that way. So the swallow
set is exactly the keyboard tiers that paint over the panes. A click
elsewhere takes the keyboard out of the box; a click *into* a box
claims it, since otherwise clicking a visible search box would read as
a click on the pane around it and close it.

**Confirmed by the later mouse audit**, which checked specifically
whether this caused the maintainer's reported symptoms. It did not.

### album art `Fit::Contain`

A cover that is not square letterboxes rather than stretching. For a
square cover it is a no-op, since the layouts already size the rect
square against the same `cell_aspect`. `Fit::Stretch` restores the old
behaviour, one argument away.

**Corrected:** this entry used to say Contain also made "the video pane
aspect-correct instead of stretched". That half is obsolete. wim does
not rasterize video at all any more; mpv draws it with its own vo and
handles its own aspect.

---

## answered by later work, kept only so the reasoning is not lost

### where video frames come from: obsolete, nothing to decide

This asked the maintainer to choose between `screenshot-to-file` plus
read-back (2-5fps), shipping an mpv lua script, or `--vo=image`,
because `screenshot-raw` is unusable over json ipc. It was marked
BLOCKING.

**The premise is gone: wim does not extract frames.** mpv has terminal
video outputs of its own, `tct` (true-color text, half-blocks, works in
any terminal including alacritty) and `kitty` (graphics protocol), and
`video::wanted` just picks one. No frame ever crosses the ipc socket.
The `FramePump` built for the old design is dead code that reports
`Unsupported`; deleting it is a ROADMAP item. See `docs/video.md`.

### frame downscale ceiling: obsolete

`VIDEO_FRAME_MAX_EDGE` existed because `ui::albumart::sample_luma`
box-averaged every source pixel per cell, making a 1080p frame ~2.07M
`get_pixel` calls per frame on the render thread. `sample_luma` is
gone, replaced by the O(output) quadrant rasterizer, and nothing feeds
frames through wim any more regardless.

### download trigger policy: shipped

`cache_listens_threshold` counted plays, so playing a song twice in one
sitting triggered a download. It is "played on two distinct days" now,
with the config field and the nix option renamed to match, since a
field whose name lies is worse than a rename.

### shellsong language design: superseded, deliberately

The syntax went through three revisions in one sitting (producer
argument dropped, then barewords dropped entirely), landing on a
call-only grammar with typed positional parameters, `[]` always a list
of plain unquoted strings, and single values interchangeable with
one-element lists. One deliberate asymmetry worth not "fixing":
`play(...)` accepts songs or albums, `queue(...)` is song-preferring,
`queue_aa(...)` widens to albums.

The entry used to end "the maintainer has asked for substantially more
design work; it is explicitly not finished". That work is now scoped
and in progress: variables, `.shg` scripts, per-identity eq and volume,
and the queue timeline (pause as an entry, a default gap between
tracks, clips). **A `when` conditional engine was proposed and
explicitly rejected by the maintainer** ("writing conditionals is just
not particularly it"), which was the right call: a rules engine is a
programmer's idea of a feature and the timeline is something a listener
uses. See ROADMAP's shellsong sections and `docs/shellsong.md`.
