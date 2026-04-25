# claude-cellar

Transparent zstd compression for Claude Code session files.

[Claude Code](https://claude.com/claude-code) stores every session as a plain
JSONL file under `~/.claude/projects/**/`. Over time these accumulate — hundreds
of MB of text that compress down to ~18% of their size but can never be
appended to while compressed. `claude-cellar` keeps the N most recent sessions
uncompressed and zstd-compresses the rest, then transparently re-hydrates them
when you launch Claude so the `/resume` picker still sees every session.

Verified compression on real sessions: **~82% size reduction** (zstd -19).
Hydration of 10 sessions takes **~5 ms** (parallel, rayon). Round-trip is
hash-verified before deleting originals.

## Why

- Sessions are plain text; `zstd -19` reduces a 4.6 MB session to ~870 KB.
- Compression is one-shot at session close; decompression is on the `/resume`
  critical path, so the codec choice favors fast decoding.
- The `/resume` picker only sees `.jsonl`, so the wrapper hydrates before
  launching Claude and cleans up on exit — zero change to your workflow.

## Install

### From crates.io

```bash
cargo install claude-cellar
claude-cellar install   # creates the shim in ~/.local/bin/claude (Unix) or %USERPROFILE%\bin\claude.cmd (Windows)
```

`install` auto-detects the real `claude` binary (canonical locations on
Linux/macOS/Windows), stores the path in `~/.config/claude-cellar/claude-bin.path`,
and replaces `claude` in PATH with a ~75-byte shim that invokes
`claude-cellar run -- "$@"`.

Revert with `claude-cellar uninstall`.

### From source

```bash
git clone https://github.com/OnCeUponTry/claude-cellar
cd claude-cellar
cargo build --release
cp target/release/claude-cellar ~/.local/bin/
claude-cellar install
```

## Usage

Everyday use is **transparent**. Launch `claude` as always; the shim hydrates
compressed sessions in parallel (~5 ms for 10 files), Claude sees every
session in `/resume`, and on exit the wrapper re-compresses any session that
was modified and deletes the unchanged hydrated files.

Manual commands for one-off operations:

```bash
# Compress all but the 5 most recent sessions (by mtime) in a directory
claude-cellar archive ~/.claude/projects/<your-project>/ --keep 5

# Preview without changes
claude-cellar archive <dir> --keep 5 --dry-run

# Compress/decompress single files
claude-cellar compress path/to/session.jsonl
claude-cellar decompress path/to/session.jsonl.zst

# Resume a specific session by id (auto-decompresses to tmpfs if needed)
claude-cellar resume <session-id>

# List sessions with size and compression state
claude-cellar list ~/.claude/projects/<your-project>/

# Inspect the persistent log
claude-cellar log --tail 20
```

## Architecture

| Component | Purpose |
|---|---|
| `archive` | Scans a directory recursively for `.jsonl` sessions, keeps the N most recent uncompressed (`--keep`), compresses the rest in parallel with zstd level 19, verifies round-trip hash, then deletes originals. |
| `run`     | Hydrates all `.jsonl.zst` → `.jsonl` in parallel, spawns the real `claude` binary with forwarded arguments, waits, then in parallel either re-compresses sessions that were modified or deletes hydrated files whose hash/mtime matches the snapshot. |
| `install` | Auto-detects the real `claude` binary, persists its path, and replaces `~/.local/bin/claude` with a bash shim (`claude.cmd` on Windows) that routes every invocation through `run`. |
| `resume`  | Single-session decompression to a tmpfs scratch (`$XDG_RUNTIME_DIR/claude-cellar/scratch/` on Linux, `$TMPDIR` elsewhere). |

Signal handling: `run` installs a SIGINT/SIGTERM/SIGHUP forwarder so `Ctrl+C`
reaches Claude but the wrapper survives to perform cleanup. Without this the
compression state would leak on abnormal exit.

## Paths

| Platform | Log | Scratch | Config |
|---|---|---|---|
| Linux   | `$XDG_STATE_HOME/claude-cellar/cellar.log` (default `~/.local/state/...`) | `$XDG_RUNTIME_DIR/claude-cellar/scratch/` (tmpfs) | `$XDG_CONFIG_HOME/claude-cellar/` |
| macOS   | `~/Library/Application Support/claude-cellar/cellar.log` | `$TMPDIR/claude-cellar-scratch/` | `~/Library/Application Support/claude-cellar/` |
| Windows | `%LOCALAPPDATA%\claude-cellar\cellar.log` | `%TEMP%\claude-cellar-scratch\` | `%APPDATA%\claude-cellar\` |

## Overrides

| Variable | Purpose |
|---|---|
| `CLAUDE_CELLAR_CLAUDE_BIN` | Explicit path to the real Claude binary; skips auto-detection. |

## Benchmarks

Measured on three real Claude Code sessions (NixOS, zstd 1.5.7 CLI, single
archive pass):

| Session size | gzip -9 | zstd -3 | zstd -19 | xz -6 |
|---:|---:|---:|---:|---:|
|   670 KB | 17.3% | 13.6% | **11.6%** | 11.1% |
|   1.9 MB | 19.5% | 16.9% | **15.0%** | 14.6% |
|   4.6 MB | 23.9% | 20.3% | **18.1%** | 17.8% |

zstd -19 lands within 0.3 points of xz -6 on ratio and decompresses ~20× faster
(important on `/resume`). zstd -3 is close on ratio with near-zero compression
time; the default `archive` uses `-19` because compression is one-shot and
the extra ratio is free.

## Status

- v0.1.0 — first public release
- Tested on Linux x86_64 (NixOS) end-to-end: hydrate, resume, exit cleanup,
  SIGINT survival
- Windows and macOS: code paths are implemented (`dirs`, `Path`,
  `Command::status`, `cfg(windows)` shim) but not yet smoke-tested on those
  platforms in this release

## License

Licensed under either of:

- **MIT License** ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.
