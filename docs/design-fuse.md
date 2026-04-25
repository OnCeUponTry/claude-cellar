# claude-cellar v0.2 — FUSE-backed transparent compression

Design document. Pre-implementation review.

Status: **approved for implementation**.
Target version: 0.2.0 (breaking change vs 0.1.x).
Branch: `experimental/fuse-mount`.

## 1. Why a FUSE filesystem

`cellar run` (the v0.1 wrapper) hydrates every `.jsonl.zst` in the projects
tree to `.jsonl` before exec'ing claude, then re-compresses on exit.
Two structural problems:

- **Transient disk cost**: the entire tree is decompressed for the duration of
  the session. With 47 sessions × ~500 KB compressed = ~25 MB live during
  every `claude` invocation. The point of compression evaporates.
- **Multi-claude data loss**: with N≥2 simultaneous claudes in the same projects
  tree, the first wrapper to exit runs cleanup over files the others are still
  using. Files in active use get unlinked or recompressed mid-write. The
  v0.1 model assumes one claude at a time.

A FUSE filesystem moves the boundary down to the kernel:

- Compressed `.zst` files are the on-disk truth (the **store**).
- The mountpoint exposes them as virtual `.jsonl` files to claude.
- Decompression happens **lazily on `open()`**, per file descriptor, into a
  tmpfs scratch buffer. Recompression happens on `release()` (last close).
- Multi-claude correctness is free: every FD is independent at the kernel
  level. No process-level coordination needed.

## 2. Goals and non-goals

### Goals

- Transparent to claude — no env vars, no flags, no behavioral changes.
- Disk usage bounded: store size + (sessions currently open × decompressed size).
- Correctness with up to ~10 concurrent claudes on the same machine, per the
  user's stated upper bound (typical use ≤4).
- Robust against daemon crash: store is the source of truth; no in-memory
  state is load-bearing across restarts.
- Robust against Claude Code internal changes: cellar speaks filesystem
  syscalls, not Claude protocol. Anything Claude does that the kernel
  understands works.
- Single-binary install. No daemon written in another language. No native
  deps beyond `fusermount3` (already universal on modern Linux).

### Non-goals

- Cross-platform parity. v0.2 is Linux-only. macOS/Windows users stay on
  v0.1.x `cellar run` until macFUSE/WinFsp adapters are written (no plan).
- Multi-user (allow_other) on the same mount. Each user mounts their own.
- Streaming decompression for partial reads. We decompress the whole session
  on `open()` because Claude reads the entire `.jsonl` to render the picker
  preview and to hydrate state on resume.
- Coordination between cellar instances on different machines sharing an NFS
  store. The same store directory must not be mounted by two FUSE daemons.

## 3. Storage layout

Three filesystem locations. Names are configurable via env vars (defaults shown).

| Role | Default | Override env var | Persistence | Filesystem type |
|---|---|---|---|---|
| **Store** | `$XDG_DATA_HOME/claude-cellar/store/` (`~/.local/share/claude-cellar/store/`) | `CLAUDE_CELLAR_STORE_DIR` | persistent | any (local, NFS, etc.) |
| **Mount** | `~/.claude/projects/` | `CLAUDE_CELLAR_MOUNT_DIR` | virtual (FUSE) | fuse |
| **Scratch** | `$XDG_RUNTIME_DIR/claude-cellar/scratch/` | `CLAUDE_CELLAR_SCRATCH_DIR` | ephemeral (boot) | tmpfs |

### Store internals

```
$STORE/
  <sanitized-cwd-1>/
    <uuid>.jsonl.zst     ← compressed session
    <uuid>.jsonl.meta    ← sidecar: orig_size, mtime, sha256 (optional, for fast getattr)
  <sanitized-cwd-2>/
    ...
```

Layout mirrors what Claude Code itself uses under `~/.claude/projects/`. The
`.meta` sidecar caches the decompressed size and SHA-256 hash so `getattr`
doesn't need to decompress. If absent, cellar falls back to reading the
zstd frame header (which carries `--content-size` since zstd 1.3.4 — set by
default in the `zstd` Rust crate). Sidecar is rebuilt opportunistically on
first access.

