# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
