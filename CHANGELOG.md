# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-04-25

### Major

- **Bundle-aware FUSE filesystem**. v0.2.x compressed `.jsonl` files but
  ignored the sibling `<uuid>/` directories that Claude Code creates for
  sub-agents and tool-results, hiding them behind the mount and breaking
  resume of those sessions. v0.3 treats the whole bundle (jsonl + sibling
  dir) as the unit:
  - The store mirrors Claude's layout exactly, except `.jsonl` files are
    stored as `.jsonl.zst`. Sibling dirs and the files inside them
    (sub-agents, tool-results, .meta.json, .txt, …) are kept raw.
  - The FUSE has two paths: virtual `.jsonl` (lazy decompress on open;
    re-compress + verify SHA-256 + atomic rename on release) and
    pass-through for everything else.
  - `migrate-store` moves bundles atomically: compresses raw `.jsonl`,
    renames already-compressed ones, and **moves the sibling `<uuid>/`
    dir along with each session**. Verified by hashing every original
    file and comparing it round-trip through the mount.
- **`install` is auto-migrating and store-aware**. On first run it
  detects what is in `~/.claude/projects/`:
  - empty → default store
  - one symlink (NFS-shared layout) → use the target as store, migrate
    in place under a project subdir named after the symlink, remove the
    symlink, write systemd unit with `Environment=CLAUDE_CELLAR_STORE_DIR=<target>`
  - real subdirs with sessions → default store, migrate everything in
- **Linux only**. The crate refuses to compile on non-Linux with a clear
  diagnostic (`claude-cellar requires Linux …`).

### Removed

- `run` command (the v0.1 wrapper), and the `~/.local/bin/claude` shim
  it installed. Both replaced by the FUSE mount.
- All Windows / macOS conditional code, the `ctrlc` Windows-only
  dependency, and the `[target.cfg(windows)]` block.

### Verified

End-to-end smoke against a real backup (54 sessions, 92 files including
sub-agent .jsonl files, tool-result .txt files, .meta.json sidecars):
SHA-256 of every original byte preserved through migrate + mount + read.
Multi-claude in parallel writes to distinct sessions without corruption.

[0.3.0]: https://github.com/OnCeUponTry/claude-cellar/releases/tag/v0.3.0

## [0.2.1] - 2026-04-25 — YANKED

Auto-migrate on install, smart store location, raw `.jsonl` accepted by
`migrate-store`. **Yanked: the FUSE did not preserve sibling `<uuid>/`
bundle dirs, breaking resume of sessions with sub-agents/tool-results.**
Use v0.3.0.

## [0.2.0] - 2026-04-25 — YANKED

First FUSE-backed release. **Yanked for the same reason as 0.2.1.**
Use v0.3.0.

## [0.1.2] - 2026-04-25

### Added

- Environment variable `CLAUDE_CELLAR_PROJECTS_DIR` to override the default
  projects root (`~/.claude/projects`).
- `--projects-dir <path>` flag on `run` for per-invocation override (takes
  precedence over the env var).
- README section "Compatibility & edge cases" documenting how Claude Code
  settings (`CLAUDE_CODE_SKIP_PROMPT_HISTORY`, `cleanupPeriodDays`,
  `--no-session-persistence`) interact with cellar.

[0.1.2]: https://github.com/OnCeUponTry/claude-cellar/releases/tag/v0.1.2

## [0.1.1] - 2026-04-25

### Fixed

- Windows: register a Ctrl+C handler so the wrapper survives Ctrl+C and runs cleanup,
  achieving parity with the SIGINT/SIGTERM/SIGHUP forwarding already implemented on Unix.
  Without this fix, hitting Ctrl+C in Windows would leave hydrated .jsonl files orphaned.
- Cleanup of an unused-variable warning when cross-compiling to Windows.
- Reorganized the post-wait exit-code resolution to keep clippy lints green across all targets.

### Internal

- Added `ctrlc` as a Windows-only dependency.

[0.1.1]: https://github.com/OnCeUponTry/claude-cellar/releases/tag/v0.1.1

## [0.1.0] - 2026-04-25

First public release.

### Added

- `archive` — keep N most recent `.jsonl` sessions uncompressed, compress the
  rest in parallel (rayon) with zstd level 19. Verifies round-trip hash before
  deleting originals. Supports `--dry-run` and `--keep <N>`.
- `run` — transparent wrapper that hydrates all `.jsonl.zst` to `.jsonl` in
  parallel, spawns the real Claude binary with forwarded arguments, waits for
  it to exit, then either re-compresses modified sessions or deletes unchanged
  hydrated files. Survives SIGINT/SIGTERM/SIGHUP to guarantee cleanup.
- `install` / `uninstall` — manages the `~/.local/bin/claude` shim (Unix) or
  `%USERPROFILE%\bin\claude.cmd` (Windows). Auto-detects the real Claude binary
  from canonical locations or via the `CLAUDE_CELLAR_CLAUDE_BIN` env var, and
  persists the path in the user config directory.
- `resume <id>` — decompresses a single session to a tmpfs scratch directory
  and launches `claude --resume` against it.
- `compress` / `decompress` / `list` — single-file operations.
- `log` — persistent log of every archive, hydrate, run, and resume, with
  timings and counts.
- Round-trip hash verification on every compression (SHA-256), with atomic
  delete-original only after the compressed file hashes back to the same
  content.
- Unix file mode preservation (0600 sessions stay 0600 once compressed).
- Recursive scan of `~/.claude/projects/` across all sub-project directories.

[0.1.0]: https://github.com/OnCeUponTry/claude-cellar/releases/tag/v0.1.0
