#!/usr/bin/env bash
# The local page's rendered checks: no page overflow at 320, 390,
# 768, 1024 and 1440 CSS px; 44 px targets; the WCAG text-spacing override at 320; 200% text
# at 1280; AA contrast. The page's own files are served with a crowded status
# (page_checks/status.json) and measured in headless Chrome. Not in the gate: it needs Chrome.
# Run after changing anything under crates/pulsar-link/src/page/.
set -euo pipefail
cd "$(dirname "$0")/.."

CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
PORT=${PORT:-38111}
PAGE=${PAGE:-crates/pulsar-link/src/page}
DIR=$(mktemp -d)
trap 'kill "${SERVER:-0}" 2>/dev/null || true; rm -rf "$DIR"' EXIT

mkdir -p "$DIR/api" "$DIR/fonts"
cp "$PAGE"/{index.html,app.js,app.css,tokens.css,pulsar-wordmark.svg} "$DIR/"
cp "$PAGE/pulsar-favicon.svg" "$DIR/favicon.svg"
cp "$PAGE/instrument-sans-latin-wght.woff2" "$DIR/fonts/"
cp scripts/page_checks/status.json "$DIR/api/status"
cp scripts/page_checks/check.html "$DIR/"

python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$DIR" >/dev/null 2>&1 &
SERVER=$!
disown
sleep 1
OUT=$("$CHROME" --headless=new --disable-gpu --window-size=1600,1000 --virtual-time-budget=30000 \
  --dump-dom "http://127.0.0.1:$PORT/check.html" 2>/dev/null |
  python3 -c 'import html, re, sys
m = re.search(r"<pre id=\"out\">(.*?)</pre>", sys.stdin.read(), re.S)
print(html.unescape(m.group(1)) if m else "no result: the checks did not finish")')
echo "$OUT"
[[ "$OUT" == *PASS ]]