### Mount semantics

The FUSE mount overlays `~/.claude/projects/`. From claude's perspective:

- Every `<uuid>.jsonl.zst` in the store appears as `<uuid>.jsonl`, decompressed
  size, decompressed mtime.
- `readdir` lists all sessions (none hidden). The picker `/resume` sees
  everything. **Solves the "session index" requirement directly.**
- `open(O_RDONLY)` for the picker preview: decompress on the fly to a per-FD
  scratch file, return that FD's data on `read()`.
- `open(O_RDWR | O_APPEND)` for an active session: same, but on `release()`
  re-compress the scratch contents back to the store.

The mountpoint takes over `~/.claude/projects/`. Existing claude sessions in
non-cellar projects that want to bypass the FUSE: not supported. The whole
projects root is owned by cellar.

### Scratch internals

```
$SCRATCH/
  fd-<pid>-<ino>-<random>.jsonl   ← per-FD decompressed buffer
```

`$XDG_RUNTIME_DIR` is tmpfs (RAM-backed, capped at 50% of RAM by default on
systemd systems). 4 active sessions × ~4 MB each = ~16 MB peak. Cleaned at
boot automatically.

## 4. Syscall semantics

The minimum FUSE op set we need to implement, with the contract.

| FUSE op | Behavior |
|---|---|
| `lookup(parent, name)` | Map name → store path. If `<name>.zst` exists in store, return inode + decompressed attrs. If `parent` is a project subdir we don't know, return ENOENT. |
| `getattr(ino)` | Return cached attrs (size from sidecar/zstd header, mtime from store file mtime, perm 0o600). Sub-ms. |
| `readdir(ino, offset)` | List `<sanitized-cwd>/` directories at root, list `*.jsonl.zst` (presented as `*.jsonl`) under each. |
| `open(ino, flags)` | Decompress `<store>/<path>.zst` → `$SCRATCH/fd-<pid>-<ino>-<rand>.jsonl`. Return scratch FD (held internally). Track {ino, scratch_path, dirty:false}. |
| `read(fh, offset, size)` | `pread()` on the scratch FD. |
| `write(fh, offset, data)` | `pwrite()` on the scratch FD. Mark `dirty=true`. |
| `release(fh)` | If dirty: read scratch, compress with zstd -19, verify SHA-256, atomic rename `<store>/<path>.zst.tmp` → `<store>/<path>.zst`, update sidecar. Always: `unlink(scratch)`, drop FD entry. |
| `unlink(name)` | `unlink(<store>/<name>.zst)`. Used for `cleanupPeriodDays` from claude. |
| `rename(from, to)` | `rename` in the store. Claude shouldn't rename sessions normally; supported for completeness. |
| `create(parent, name, flags)` | New session: create empty scratch FD, store path will exist on first `release(dirty)`. Track in fd table. |
| `fsync(fh)` | If dirty: flush scratch, **do not** recompress (would be too slow on every fsync). The compression happens on `release`. Call `fdatasync(scratch_fd)`. |
| `mkdir(parent, name)` | Create empty store subdir. Claude creates a project dir on first use. |

Ops we explicitly do **not** implement (return EROFS or EOPNOTSUPP):

- `setxattr` / `getxattr` — not used by claude.
- Hardlinks — not used.
- Symlinks within the mount — not used.

## 5. FD lifecycle and the dirty bit

```
open() → decompress to scratch → assign fh → fd table entry { ino, scratch_path, dirty=false, fd }
       │
       ├─ read()  → pread on scratch.   never dirties.
       ├─ write() → pwrite on scratch.  sets dirty=true.
       ├─ fsync() → fdatasync scratch.  no recompress.
       │
release()
       │
       ├─ if dirty: compress scratch → store.zst.tmp → verify SHA → rename → update sidecar
       └─ always: unlink scratch, drop fd table entry
```

Multiple FDs to the same inode (rare for sessions; possible if claude opens
the picker preview while another claude is editing): each FD has its own
scratch and own dirty bit. On `release`, the **last** FD to write wins per
the order of `release` calls. Per the user's constraint that no two claudes
ever edit the same session, this race never occurs in practice — but the
behavior is well-defined: last writer wins, no corruption, no truncation.

