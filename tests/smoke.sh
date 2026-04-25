#!/usr/bin/env bash
# Manual smoke test for the v0.2 FUSE mount.
# Requires: target/release/claude-cellar built, fusermount3 in PATH.
# Usage: bash tests/smoke.sh

set -euo pipefail

BIN="${CELLAR_BIN:-target/release/claude-cellar}"
[[ -x "$BIN" ]] || { echo "binary not found at $BIN; build with cargo build --release"; exit 1; }

TMP="$(mktemp -d)"
trap 'fusermount3 -u "$TMP/mount" 2>/dev/null || true; rm -rf "$TMP"' EXIT

mkdir -p "$TMP/store/proj-A" "$TMP/mount"

echo "== 1. Pre-populate store with one compressed session =="
echo "alpha" > "$TMP/seed.jsonl"
echo "beta"  >> "$TMP/seed.jsonl"
"$BIN" compress "$TMP/seed.jsonl" >/dev/null
mv "$TMP/seed.jsonl.zst" "$TMP/store/proj-A/abc-123.jsonl.zst"

echo "== 2. Mount FUSE =="
(setsid "$BIN" mount --foreground --store-dir "$TMP/store" --mount-dir "$TMP/mount" </dev/null >"$TMP/log" 2>&1 & disown)
sleep 1
mount | grep -q "$TMP/mount" || { echo "FAIL: not mounted"; cat "$TMP/log"; exit 1; }
echo "  ok"

echo "== 3. Read-only roundtrip =="
got=$(cat "$TMP/mount/proj-A/abc-123.jsonl")
[[ "$got" == $'alpha\nbeta' ]] || { echo "FAIL: read got '$got'"; exit 1; }
echo "  ok"

echo "== 4. Append =="
echo "gamma" >> "$TMP/mount/proj-A/abc-123.jsonl"
sleep 0.3
got=$(cat "$TMP/mount/proj-A/abc-123.jsonl")
[[ "$got" == $'alpha\nbeta\ngamma' ]] || { echo "FAIL: append got '$got'"; exit 1; }
echo "  ok"

echo "== 5. Create new session =="
printf "line1\nline2\n" > "$TMP/mount/proj-A/new-456.jsonl"
sleep 0.3
got=$(cat "$TMP/mount/proj-A/new-456.jsonl")
[[ "$got" == $'line1\nline2' ]] || { echo "FAIL: new session got '$got'"; exit 1; }
echo "  ok"

echo "== 6. Mkdir new project =="
mkdir "$TMP/mount/proj-B"
echo "in-B" > "$TMP/mount/proj-B/zzz.jsonl"
sleep 0.3
[[ -f "$TMP/store/proj-B/zzz.jsonl.zst" ]] || { echo "FAIL: zzz.zst not in store"; exit 1; }
echo "  ok"

echo "== 7. Multi-writer (4 concurrent) =="
mkdir "$TMP/mount/proj-multi"
for i in 1 2 3 4; do
  (
    f="$TMP/mount/proj-multi/sess-$i.jsonl"
    for j in $(seq 1 200); do echo "writer-$i first-$j" >> "$f"; done
    sleep 0.$((RANDOM % 10))
    for j in $(seq 201 400); do echo "writer-$i second-$j" >> "$f"; done
  ) &
done
wait
sleep 0.5
for i in 1 2 3 4; do
  n=$(wc -l < "$TMP/mount/proj-multi/sess-$i.jsonl")
  [[ "$n" -eq 400 ]] || { echo "FAIL: sess-$i has $n lines, expected 400"; exit 1; }
done
echo "  ok"

echo "== 8. Umount =="
fusermount3 -u "$TMP/mount"
echo "  ok"

echo "== 9. Verify all sessions on disk decompress correctly =="
for f in "$TMP/store"/*/*.zst; do
  out=$(mktemp)
  "$BIN" decompress "$f" "$out" >/dev/null
  [[ -s "$out" ]] || { echo "FAIL: $f decompressed empty"; exit 1; }
  rm "$out"
done
echo "  ok"

echo
echo "ALL SMOKE TESTS PASSED"
