# TorBox integration status

TorBox support is available through the shared API client at `wim::torbox`.
Configure the token file with `torbox_token_file` or the Home Manager option
`programs.wim.torboxTokenFile`. `WIM_TORBOX_TOKEN_FILE`, when set, overrides
the configured path.

Create a token file outside the Nix store, protect it with your secret manager
(for example agenix), and expose its path to Wim:

```sh
export WIM_TORBOX_TOKEN_FILE="$HOME/.config/wim/torbox-token"
```

The file should contain only the API token. Do not put the token directly in
the environment, config, shell history, or command arguments. TorBox requires
the token in the query string for its request-download endpoint; Wim keeps
that URL out of error messages. Returned CDN URLs are temporary and must not
be persisted or logged.

Account item identities can be saved in named queues as service/item/file IDs;
temporary stream URLs are excluded. Playback-link resolution runs in the
existing background track resolver before items enter mpv's playlist.

Voyager's public metadata search API is gone: `search-api.torbox.app` no
longer resolves, and the main API exposes no metadata-search route. Wim's
TorBox search (`T`) is therefore an **account library search**: it lists your
torrent, Usenet, and web-download items through the documented
`/torrents/mylist`, `/usenet/mylist`, and `/webdl/mylist` endpoints and
filters them locally, case-insensitively, against item names and file names.
It never sends your query to a third party, and it cannot find content that
is not already in your account. A service whose listing fails is reported
("results may be incomplete") instead of hidden; if every service fails, the
search fails with the sanitized error.

Select a library result and press Enter to open its playable files; Enter
plays, `q` queues. Opening a result never adds anything to the account. For
discovering new media online, use the separate Watch workspace (`W`), which
talks to an explicitly configured Stremio-compatible addon; TorBox search
and Watch are independent.

The TorBox workspace opens with `T`. When not configured, it opens the
connection screen; otherwise `/` edits the TorBox-only search query, Enter
searches, and `b` opens the account. Query input owns the keyboard, so letters
such as `b`, `j`, and `k` are typed normally. From the account, `1`/`2`/`3`
switch services, `r` refreshes, `n` loads the next page, `s` returns to search,
and Enter inspects files. Enter on a playable file plays it; `q` queues it.
In the account list, `+` accepts a link, `p` pauses/resumes where supported,
`R` reannounces a torrent, and `d` requires a second press to confirm deletion.
Torrents support resume, reannounce, and delete (the API does not expose
pause); Usenet supports pause, resume, and delete; web downloads support
delete.

### Connection setup

Wim's connection screen accepts a **path to a token file**, checks it with a
read-only account request, and saves only that path in `config.toml`. It never
asks for or stores the token itself. Prefer a protected file outside the Nix
store (for example, provisioned with agenix). `WIM_TORBOX_TOKEN_FILE` overrides
the configured path. The setup screen is available from the TorBox workspace
with `c` and from Settings → TorBox connection. shellswim uses the same
configuration; `torbox_status()` reports whether the configured token file can
connect without printing its contents.

### Network troubleshooting

TorBox may reject or block connections routed through Mullvad VPN; this is a
reported compatibility issue, and may vary by exit server or account. A failed
connection does not by itself prove that the token is invalid. Verify the file
path and token, then retry from a network TorBox accepts. If you choose to
change VPN routing, consider the privacy tradeoff; Wim never changes VPN
settings automatically.

shellswim supports `torbox_status()`, `torbox_search(query)` (library
search, matching the `T` workspace semantics), `torbox_list(torrent|usenet|
web)`, `torbox_add(service, link)`, `torbox_play(service, item_id, file_id)`,
`torbox_queue(service, item_id, file_id)`, and `torbox_control(service,
item_id, operation)`. Calls run asynchronously in interactive mode and are
awaited in one-shot commands; account-changing commands remain explicit and
dry runs do not execute them. Use only operations supported by that service
as listed above. ASCII video capture is not supported for remote TorBox
streams; use mpv's normal video output.