## 6. Multi-claude correctness

Refresher of the v0.1 bug: cellar #1 hydrates the tree, claude #1, #2, #3, #4
each open different sessions. Cellar #1 exits first and recompresses files
that #2/#3/#4 are still writing.

Under FUSE this can't happen by construction:

- Each `open()` from each claude goes through the kernel and into the same
  daemon process. There is exactly one daemon. There is no "first to exit".
- Each FD is a separate scratch file. Compressions on release only touch
  the FD's own scratch.
- Atomic rename on the store side means no half-written `.zst` is ever
  visible to a concurrent reader.
- The store is mutated only on `release(dirty=true)`, never speculatively.

The user's constraint ("no two claudes in the same session") becomes
unnecessary for correctness — it's still a sane operational rule, but if
violated, the result is well-defined (last release wins) rather than corruption.

## 7. Failure modes

### 7.1 Daemon crash with active FDs

Scratch files are orphaned in `$XDG_RUNTIME_DIR` (cleaned at next boot).
Store `.zst` files remain at the version they were before the crashed FDs
opened them — claude's writes during that session are lost. The
`.zst.tmp` files (if any in flight) are cleaned on daemon next start.

systemd unit: `Restart=on-failure`, `RestartSec=1s`. Claude's open FDs to
the unmounted FS get EIO; claude prints the error and exits. User restarts
claude, daemon is back up, sessions are intact at last successful release.

### 7.2 Daemon doesn't start (FUSE module missing, permissions, etc.)

