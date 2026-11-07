#!/usr/bin/env bash
# Refreshes the dashboard: `bot dashboard` ingests the journal + run log into
# <workdir>/data/metrics.db (durable history) and renders <workdir>/data/dashboard.html.
# Separate process from the live bot: it only reads the bot's files and uses dashboard.rpc.
#
#   tmux new-window -n dash ./scripts/dashboard_loop.sh        # refresh every $EVERY seconds
#   ./scripts/dashboard_loop.sh once                           # one pass, drain the backlog, exit
#
# Overrides (env): BIN (dashboard binary), CFG (config file), WORKDIR (holds data/), EVERY (seconds).
# Point BIN/WORKDIR at another build or data dir to render a copied run (e.g. a shadow snapshot):
#   BIN=/path/to/bot WORKDIR=/path/to/snapshot CFG=config.toml ./scripts/dashboard_loop.sh once
#
# Then open <workdir>/data/dashboard.html in a browser (from Windows under WSL2: \\wsl$\<distro>\<path>).
set -u
WORKDIR=${WORKDIR:-"$(dirname "$0")/../bot"}
cd "$WORKDIR" || exit 1
BIN=${BIN:-./target/release/bot}
CFG=${CFG:-config.local.toml}
EVERY=${EVERY:-60}

mkdir -p data

# One pass, draining the whole liquidation-tx backlog (hundreds of txs after a busy day take a
# few minutes at the spaced-out lookup rate), then exit.
if [ "${1:-}" = once ] || [ "${1:-}" = drain ]; then
  exec nice -n 10 timeout 3600 "$BIN" -c "$CFG" dashboard --drain
fi

# Periodic refresh: one batch per pass, so a backlog drains gently over several passes.
while true; do
  if ! nice -n 10 timeout 300 "$BIN" -c "$CFG" dashboard >> data/dashboard.log 2>&1; then
    echo "$(date -u +%FT%TZ) dashboard pass failed" >> data/dashboard.log
  fi
  sleep "$EVERY"
done
