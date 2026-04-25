# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-04-25

### Breaking

- `run` is deprecated and refuses to start. The transparent-wrapper model
  was unsafe with multiple concurrent `claude` processes (cleanup on exit
  could touch files still in use by other claudes). Replaced by a FUSE
  filesystem mounted on `~/.claude/projects/`.
- `install` no longer creates a `~/.local/bin/claude` shim. Instead it
  registers and starts a `claude-cellar.service` systemd user unit. After
  install, run `claude` natively; the kernel routes its filesystem
  operations to the FUSE daemon.
- v0.2 is **Linux-only**. macOS and Windows users should stay on v0.1.x
  until macFUSE/WinFsp adapters are added.

### Architecture

- New FUSE filesystem (`fuser` 0.15, no `libfuse` dev dep at compile time;
  uses `fusermount3` at runtime). Single daemon per user, mounted at
  `~/.claude/projects/`, backed by a separate **store** directory at
  `~/.local/share/claude-cellar/store/` (or `$CLAUDE_CELLAR_STORE_DIR`).
  Decompression is lazy on `open()` into per-FD tmpfs buffers under
  `$XDG_RUNTIME_DIR/claude-cellar/scratch/`. Recompression is atomic on
  `release()`: write `.zst.tmp`, verify SHA-256, rename over `.zst`,
  update `.meta` sidecar.
- Multi-claude correctness is guaranteed by the kernel's per-FD isolation
  rather than by any process-level coordination.
- Disk usage is now bounded to **store size + (active FDs × decompressed
  size)** instead of the whole tree being decompressed during a session.
- `.jsonl.meta` sidecars cache `(decompressed_size, mtime, sha256)` so
  `getattr` doesn't decompress; falls back to the zstd frame's
  content-size header if the sidecar is missing.

### Added

- `mount [--foreground] [--store-dir <P>] [--mount-dir <P>]` — start the
  FUSE daemon. Default is fork-and-exit; `--foreground` is for systemd
  `Type=simple`.
- `umount` — invoke `fusermount3 -u` on the mount.
- `status` — print mount state, daemon pid, store size and session count.
- `migrate-store [--from <D>] [--dry-run]` — move existing v0.1 layout
  (`.jsonl.zst` under `~/.claude/projects/<sanitized-cwd>/`) into the new
  store.
- `systemd/claude-cellar.service` template, installed by `install` to
  `~/.config/systemd/user/`.
- `archive` and `compress` now refuse if the target directory is the
  active FUSE mount, with a clear error pointing to `umount`.
- Cap of 16 simultaneous open FDs per daemon (`CLAUDE_CELLAR_MAX_FDS` to
  override). Beyond that, `open()` returns `EMFILE` as a defensive limit.
- `docs/design-fuse.md` — full architecture document.
- `tests/smoke.sh` — bash reproduction of the smoke scenarios.
- `tests/fuse_basic.rs` — Rust integration test (run with
  `cargo test --release -- --ignored`).

### Removed (effectively)

- The `~/.local/bin/claude` shim. v0.1's `cellar run` exec model is gone
  from the recommended flow; the binary still ships `run` for back-compat
  but it prints a deprecation error and exits non-zero.

[0.2.0]: https://github.com/OnCeUponTry/claude-cellar/releases/tag/v0.2.0

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