`claude-cellar mount` exits with diagnostic. Fallback CLI: `claude-cellar
mount --passthrough` does a `bind` mount of `$STORE` directly to the mount
point. Claude sees raw `.zst` files (which it doesn't understand) — same as
having no cellar. Better than breaking claude entirely. User informed via
stderr that compression is off until the daemon is fixed.

### 7.3 Store on NFS, NFS goes away mid-session

Reads from the daemon to NFS fail → daemon returns EIO to claude →
claude shows a transient error. Once NFS is back, retry succeeds. No
corruption (we never have a partially written `.zst` on NFS that survives
NFS recovery — atomic rename is per-server).

### 7.4 Disk full during compress

`compress_file` writes `.zst.tmp`, fails on ENOSPC, deletes `.zst.tmp`,
returns error. The original `.zst` (older version) stays. The scratch file
is preserved (NOT unlinked) and we log a clear error so the user can
manually rescue it from `$SCRATCH/`. claude's release returns success to
claude (so claude doesn't loop), but cellar logs ERROR.

### 7.5 SHA-256 mismatch on round-trip verify

Same as `compress_file` v0.1 behavior: delete `.zst.tmp`, keep original
`.zst`, log ERROR. The scratch file is preserved.

## 8. Configuration

All env vars optional. Sensible defaults for zero-config install.

| Env var | Default | Purpose |
|---|---|---|
| `CLAUDE_CELLAR_STORE_DIR` | `$XDG_DATA_HOME/claude-cellar/store/` | Where compressed sessions live. Set to NFS path for shared/canonical setups. |
| `CLAUDE_CELLAR_MOUNT_DIR` | `~/.claude/projects/` | Where to mount. Almost never overridden. |
| `CLAUDE_CELLAR_SCRATCH_DIR` | `$XDG_RUNTIME_DIR/claude-cellar/scratch/` | Per-FD buffers. Should be tmpfs. |
| `CLAUDE_CELLAR_LOG_LEVEL` | `info` | `error` / `warn` / `info` / `debug`. |

Config file: none. Everything is env vars + CLI flags. Keeps state out of
the picture.

## 9. CLI surface

New / modified commands:

| Command | Purpose | Replaces in v0.1 |
|---|---|---|
| `mount [--passthrough] [--foreground]` | Start daemon and mount FUSE. `--foreground` for systemd `Type=simple`. | (new) |
| `umount` | Stop daemon and unmount. | (new) |
| `migrate-store [--from <dir>] [--dry-run]` | Move existing `.jsonl.zst` from old layout (`~/.claude/projects/`) to the store. | (new) |
| `status` | Print mount state, store size, active FDs, daemon PID. | (new) |
| `archive`, `compress`, `decompress`, `list`, `resume`, `log` | Unchanged. Operate directly on store. | same |

Removed / deprecated:

| Command | Status in v0.2 | Why |
|---|---|---|
| `run` | **deprecated** with stderr warning | Replaced by mount-based flow. |
| `install` (shim) | **deprecated** with stderr warning | No more `~/.local/bin/claude` shim; user runs `claude` natively. |
| `uninstall` | Repurposed: removes shim if installed by old version, prints migration instructions. | |

## 10. Migration from v0.1.x

User has `.jsonl.zst` files in `~/.claude/projects/<sanitized-cwd>/` from
the v0.1 archive command. Plus possibly an active `~/.local/bin/claude` shim.

```
$ claude-cellar migrate-store --dry-run
will move:
  ~/.claude/projects/-home-username/uuid-A.jsonl.zst → ~/.local/share/claude-cellar/store/-home-username/uuid-A.jsonl.zst
  ~/.claude/projects/-home-username/uuid-B.jsonl.zst → ...
will keep raw .jsonl in place (active sessions); they're picked up on first mount.

$ claude-cellar uninstall   # if v0.1 shim exists
removed: ~/.local/bin/claude (was cellar shim)

$ claude-cellar migrate-store   # actually moves
moved 47 .jsonl.zst (5.8 MB total)

$ systemctl --user enable --now claude-cellar.service
mounted ~/.claude/projects/ (FUSE, store: ~/.local/share/claude-cellar/store/)

$ claude   # works as always; picker shows everything; disk stays bounded
```

Migration is one-way. No `unmigrate` command. If they want to revert,
unmount + decompress everything back to `~/.claude/projects/` manually
(documented in README).

## 11. systemd user unit

```ini
[Unit]
Description=claude-cellar FUSE mount for Claude Code sessions
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/claude-cellar mount --foreground
ExecStop=%h/.local/bin/claude-cellar umount
Restart=on-failure
RestartSec=1
# Tighten privileges:
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/.local/share/claude-cellar %h/.claude %t/claude-cellar
PrivateTmp=no

[Install]
WantedBy=default.target
```

Shipped as a template the user enables with `systemctl --user enable
claude-cellar.service`. README documents how, and how to mount manually
without systemd.

## 12. Testing plan

### Unit tests (no FUSE)

- `compress_file` round-trip with verify hash (already exists).
- Sidecar read/write/repair.
- Store layout helpers (`sanitize_cwd`, `store_path_for`, etc).

### Integration tests (FUSE in CI)

CI runs on Linux with FUSE available (GitHub Actions `ubuntu-latest`
supports FUSE since 2023):

1. **Single claude basic**: mount, write a file via standard tools (`cp`,
   `cat`), unmount, verify store has compressed file with same content.
2. **Picker visibility**: pre-populate store with 50 `.zst`, mount, verify
   `ls` shows 50 `.jsonl` entries with correct sizes/mtimes.
3. **Lazy decompress**: pre-populate store, mount, `stat` all 50 entries,
   verify zero scratch files were created.
4. **Multi-FD same file**: open same file twice, verify both see same data.
5. **Multi-claude concurrent**: spawn 4 dummy "claude" processes
   (`(cat <input> >> file && sleep 5 && exit) &`), each appending to a
   different session, all release within milliseconds of each other,
   verify all 4 sessions are intact in the store with all 4 sets of writes.
6. **Crash recovery**: spawn dummy claude with active FD, `kill -9` the
   FUSE daemon, verify scratch file is orphaned (not corrupted), restart
   daemon, verify store still has the pre-crash version.
7. **NFS-backed store**: mount NFS dummy in CI (or skip on non-NFS CI),
   run scenario 5, verify correctness.
8. **Migration**: pre-populate `~/.claude/projects/` with v0.1-style
   layout, run `migrate-store`, verify store is correct and original is
   gone.

### Manual smoke (lab 666, then 777)

1. Daemon up, single `claude` session, exit. Verify `.zst` updated in store.
2. 4 simultaneous `claude` sessions in different cwds. All exit cleanly.
   Verify all 4 stored.
3. 4 simultaneous, kill -9 one mid-session. Verify others unaffected.
4. Daemon up for 24h continuous, monitor RAM/handles for leaks.
5. NFS-backed store (NFS-backed home use case): same as 1-4.

## 13. Decisions

1. **`mount` is fork-and-exit by default**, with `--foreground` for systemd.
   This is the standard FUSE/daemon pattern: interactive use returns to the
   prompt immediately; systemd uses `--foreground` so its `Type=simple`
   process model works correctly.

2. **Detection of Claude path-default changes is deferred to v0.2.1.**
   Under FUSE the failure mode is benign (no corruption — Claude just writes
   raw `.jsonl` outside cellar if it changes its default path). The `status`
   subcommand can flag this. Address only if it surfaces in real use.

3. **`archive` and `compress` refuse while the daemon is mounted.**
   They detect the mount via `/proc/self/mountinfo` or by reading the
   daemon's PID file; on conflict they print a clear error pointing the user
   to `claude-cellar umount` first. No silent races.

4. **Sidecar `.meta` files are GC'd on `unlink`.** When the daemon (or any
   other path) removes a `.zst`, the sibling `.meta` is removed in the same
   operation. No orphans.

5. **Concurrency cap: 16 active FDs.** Beyond that, `open()` returns
   `EMFILE`. Sized for the user's stated upper bound (~4 simultaneous
   claudes) plus headroom for picker previews and one-off tools. Not
   user-facing; pure backstop against runaway callers. Tuneable via
   `CLAUDE_CELLAR_MAX_FDS` env var if a real workload demands it.

