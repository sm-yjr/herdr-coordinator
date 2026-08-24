#!/usr/bin/env bash
# Resolve the same state directory as ./fleet, then launch the compatibility dashboard.
set -euo pipefail

PLUGIN_ID="sm-yjr.herdr-coordinator"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if [ -n "${HERDR_PLUGIN_STATE_DIR:-}" ]; then
  state_dir="$HERDR_PLUGIN_STATE_DIR"
  plugin_mode=1
elif [ -n "${HERDR_COORDINATOR_HOME:-}" ]; then
  state_dir="$HERDR_COORDINATOR_HOME"
  plugin_mode=0
else
  state_base="${XDG_STATE_HOME:-$HOME/.local/state}"
  stable="$state_base/herdr/plugins/$PLUGIN_ID"
  debug="$state_base/herdr-dev/plugins/$PLUGIN_ID"
  if [ -d "$stable" ]; then
    state_dir="$stable"
    plugin_mode=1
  elif [ -d "$debug" ]; then
    state_dir="$debug"
    plugin_mode=1
  else
    state_dir="$HOME/.herdr-coordinator"
    plugin_mode=0
  fi
fi

# dashboard_core.sh reads HERDR_COORDINATOR_HOME. In plugin mode, also keep the
# plugin markers so ./fleet watch-start becomes an intentional no-op.
export HERDR_COORDINATOR_HOME="$state_dir"
if [ "$plugin_mode" -eq 1 ]; then
  export HERDR_PLUGIN_STATE_DIR="$state_dir"
  export HERDR_PLUGIN_ID="${HERDR_PLUGIN_ID:-$PLUGIN_ID}"
fi

exec "$SCRIPT_DIR/dashboard_core.sh" "$@"
