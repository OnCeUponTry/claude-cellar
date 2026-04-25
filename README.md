# claude-cellar

Transparent zstd compression for Claude Code session files via a FUSE filesystem.

[Claude Code](https://claude.com/claude-code) stores every session as a plain
JSONL file under `~/.claude/projects/**/`. Over time these accumulate — hundreds
of MB of text that compress down to ~18% of their size with `zstd -19`.

`claude-cellar` v0.2 mounts a FUSE filesystem on `~/.claude/projects/` so
that Claude sees regular `.jsonl` files while the real bytes live as
`.jsonl.zst` in a separate **store** directory. Decompression happens lazily
on `open()` into a per-FD tmpfs scratch buffer; recompression happens on
the last `close()` if the FD was written to. Multi-claude is correct by
construction (every FD is kernel-isolated; there is exactly one daemon
serving the mount).

Verified compression on real sessions: **~82% size reduction** (zstd -19,
hash-verified round-trip). Decompression on open is sub-second for typical
sessions.

## Install (Linux)

### From crates.io

```bash
cargo install claude-cellar
claude-cellar install   # registers and starts the systemd user service
```

That's it. After `install`, `claude` works as always — the FUSE daemon
is up, mounted at `~/.claude/projects/`, and started automatically at
every login.

**If you already had Claude Code sessions** in `~/.claude/projects/`,
`install` auto-detects them and migrates them into the store
(compressing raw `.jsonl` and reorganizing into the project-subdir
layout the FUSE expects). The picker `/resume` shows everything from
the first launch.

If your `~/.claude/projects/<sub>` is a symlink to another location
(e.g. an NFS share), `install` follows the symlink and uses that target
as the store, so your sessions stay where they were rather than being
moved to a new directory.

### From source

```bash
git clone https://github.com/OnCeUponTry/claude-cellar
cd claude-cellar
cargo build --release
cp target/release/claude-cellar ~/.local/bin/
~/.local/bin/claude-cellar install
```

### Migration from v0.1.x

If you used v0.1's `install` (which created a `~/.local/bin/claude` shim):

```bash
claude-cellar uninstall   # removes the v0.1 shim, restores the real claude
claude-cellar install     # the new install handles the rest, including
                          # picking up existing .jsonl.zst into the store
```

## What you don't have to do

- Run `claude-cellar mount` manually. systemd does it.
- Decompress sessions before resuming. The FUSE mount makes them appear as
  regular `.jsonl`.
- Worry about multiple `claude` instances stepping on each other. There is
  one daemon, every FD is kernel-isolated.
- Configure anything. Defaults work out of the box.

## Architecture

```
                        ~/.claude/projects/        ← FUSE mount (virtual)
                              │
                              ▼
                     [ claude-cellar daemon ]
                       │                  │
            decompress on open()    re-compress on release()
                       │                  │
                       ▼                  ▼
        $XDG_RUNTIME_DIR/claude-cellar/   ~/.local/share/claude-cellar/store/
                  scratch/                          (compressed .jsonl.zst)
                  (per-FD tmpfs)
```

| Path | Default | Override env var |
|---|---|---|
| **Mount** | `~/.claude/projects/` | `CLAUDE_CELLAR_MOUNT_DIR` |
| **Store** | `~/.local/share/claude-cellar/store/` | `CLAUDE_CELLAR_STORE_DIR` |
| **Scratch** (tmpfs) | `$XDG_RUNTIME_DIR/claude-cellar/scratch/` | `CLAUDE_CELLAR_SCRATCH_DIR` |
| **Log** | `$XDG_STATE_HOME/claude-cellar/cellar.log` | — |
| **Config** | `$XDG_CONFIG_HOME/claude-cellar/` | — |

You can put the **store** on any filesystem (local, NFS, encrypted volume).
Scratch should stay on tmpfs (default). The mount is fixed at
`~/.claude/projects/` because that's what Claude looks at.

## Commands

Day-to-day, you run `claude` (not `claude-cellar`). The CLI is here for
maintenance and inspection:

```bash
claude-cellar status                    # mount state, store size, daemon pid
claude-cellar list <dir>                # list sessions in a dir (raw + zst)
claude-cellar log --tail 50             # daemon activity log
claude-cellar mount                     # manual mount (rarely needed)
claude-cellar umount                    # manual umount
claude-cellar migrate-store [--from D]  # move v0.1 .jsonl.zst into the store
claude-cellar archive <dir> --keep N    # batch compress a non-mount dir
claude-cellar compress <file>           # one-shot compress (stand-alone)
claude-cellar decompress <file.zst>     # one-shot decompress
claude-cellar resume <id>               # decompress one session and exec claude --resume
```

## Why FUSE (and not a wrapper)

v0.1 used a `cellar run` wrapper that hydrated every `.jsonl.zst` to
`.jsonl` before exec'ing `claude`, then re-compressed on exit. Two
structural problems:

1. **Transient disk doubled** during a session: ~25 MB of compressed
   sessions decompressed to ~125 MB on disk for the duration of every
   `claude` invocation.
2. **Multi-claude data loss**: with two `claude` instances open, the
   first wrapper to exit ran cleanup over files the others were still
   writing.

v0.2 sidesteps both by moving the boundary to the kernel: compressed
files stay compressed in the store; only the *currently open* sessions
are decompressed (in tmpfs); every FD is independent.

## Configuration

All env vars optional. Defaults work out of the box.

| Var | Purpose |
|---|---|
| `CLAUDE_CELLAR_STORE_DIR` | Override the store location (default `~/.local/share/claude-cellar/store`). Useful for NFS-shared layouts. |
| `CLAUDE_CELLAR_MOUNT_DIR` | Override the mount location (rare; default `~/.claude/projects`). |
| `CLAUDE_CELLAR_SCRATCH_DIR` | Override the scratch dir (default `$XDG_RUNTIME_DIR/claude-cellar/scratch`). |
| `CLAUDE_CELLAR_MAX_FDS` | Cap on simultaneous open FDs through the FUSE (default 16). |
| `CLAUDE_CELLAR_CLAUDE_BIN` | Explicit path to the real Claude binary (used by `resume`). |

## Compatibility

| Claude Code setting / env / flag | Effect on cellar |
|---|---|
| Default install (`~/.claude/projects/`) | Works out of the box. |
| `CLAUDE_CODE_SKIP_PROMPT_HISTORY=1` | No `.jsonl` is written; FUSE sees nothing; no-op. |
| `--no-session-persistence` (per-run) | That run does not persist; cellar ignores it. |
| `cleanupPeriodDays` in settings.json | Claude prunes via the FUSE; cellar deletes the matching `.zst` and sidecar. |

## Platform support

Linux x86_64 / aarch64. Requires a kernel with FUSE (`CONFIG_FUSE_FS`)
and `fusermount3` in PATH (any modern distro).

claude-cellar does not run on macOS or Windows. The crate refuses to
compile on those targets with a clear diagnostic.

## Benchmarks

Measured on three real Claude Code sessions (zstd 1.5.7, single archive pass):

| Session size | gzip -9 | zstd -3 | zstd -19 | xz -6 |
|---:|---:|---:|---:|---:|
|   670 KB | 17.3% | 13.6% | **11.6%** | 11.1% |
|   1.9 MB | 19.5% | 16.9% | **15.0%** | 14.6% |
|   4.6 MB | 23.9% | 20.3% | **18.1%** | 17.8% |

zstd -19 lands within 0.3 points of xz -6 on ratio and decompresses ~20×
faster (important on the `open()` path). zstd -19 is the default.

## License

Licensed under either of:

- **MIT License** ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.
