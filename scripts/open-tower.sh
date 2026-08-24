#!/usr/bin/env bash
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
plugin_id="${HERDR_PLUGIN_ID:-sm-yjr.herdr-coordinator}"

action_open() {
  exec "$herdr_bin" plugin pane open \
    --plugin "$plugin_id" \
    --entrypoint tower \
    --placement overlay \
    --focus
}

decision="OPEN"
if command -v python3 >/dev/null 2>&1; then
  panes="$("$herdr_bin" pane list 2>/dev/null || true)"
  if [ -n "$panes" ]; then
    decision="$(printf '%s' "$panes" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    print("OPEN"); raise SystemExit(0)
result = data.get("result", data)
panes = result.get("panes", []) if isinstance(result, dict) else []
tower = next(
    (p for p in panes if (p.get("label") or p.get("title") or "") == "Fleet Control Tower"),
    None,
)
if not tower or not tower.get("pane_id"):
    print("OPEN")
elif tower.get("focused"):
    print("CLOSE " + str(tower["pane_id"]))
else:
    print("FOCUS " + str(tower["pane_id"]))
' 2>/dev/null || echo OPEN)"
  fi
fi

case "$decision" in
  "FOCUS "*)
    exec "$herdr_bin" plugin pane focus "${decision#FOCUS }"
    ;;
  "CLOSE "*)
    exec "$herdr_bin" pane close "${decision#CLOSE }"
    ;;
  *)
    action_open
    ;;
esac
