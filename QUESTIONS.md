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
