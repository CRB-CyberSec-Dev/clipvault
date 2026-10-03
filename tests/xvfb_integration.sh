#!/usr/bin/env bash
# Headless X11 integration test for clipvault.
# Boots the daemon under Xvfb and verifies capture → store → serve.
#
# Usage: ./tests/xvfb_integration.sh   (or: make xvfb-test)
set -u

BIN="${CLIPVAULT_BIN:-./target/debug/clipvault}"
XVFB_DISPLAY=":99"
PASS=0
FAIL=0

say()  { printf '\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

cleanup() {
    [ -n "${DPID:-}" ] && kill "$DPID" 2>/dev/null
    [ -n "${XPID:-}" ] && kill "$XPID" 2>/dev/null
}
trap cleanup EXIT

say "build"
cargo build --workspace >/dev/null 2>&1 || { echo "build failed"; exit 1; }

say "xvfb + daemon"
Xvfb "$XVFB_DISPLAY" -screen 0 1280x800x24 >/dev/null 2>&1 &
XPID=$!
sleep 1

export DISPLAY="$XVFB_DISPLAY"
# Isolate IPC socket and state from any real daemon/session.
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d)
chmod 700 "$XDG_RUNTIME_DIR"
export HOME
HOME=$(mktemp -d)
mkdir -p "$HOME/.config"

"$BIN" daemon >/tmp/clipvault-xvfb.log 2>&1 &
DPID=$!
sleep 2

say "1. capture text"
echo "xvfb-capture-test" | xclip -selection clipboard
sleep 1
if "$BIN" list | grep -q "xvfb-capture-test"; then ok "text captured"; else fail "text not captured"; fi

say "2. capture image"
base64 -d > /tmp/cv-test.png <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==
PNG
xclip -selection clipboard -t image/png /tmp/cv-test.png
sleep 1
if "$BIN" list | grep -q "image"; then ok "image captured"; else fail "image not captured"; fi
if ls "$HOME/.local/share/clipvault/images/"*.png >/dev/null 2>&1; then ok "image file stored"; else fail "image file missing"; fi

say "3. large payload (INCR read)"
python3 -c "print('Z'*1200000, end='')" | xclip -selection clipboard
sleep 2
if "$BIN" list | head -1 | grep -q "ZZZZ"; then ok "1.2MB clip captured"; else fail "INCR capture failed"; fi

say "4. serve back (clipboard ownership)"
ID=$("$BIN" list | grep "xvfb-capture-test" | awk '{print $1}')
"$BIN" select "$ID"
sleep 0.5
OUT=$(xclip -selection clipboard -o)
if [ "$OUT" = "xvfb-capture-test" ]; then ok "round-trip select"; else fail "select served: '$OUT'"; fi

say "5. dedup (re-copy bumps, no new row)"
BEFORE=$("$BIN" list | wc -l)
echo "xvfb-capture-test" | xclip -selection clipboard
sleep 1
AFTER=$("$BIN" list | wc -l)
if [ "$BEFORE" = "$AFTER" ]; then ok "dedup held ($AFTER rows)"; else fail "row count $BEFORE -> $AFTER"; fi

say "6. self-suppression (our own copy must not duplicate)"
"$BIN" select "$ID"
sleep 1
AFTER2=$("$BIN" list | wc -l)
if [ "$AFTER" = "$AFTER2" ]; then ok "no self-capture"; else fail "self-capture: $AFTER -> $AFTER2"; fi

say "7. IPC status"
if "$BIN" status | grep -q '"ok":true'; then ok "status responds"; else fail "status broken"; fi

say "8. clear keeps pins"
# pin id via sqlite (CLI has no pin; UI has) — use python sqlite3
DB="$HOME/.local/share/clipvault/history.db"
python3 - "$DB" "$ID" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("UPDATE items SET pinned=1 WHERE id=?", (sys.argv[2],)); c.commit()
PY
"$BIN" clear
sleep 0.5
if "$BIN" list | grep -q "xvfb-capture-test"; then ok "pinned survived clear"; else fail "pinned item was cleared"; fi

say "result"
echo "  passed: $PASS   failed: $FAIL"
[ "$FAIL" = 0 ]