## 13a. Lifecycle: daemon 24/7 (no shim)

The daemon runs as a systemd user service, started at login and stopped at
logout. **No `~/.local/bin/claude` shim is involved.** The user types
`claude` natively; the kernel routes its filesystem syscalls to the FUSE
daemon transparently.

Properties of this model:

- **One-time setup**: `claude-cellar install` registers and enables the
  systemd user service. After that, the user runs `claude` as always.
- **Single daemon for all claudes**: regardless of how many `claude`
  processes the user starts, there is exactly one daemon serving the mount.
  4 claudes = 1 daemon + 4 active FDs.
- **Resource cost when idle**: ~10 MB RAM, ~0% CPU. Negligible on any
  modern machine; not user-perceivable.
- **Zero startup latency**: the mount is always live; `claude` opens
  immediately.
- **No shim**: keeps the v0.2 codebase free of the bash-shim class of bugs
  that v0.1's `cellar run` introduced.

Auto-shutdown on idle (the alternative ephemeral mode) was considered and
rejected: the 10 MB RAM saving is not worth the ~300-500 ms startup
latency, the added shim complexity, and the new failure modes (race
between two `claude` invocations starting simultaneously, shim's
mountpoint check failing on slow systems, etc.). If a real user demands
ephemeral mode, it can be added behind a flag in v0.3.

## 14. Out of scope for v0.2

- Compaction of old `.zst` files (re-compress at higher level if zstd
  improves, or convert format) — manual via `archive` command.
- Encryption at rest — the user can put the store on an encrypted volume.
- Quotas/eviction policies — we don't delete anything; that's claude's
  `cleanupPeriodDays` job.
- Multi-user same machine on the same mount — separate mounts per user.

## 15. Deliverables

- `src/main.rs` refactored: keep utilities, add `mount`/`umount`/
  `migrate-store`/`status`, deprecate `run`/`install`.
- `src/fuse.rs` (new): the `Filesystem` impl + FD table + scratch lifecycle.
- `src/store.rs` (new): store layout helpers, sidecar I/O.
- `Cargo.toml`: add `fuser = { version = "0.15", default-features = false }`,
  `nix` for fdatasync/sigaction.
- `docs/design-fuse.md` (this file).
- `systemd/claude-cellar.service` (the unit).
- README updated: new flow, install steps, migration.
- CHANGELOG: 0.2.0 entry.
- Tests: `tests/fuse_integration.rs`.

Estimated LOC delta: +800-1000, -100 (deprecated paths simplified, not
removed).

---

End of design.
