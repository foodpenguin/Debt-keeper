#!/usr/bin/env bash
# One-shot setup of the liquidation bot on Ubuntu 24.04 (native or WSL2).
# Idempotent: safe to re-run. Never touches keys: .env (RPC URLs) and the keystore are copied
# by you, by hand.
#
#   ./scripts/setup_wsl.sh                    # tools + build + config
#   ./scripts/setup_wsl.sh --install-service  # also install/enable the shadow-mode systemd service (sudo)
#   SKIP_APT=1 ./scripts/setup_wsl.sh         # skip apt (no sudo), e.g. when packages are present
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SERVICE=0
[[ "${1:-}" == "--install-service" ]] && INSTALL_SERVICE=1
say() { printf '\n==> %s\n' "$*"; }

case "$REPO" in
  /mnt/*) echo "ERROR: the repo is on a Windows drive ($REPO). Clone it under ~ (WSL filesystem) for speed." >&2; exit 1 ;;
esac

if [[ "${SKIP_APT:-0}" != "1" ]]; then
  say "apt packages (sudo)"
  sudo apt-get update -y
  sudo apt-get install -y build-essential pkg-config git curl chrony ca-certificates
fi

say "Rust"
if ! command -v cargo >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup update stable >/dev/null
cargo --version

say "Foundry"
if [[ ! -x "$HOME/.foundry/bin/forge" ]]; then
  curl -sSL https://foundry.paradigm.xyz | bash
  "$HOME/.foundry/bin/foundryup"
fi
export PATH="$HOME/.foundry/bin:$PATH"
forge --version | head -1

say "submodules + contracts (artifacts are read by the bot)"
cd "$REPO"
git submodule update --init --recursive
(cd contracts && forge build)

say "bot (release build)"
(cd bot && cargo build --release)

say "config"
if [[ ! -f bot/config.local.toml ]]; then
  cp bot/config.example.toml bot/config.local.toml
  echo "created bot/config.local.toml from the example (shadow mode)"
fi
mkdir -p bot/data
if [[ ! -f .env ]]; then
  echo "WARNING: $REPO/.env is missing. Copy it from the old machine by hand (it holds the RPC API keys;"
  echo "         it is git-ignored on purpose). Without it only the public Tenderly endpoint is usable."
fi

if [[ "$INSTALL_SERVICE" == "1" ]]; then
  say "systemd service (shadow mode)"
  if [[ "$(ps -p 1 -o comm=)" != "systemd" ]]; then
    echo "ERROR: systemd is not running. Add '[boot]\nsystemd=true' to /etc/wsl.conf, run 'wsl --shutdown' in Windows, reopen Ubuntu." >&2
    exit 1
  fi
  sed -e "s#@REPO@#$REPO#g" -e "s#@USER@#$USER#g" deploy/liqbot-shadow.service | sudo tee /etc/systemd/system/liqbot-shadow.service >/dev/null
  sed -e "s#@REPO@#$REPO#g" -e "s#@USER@#$USER#g" deploy/liqbot.logrotate | sudo tee /etc/logrotate.d/liqbot >/dev/null
  sudo systemctl daemon-reload
  sudo systemctl enable --now liqbot-shadow.service
  sudo systemctl enable --now chrony
  systemctl --no-pager status liqbot-shadow.service | head -5
fi

say "done"
echo "shadow run by hand : cd $REPO/bot && ./target/release/bot -c config.local.toml watch"
echo "service logs       : tail -f $REPO/bot/data/shadow_run.log"
echo "loss report        : cd $REPO/bot && ./target/release/bot -c config.local.toml report"
