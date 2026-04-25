# claude-cellar

Bundle-aware transparent zstd compression for Claude Code sessions, via
a Linux FUSE filesystem.

[Claude Code](https://claude.com/claude-code) stores each session as a
top-level `<uuid>.jsonl` plus an optional sibling directory `<uuid>/`
that holds sub-agents and tool-results. Over time these accumulate.

`claude-cellar` mounts a FUSE on `~/.claude/projects/`. The session
files (`.jsonl`) are stored compressed (`.jsonl.zst`) and decompressed
lazily on `open`; the sibling bundle dirs (sub-agents, tool-results)
pass through unchanged. From Claude's perspective everything looks
exactly like the regular layout.

Verified end-to-end on real sessions with sub-agents: SHA-256 hash of
every original byte (`.jsonl`, sub-agent files, tool-result files) is
preserved through the mount.

## Install (Linux only)

```bash
cargo install claude-cellar
claude-cellar install
```

After `install`, `claude` works as always. The FUSE daemon runs as a
systemd user service started at login.

If you already had Claude sessions in `~/.claude/projects/`, `install`
auto-detects them and migrates each **bundle** (jsonl + sibling dir)
into the store. If `~/.claude/projects/<sub>` is a symlink to another
filesystem (e.g. NFS), `install` follows it and uses the target as the
store, so your data stays where it was.

### From the GitHub release tarball

```bash
curl -L -o /tmp/cellar.tgz \
  https://github.com/OnCeUponTry/claude-cellar/releases/download/v0.3.0/claude-cellar-v0.3.0-x86_64-unknown-linux-musl.tar.gz
mkdir -p ~/.local/bin
tar -xzf /tmp/cellar.tgz -C ~/.local/bin/
chmod +x ~/.local/bin/claude-cellar
~/.local/bin/claude-cellar install
```

### From source

```bash
git clone https://github.com/OnCeUponTry/claude-cellar
cd claude-cellar
cargo build --release
cp target/release/claude-cellar ~/.local/bin/
~/.local/bin/claude-cellar install
```

## What you don't have to do

- Run `claude-cellar mount` manually. systemd does it at login.
- Decompress sessions before resuming. The FUSE shows them already
  decompressed.
- Worry about sub-agent files or tool-results. They pass through.
- Worry about multiple `claude` instances. There is one daemon, every FD
  is kernel-isolated.
- Configure anything. Defaults work out of the box.

## Architecture

```
                            ~/.claude/projects/        (FUSE mount, virtual)
                                  │
                                  ▼
                         [ claude-cellar daemon ]
                          │             │             │
            decompress on open()  pass through    re-compress on release
                  │              │            │             (jsonl only)
                  ▼              ▼            ▼
          $XDG_RUNTIME_DIR  store/<proj>/<uuid>/    store/<proj>/
              (per-FD       (sub-agents,            <uuid>.jsonl.zst
               scratch)     tool-results,
                            raw, untouched)
```

| Path | Default | Override env |
|---|---|---|
| Mount | `~/.claude/projects/` | `CLAUDE_CELLAR_MOUNT_DIR` |
| Store | `~/.local/share/claude-cellar/store/` | `CLAUDE_CELLAR_STORE_DIR` |
| Scratch (tmpfs) | `$XDG_RUNTIME_DIR/claude-cellar/scratch/` | `CLAUDE_CELLAR_SCRATCH_DIR` |
| Log | `$XDG_STATE_HOME/claude-cellar/cellar.log` | — |

For NFS-shared layouts, point the store at the NFS dir; the FUSE keeps
serving the mount locally and only touches NFS for `.zst` reads/writes.

## Commands

Day-to-day you run `claude` (not `claude-cellar`). The CLI is here for
maintenance:

```bash
claude-cellar status                   # mount state, store size, daemon pid
claude-cellar log --tail 50            # daemon activity log
claude-cellar mount [--foreground]     # manual mount (rarely needed)
claude-cellar umount                   # manual umount
claude-cellar migrate-store [--from D] # migrate bundles into store layout
claude-cellar archive <dir> --keep N   # batch compress in a non-mount dir
claude-cellar compress <file>          # one-shot compress (stand-alone)
claude-cellar decompress <file.zst>    # one-shot decompress
claude-cellar list <dir>               # list sessions in a dir
claude-cellar resume <id>              # decompress + exec claude --resume
```

## How install decides where the store lives

| State of `~/.claude/projects/` | Result |
|---|---|
| Empty / absent | default store at `~/.local/share/claude-cellar/store/` |
| Has exactly one **symlink** subdir (NFS-shared layout) | follow the symlink: that target becomes the store; bundles migrate in place under a project sub-directory named after the symlink; the symlink is removed; systemd unit gets `Environment=CLAUDE_CELLAR_STORE_DIR=<target>` |
| Has real sub-directories with sessions | default store; migrate everything in |

`migrate-store` understands raw `.jsonl` (compresses on move), already-
compressed `.jsonl.zst` (plain rename), `.meta` sidecars, and the sibling
`<uuid>/` bundle dirs (moved as-is, preserving sub-agents and tool-results).

## Compatibility

| Claude Code setting / env / flag | Effect on cellar |
|---|---|
| Default install (`~/.claude/projects/`) | Works out of the box. |
| `CLAUDE_CODE_SKIP_PROMPT_HISTORY=1` | No `.jsonl` is written; FUSE sees nothing; no-op. |
| `--no-session-persistence` (per-run) | That run does not persist; cellar ignores it. |
| `cleanupPeriodDays` in settings.json | Claude prunes via the FUSE; cellar deletes the matching `.zst` and sidecar. |

## Configuration

| Var | Purpose |
|---|---|
| `CLAUDE_CELLAR_STORE_DIR` | Override store location (e.g. NFS path). |
| `CLAUDE_CELLAR_MOUNT_DIR` | Override mount location (rare). |
| `CLAUDE_CELLAR_SCRATCH_DIR` | Override scratch dir (should be tmpfs). |
| `CLAUDE_CELLAR_MAX_FDS` | Cap on simultaneous FUSE FDs (default 16). |
| `CLAUDE_CELLAR_CLAUDE_BIN` | Explicit path to the real Claude binary (used by `resume`). |

## Platform support

Linux x86_64 / aarch64 with FUSE (`CONFIG_FUSE_FS`) and `fusermount3`.

claude-cellar 0.3+ does **not** run on macOS or Windows; the crate refuses
to compile there with a clear diagnostic.

## Status of older versions

- **v0.2.x** is yanked: it compressed `.jsonl` but did not preserve the
  sibling `<uuid>/` bundle dirs that Claude creates for sessions with
  sub-agents or tool-results, so resuming such sessions could miss state.
  Use v0.3.0 instead.
- **v0.1.x** still works as a manual archive tool (no FUSE, no daemon).
  Single-instance use only.

## Benchmarks

Measured on three real Claude Code sessions (zstd 1.5.7, single archive):

| Session size | gzip -9 | zstd -3 | zstd -19 | xz -6 |
|---:|---:|---:|---:|---:|
|   670 KB | 17.3% | 13.6% | **11.6%** | 11.1% |
|   1.9 MB | 19.5% | 16.9% | **15.0%** | 14.6% |
|   4.6 MB | 23.9% | 20.3% | **18.1%** | 17.8% |

zstd -19 is within 0.3 points of xz -6 on ratio and decompresses ~20×
faster. Default for cellar is `-19`.

## License

Licensed under either of:

- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual-licensed as above, without any additional terms
or conditions.
