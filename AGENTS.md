# Agent notes

## Commits and CI
- Progressive commits: keep each commit small (under ~100 changed lines; check
  with `git diff --cached --stat` before committing). Split larger changes into
  several commits, each building and passing fmt and clippy on Linux.
- CI skip: for commits that change only comments, docs, or formatting, put
  `[skip ci]` in the commit subject. GitHub Actions checks the marker on the
  tip commit of a push only, so earlier commits in the push do not matter.
  Never use it on code changes.

## Disk hygiene
Applies on this laptop only (the user's SSD is small).
Check `du -sh target` before and after large builds. If it exceeds 4G, run
`cargo clean` (or `rm -rf target/debug`) when no other build is running in this
worktree. Never delete `result` (Nix GC root).
