# Agent notes

## Disk hygiene
Check `du -sh target` before and after large builds. If it exceeds 4G, run
`cargo clean` (or `rm -rf target/debug`) when no other build is running in this
worktree. Never delete `result` (Nix GC root).
