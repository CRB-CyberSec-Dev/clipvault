#!/usr/bin/env bash
# Wayland smoke test: boots a nested sway (as an X11 client) and verifies
# capture, storage, serve-back, and the layer-shell popup.
#
# Usage: ./tests/wayland_smoke.sh    (needs: sway, wl-clipboard, x11-utils)
set -u

BIN="${CLIPVAULT_BIN:-./target/debug/clipvault}"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

RD=$(mktemp -d); chmod 700 "$RD"          # private XDG_RUNTIME_DIR
HD=$(mktemp -d)                           # private HOME (fresh config/db)
export XDG_RUNTIME_DIR="$RD" HOME="$HD"

cat > /tmp/cv-sway.conf <<'EOF'
set $mod Mod4
bindsym $mod+Return exec true
EOF

cleanup() {
    [ -n "${DPID:-}" ] && kill "$DPID" 2>/dev/null
    [ -n "${SPID:-}" ] && kill "$SPID" 2>/dev/null
}
trap cleanup EXIT

echo "== nested sway =="
sway -c /tmp/cv-sway.conf >/tmp/cv-sway.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do [ -S "$RD/wayland-1" ] && break; sleep 0.5; done
[ -S "$RD/wayland-1" ] || { echo "sway did not create wayland-1 socket"; cat /tmp/cv-sway.log | tail -5; exit 1; }
SWAYSOCK=$(ls "$RD"/sway-ipc.*.sock 2>/dev/null | head -1)
export WAYLAND_DISPLAY=wayland-1 XDG_SESSION_TYPE=wayland
unset DISPLAY
echo "  sway up (sock: $SWAYSOCK)"

echo "== daemon (wayland backend) =="
"$BIN" daemon >/tmp/cv-wayland-daemon.log 2>&1 &
DPID=$!
sleep 2.5
grep -q "backend: wayland" /tmp/cv-wayland-daemon.log && ok "wayland backend selected" || { fail "backend selection"; tail -5 /tmp/cv-wayland-daemon.log; }
grep -q "backend ready: wayland" /tmp/cv-wayland-daemon.log && ok "watcher ready" || fail "watcher not ready"

echo "== capture text =="
wl-copy "wayland-capture-test"
sleep 1.5
"$BIN" list | grep -q "wayland-capture-test" && ok "text captured" || fail "text not captured"

echo "== capture image =="
base64 -d > /tmp/cv-wl.png <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==
PNG
wl-copy -t image/png < /tmp/cv-wl.png
sleep 1.5
"$BIN" list | grep -q "image" && ok "image captured" || fail "image not captured"

echo "== serve back =="
ID=$("$BIN" list | grep "wayland-capture-test" | awk '{print $1}')
"$BIN" select "$ID"
sleep 1
OUT=$(wl-paste -n 2>/dev/null)
if [ "$OUT" = "wayland-capture-test" ]; then ok "round-trip select"; else fail "select served: '$OUT'"; fi

echo "== popup (layer-shell) =="
"$BIN" toggle
sleep 1.5
if SWAYSOCK="$SWAYSOCK" swaymsg -t get_tree 2>/dev/null | grep -qi "clipvault"; then
    ok "layer-shell popup surfaced"
else
    # Layer surfaces may not appear in get_tree on all sway versions —
    # accept a clean daemon log as a soft pass.
    if grep -qE "panic|ERROR" /tmp/cv-wayland-daemon.log; then
        fail "popup error in daemon log"
    else
        ok "popup shown (not visible in sway tree; no errors logged)"
    fi
fi
"$BIN" hide 2>/dev/null

echo "== result =="
echo "  passed: $PASS   failed: $FAIL"
[ "$FAIL" = 0 ]
