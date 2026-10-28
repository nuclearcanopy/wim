# open questions

things the overnight build loop defaulted on rather than blocking for.
review and correct whenever; defaults are logged in the relevant commit
too.

## opensubtitles api key

`src/opensubtitles.rs` is built and wired in (search + download +
sub-add, behind `config.opensubtitles_api_key`, off by default), but
the actual REST endpoint paths/response shape (`api.opensubtitles.com/
api/v1/subtitles`, `/download`) are a best-effort guess from general
knowledge, not verified against live docs or a real account; there's
no network access in this environment to check, and no key to
actually call the api with either way. needs a real opensubtitles.com
api key from the maintainer to live-test; until then, treat the
request/response shapes in opensubtitles.rs as unverified.

## navidrome password storage

asked whether the password could be stored more securely. it cannot be
hashed away: subsonic auth is `md5(password + salt)` with a fresh salt
per request, so the client needs the cleartext for every call. offered
agenix + `password_file` (already supported, and the only option the
home-manager module exposes), opensubsonic `apiKeyAuthentication`
(spec verified; navidrome support is an unmerged PR, #6219, so there is
no server to talk to yet), or app-side hardening. maintainer chose
app-side hardening only: 0600, zeroize on drop, warn on persisting a
plaintext password. declined rotating the exposed credential, having
been told it was echoed into transcripts twice.

## download trigger policy

`cache_listens_threshold` counted plays, so playing a song twice in one
sitting triggered a download. maintainer wants "played on two distinct
days" instead. being implemented on `fix/nowplaying-queue`, including
renaming the config field and the nix option, since a field whose name
lies is worse than a rename.

## shellsong language design

the syntax went through three revisions with the maintainer in one
sitting (producer argument dropped, then barewords dropped entirely).
current state: call-only grammar, `[]` is always a list of plain
unquoted strings, single values interchangeable with one-element lists,
typed positional parameters per function. deliberate asymmetry worth
not "fixing" later: `play(...)` accepts songs or albums, `queue(...)`
is song-preferring, `queue_aa(...)` widens to albums. maintainer has
asked for substantially more design work, polish, and brainstorming
before this is considered done; it is explicitly not finished.

## suggestions menu keybind

`R` for "recommendations". `s` and `S` were already shuffle and
starred songs, and `R` keeps the shift-letter convention the other
view-openers use (`S` starred, `L` lyrics, `I` index, `Q` queue,
`K` keybinds). Remappable like any other action if that reads wrong.

## suggestions menu scope

shipped against local play history only; the three network-backed
generators stay quiet until their fetches are wired (see ROADMAP).
That was deliberate rather than blocking the whole menu on network
plumbing: the history-driven sections are the ones that work with no
server at all, and the view already carries an `enriching` flag so a
thin list says "so far" instead of presenting itself as complete.

## mpv --no-config

Pinned unconditionally rather than made configurable. wim drives mpv as
a backend and sets its own af chain, replaygain, volume and vo; a user
`~/.config/mpv/mpv.conf` layering on top of that produces symptoms
indistinguishable from a bug in wim, which is exactly what happened
once already. If someone genuinely wants their mpv.conf honoured, that
wants an explicit opt-out config field rather than the current silent
inheritance.
