#!/usr/bin/env bash
# Bundle integrity smoke for v0.3:
# - Extract a real backup of Claude sessions (with subagents/tool-results).
# - Hash every file inside.
# - Install cellar v0.3 (which migrates).
# - Mount + verify every original file is accessible via the FUSE mount.
# - Hash via mount and compare against pre-install hashes.
# - If any mismatch: FAIL.
#
# Usage:
#   CELLAR_BIN=path/to/claude-cellar BACKUP=path/to/sessions-666.tgz \
#     bash tests/bundle_smoke.sh

set -euo pipefail

BIN="${CELLAR_BIN:-./target/release/claude-cellar}"
BKP="${BACKUP:?Set BACKUP=/path/to/sessions-666.tgz}"
[[ -x "$BIN" ]] || { echo "binary not executable: $BIN"; exit 1; }
[[ -f "$BKP" ]] || { echo "backup not found: $BKP"; exit 1; }

TMP="$(mktemp -d)"
trap '
  fusermount3 -u "$TMP/projects" 2>/dev/null || true
  rm -rf "$TMP"
' EXIT

# Layout: simulate a user with NFS-style symlink.
mkdir -p "$TMP/projects" "$TMP/canonical"
tar xzf "$BKP" -C "$TMP/canonical/"   # → $TMP/canonical/sessions-666/
NFS_DIR="$TMP/canonical/sessions-666"
[[ -d "$NFS_DIR" ]] || { echo "extracted dir missing"; exit 1; }
ln -s "$NFS_DIR" "$TMP/projects/-home-momo"

echo "== 1. Catalog every file in the source tree =="
PRE_HASHES="$TMP/pre-hashes.txt"
( cd "$NFS_DIR" && find . -type f -print0 | sort -z | xargs -0 sha256sum ) > "$PRE_HASHES"
PRE_COUNT=$(wc -l < "$PRE_HASHES")
echo "  $PRE_COUNT files cataloged"

echo "== 2. claude-cellar install (migrate inside the symlink target) =="
export CLAUDE_CELLAR_MOUNT_DIR="$TMP/projects"
export CLAUDE_CELLAR_SCRATCH_DIR="$TMP/scratch"
mkdir -p "$TMP/scratch"
"$BIN" install --no-systemd
# Mount in foreground in a subshell so we can verify; let it run in bg here.
(setsid "$BIN" mount --foreground --mount-dir "$TMP/projects" \
  --store-dir "$NFS_DIR" </dev/null >"$TMP/cellar.log" 2>&1 & disown)
sleep 1

echo "== 3. Verify mount is up =="
mount | grep -q "$TMP/projects" || { echo "FAIL: not mounted"; cat "$TMP/cellar.log"; exit 1; }

echo "== 4. Hash every file via the FUSE mount =="
POST_HASHES="$TMP/post-hashes.txt"
PROJ_DIR="$TMP/projects/-home-momo"
[[ -d "$PROJ_DIR" ]] || { echo "FAIL: project dir not visible"; exit 1; }
( cd "$PROJ_DIR" && find . -type f -print0 | sort -z | xargs -0 sha256sum ) > "$POST_HASHES"
POST_COUNT=$(wc -l < "$POST_HASHES")
echo "  $POST_COUNT files visible via mount"

echo "== 5. Compare =="
if [[ "$PRE_COUNT" -ne "$POST_COUNT" ]]; then
  echo "FAIL: file count mismatch (pre=$PRE_COUNT post=$POST_COUNT)"
  diff <(awk '{print $2}' "$PRE_HASHES" | sort) \
       <(awk '{print $2}' "$POST_HASHES" | sort) | head -50
  exit 1
fi

# Compare hash sets ignoring filename order.
diff <(awk '{print $1, $2}' "$PRE_HASHES" | sort) \
     <(awk '{print $1, $2}' "$POST_HASHES" | sort) > "$TMP/hashdiff" || true
if [[ -s "$TMP/hashdiff" ]]; then
  echo "FAIL: hash mismatches:"
  head -30 "$TMP/hashdiff"
  exit 1
fi
echo "  every file's content is identical end-to-end"

echo "== 6. Round-trip a write =="
NEW_FILE="$PROJ_DIR/__test_smoke__.jsonl"
echo '{"smoke":"ok"}' > "$NEW_FILE"
sleep 0.3
read_back=$(cat "$NEW_FILE")
[[ "$read_back" == '{"smoke":"ok"}' ]] || { echo "FAIL: write/read mismatch: $read_back"; exit 1; }
echo "  write OK"

echo "== 7. Cleanup =="
fusermount3 -u "$TMP/projects"
echo
echo "ALL BUNDLE TESTS PASSED  ($PRE_COUNT files round-tripped)"
