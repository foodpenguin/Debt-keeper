#!/usr/bin/env bash
# Refreshes the dashboard every minute: `bot dashboard` ingests the journal + run log into
# bot/data/metrics.db (durable history) and renders bot/data/dashboard.html.
# Separate process from the live bot: it only reads the bot's files and uses dashboard.rpc.
#
#   tmux new-window -n dash ./scripts/dashboard_loop.sh
#
# Then open bot/data/dashboard.html in a browser (from Windows under WSL2: \\wsl$\<distro>\<path>).
set -u
cd "$(dirname "$0")/../bot" || exit 1
CFG=${CFG:-config.local.toml}
EVERY=${EVERY:-60}
while true; do
  if ! nice -n 10 timeout 300 ./target/release/bot -c "$CFG" dashboard >> data/dashboard.log 2>&1; then
    echo "$(date -u +%FT%TZ) dashboard pass failed" >> data/dashboard.log
  fi
  sleep "$EVERY"
done
