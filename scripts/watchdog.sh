#!/usr/bin/env bash
# Watchdog for the live bot: emails you (Gmail SMTP via curl) when something needs attention.
# Uses no LLM and never touches keys. Runs as a loop, e.g. in a second tmux window:
#
#   ./scripts/watchdog.sh            # loop forever, check every CHECK_SECS (default 60)
#   ./scripts/watchdog.sh --once     # one check, then exit
#   ./scripts/watchdog.sh --test     # send a test email and exit
#   DRY_RUN=1 ./scripts/watchdog.sh --once   # print emails instead of sending
#
# Alerts (bundled into one email per check, at most MAX_MAILS_PER_HOUR):
#   - bot process gone, or no block processed for STALE_SECS
#   - LIVE tx SENT / LIVE tx outcome / LIVE send failed / HALTED / "sending paused" / "cannot cover worst-case gas"
#   - hot wallet below MIN_HOT_ETH (checked hourly)
#   - blocks reaching the bot late: median head_age_ms above HEAD_LAG_MS for LAG_CHECKS checks in a
#     row (NTP-synced healthy baseline: about +680 ms), or "flashblocks stream silent" in the log
# Plus a daily summary at SUMMARY_HOUR ($WATCH_TZ) so you know the watchdog itself is alive.
# Other liquidators' liquidations ("LIQUIDATION observed") only go into the daily summary.
#
# Mail settings (create them yourself, both chmod 600; never in git):
#   ~/.config/liqbot/mail.env : MAIL_FROM="you@gmail.com"  MAIL_TO="a@gmail.com b@gmail.com"  (space- or comma-separated)
#   ~/.config/liqbot/netrc    : machine smtp.gmail.com login you@gmail.com password <16-char app password>
# The app password is read by curl from the netrc file, so it never appears in argv or logs.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="${LOG:-$REPO/bot/data/live_run.log}"
STATE="${STATE:-$REPO/bot/data/watchdog}"
CONF="${CONF:-$HOME/.config/liqbot}"
# Hot wallet to watch: from the git-ignored local config (exec.executor), unless HOT is set.
HOT="${HOT:-$(grep -m1 "^executor = " "$REPO/bot/config.local.toml" 2>/dev/null | cut -d\" -f2)}"
RPC="${RPC:-https://mainnet.base.org}"
CHECK_SECS="${CHECK_SECS:-60}"
STALE_SECS="${STALE_SECS:-300}"
MIN_HOT_ETH="${MIN_HOT_ETH:-0.0002}"
SUMMARY_HOUR="${SUMMARY_HOUR:-09}"
WATCH_TZ="${WATCH_TZ:-UTC}"   # time zone of SUMMARY_HOUR and of the timestamps in mails
MAX_MAILS_PER_HOUR="${MAX_MAILS_PER_HOUR:-6}"
# Measured baseline with an NTP-synced clock, heads woken by the flashblock stream: p50 ~ +680 ms,
# p90 ~ +730 ms; the 2.5 s HTTP-poll fallback is ~ +1200 ms or worse. Keep the clock NTP-synced
# (WSL2 clocks drift without it, which makes these numbers meaningless).
HEAD_LAG_MS="${HEAD_LAG_MS:-1100}"
LAG_CHECKS="${LAG_CHECKS:-5}"
DRY_RUN="${DRY_RUN:-0}"
BOT_NAME="${BOT_NAME:-bot}"   # process name (comm) of the bot binary
export PATH="$PATH:$HOME/.foundry/bin"
mkdir -p "$STATE"

now() { date +%s; }
tw() { TZ="$WATCH_TZ" date "+%Y-%m-%d %H:%M"; }
get() { cat "$STATE/$1" 2>/dev/null || echo "${2:-}"; }
put() { printf '%s' "$2" > "$STATE/$1"; }
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }  # tracing colours its output when the bot runs on a terminal

send_mail() { # send_mail <subject> <body>
  local subject="[liqbot] $1" body="$2"
  if [[ "$DRY_RUN" == 1 ]]; then
    printf -- '----- DRY RUN mail: %s\n%s\n-----\n' "$subject" "$body"; return 0
  fi
  [[ -r "$CONF/mail.env" && -r "$CONF/netrc" ]] || { echo "$(tw) mail config missing in $CONF" >&2; return 1; }
  # shellcheck disable=SC1091
  source "$CONF/mail.env"
  local rcpts=() r; for r in ${MAIL_TO//,/ }; do rcpts+=(--mail-rcpt "$r"); done
  local msg; msg=$(mktemp)
  {
    printf 'From: liqbot <%s>\r\nTo: %s\r\nSubject: %s\r\nDate: %s\r\n' "$MAIL_FROM" "$(tr -s ' ,' ',' <<<"$MAIL_TO" | sed 's/,$//; s/,/, /g')" "$subject" "$(date -R)"
    printf 'Content-Type: text/plain; charset=UTF-8\r\n\r\n'
    printf '%s\n' "$body" | sed 's/$/\r/'
  } > "$msg"
  local rc=0
  curl -sS --max-time 30 --ssl-reqd --url "smtps://smtp.gmail.com:465" --netrc-file "$CONF/netrc" \
    --mail-from "$MAIL_FROM" "${rcpts[@]}" --upload-file "$msg" || rc=$?
  rm -f "$msg"
  [[ $rc == 0 ]] && echo "$(tw) mail sent: $subject" || echo "$(tw) mail FAILED (curl $rc): $subject" >&2
  return $rc
}

# Hourly mail budget so a crash day cannot flood the inbox (or trip Gmail's limits).
mail_budget_ok() {
  local hour; hour=$(date +%Y%m%d%H)
  [[ "$(get mail_hour)" == "$hour" ]] || { put mail_hour "$hour"; put mail_count 0; }
  [[ "$(get mail_count 0)" -lt "$MAX_MAILS_PER_HOUR" ]]
}
mail_counted() { put mail_count $(( $(get mail_count 0) + 1 )); }

# Exact process name, then its own argv: a shell whose command line merely mentions the bot cannot match.
bot_running() {
  local p
  for p in $(pgrep -x "$BOT_NAME"); do
    tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q -- 'watch --live' && return 0
  done
  return 1
}

check() {
  local alerts=() t; t=$(now)

  # 1. process alive (alert once per outage, and once when it is back)
  if bot_running; then
    [[ "$(get down 0)" == 1 ]] && { alerts+=("bot process is running again"); put down 0; }
  else
    [[ "$(get down 0)" == 0 ]] && { alerts+=("BOT PROCESS IS NOT RUNNING (crashed, stopped, or WSL restarted). Restart it by hand: it needs the keystore password."); put down 1; }
  fi

  # 2. new log lines since the last check
  local size off new=""
  size=$(stat -c %s "$LOG" 2>/dev/null || echo 0)
  off=$(get log_offset 0)
  (( size < off )) && off=0                                   # log truncated / rotated
  if (( size > off )); then
    new=$(tail -c +"$((off + 1))" "$LOG" | head -c $((size - off)) | strip_ansi)
    put log_offset "$size"
  fi
  if [[ -n "$new" ]]; then
    local ev
    ev=$(grep -E 'LIVE tx SENT|LIVE tx outcome|LIVE send failed|HALTED|sending paused|cannot cover worst-case gas|balance check failed|fell behind|flashblocks stream silent' <<<"$new" | cut -c1-400 || true)
    [[ -n "$ev" ]] && alerts+=("log events:"$'\n'"$ev")
    # counters for the daily summary
    put n_blocks $(( $(get n_blocks 0) + $(grep -c 'INFO bot: block ' <<<"$new" || true) ))
    put n_plans $(( $(get n_plans 0) + $(grep -c 'INFO bot: PLAN ' <<<"$new" || true) ))
    put n_sent $(( $(get n_sent 0) + $(grep -c 'LIVE tx SENT' <<<"$new" || true) ))
    put n_liq $(( $(get n_liq 0) + $(grep -c 'LIQUIDATION observed' <<<"$new" || true) ))
    grep 'LIQUIDATION observed' <<<"$new" | cut -c1-300 >> "$STATE/liq_today" || true
    # head latency: median head_age_ms of this check's blocks
    local lag
    lag=$(grep -o 'INFO bot: block .*head_age_ms=-\?[0-9]*' <<<"$new" | grep -o 'head_age_ms=-\?[0-9]*' | cut -d= -f2 | sort -n | awk '{v[NR]=$1} END{if (NR) print v[int((NR+1)/2)]}' || true)
    if [[ -n "$lag" ]]; then
      echo "$lag" >> "$STATE/lag_today"
      if (( lag > HEAD_LAG_MS )); then
        put lag_n $(( $(get lag_n 0) + 1 ))
        if (( $(get lag_n 0) >= LAG_CHECKS )) && [[ "$(get lagging 0)" == 0 ]]; then
          alerts+=("blocks reach the bot late: median head_age_ms ${lag} ms (> ${HEAD_LAG_MS}) for ${LAG_CHECKS}+ checks. WebSocket heads probably dead (HTTP polling only). Restart the bot if it lasts.")
          put lagging 1
        fi
      else
        [[ "$(get lagging 0)" == 1 ]] && alerts+=("head latency back to normal (median head_age_ms ${lag} ms)")
        put lag_n 0; put lagging 0
      fi
    fi
  fi

  # 3. stale: no block processed recently (only meaningful while the process runs)
  if bot_running; then
    local last ts age
    last=$(tac "$LOG" 2>/dev/null | strip_ansi | grep -m1 'INFO bot: block ' | cut -c1-19)
    if [[ -n "$last" ]]; then
      ts=$(date -u -d "${last/T/ } UTC" +%s 2>/dev/null || echo "$t"); age=$(( t - ts ))
      if (( age > STALE_SECS )); then
        [[ "$(get stale 0)" == 0 ]] && { alerts+=("no block processed for ${age}s (RPC down? stuck?)"); put stale 1; }
      else
        [[ "$(get stale 0)" == 1 ]] && { alerts+=("blocks are being processed again"); put stale 0; }
      fi
    fi
  fi

  # 4. hot wallet balance, hourly
  if (( t - $(get bal_at 0) >= 3600 )); then
    put bal_at "$t"
    local bal; bal=$(cast balance "$HOT" --ether --rpc-url "$RPC" 2>/dev/null || echo "")
    if [[ -n "$bal" ]]; then
      put hot_bal "$bal"
      if awk -v b="$bal" -v m="$MIN_HOT_ETH" 'BEGIN{exit !(b < m)}'; then
        [[ "$(get low 0)" == 0 ]] && { alerts+=("hot wallet $HOT has $bal ETH (< $MIN_HOT_ETH): top it up on Base"); put low 1; }
      else
        put low 0
      fi
    fi
  fi

  # 5. send the bundled alerts
  if (( ${#alerts[@]} )); then
    local body; body="$(tw) ($WATCH_TZ)"$'\n\n'"$(printf '%s\n\n' "${alerts[@]}")"$'\n'"last log lines:"$'\n'"$(tail -n 8 "$LOG" 2>/dev/null | strip_ansi | cut -c1-300)"
    if mail_budget_ok; then
      send_mail "ALERT: $(head -c 60 <<<"${alerts[0]}" | head -1)" "$body" && mail_counted
    else
      echo "$(tw) mail budget exhausted this hour; alert kept in $STATE/unsent.log" >&2
      printf '%s\n\n' "$body" >> "$STATE/unsent.log"
    fi
  fi

  # 6. daily summary
  local today; today=$(TZ="$WATCH_TZ" date +%F)
  if [[ "$(TZ="$WATCH_TZ" date +%H)" == "$SUMMARY_HOUR" && "$(get summary_day)" != "$today" ]]; then
    put summary_day "$today"
    local body
    body="Daily summary $(tw) ($WATCH_TZ), since the previous summary:
  bot running        : $(bot_running && echo yes || echo NO)
  blocks processed   : $(get n_blocks 0)
  plans simulated    : $(get n_plans 0)
  our txs sent       : $(get n_sent 0)
  liquidations seen  : $(get n_liq 0)   (by anyone, in our markets)
  hot wallet ETH     : $(get hot_bal ?)
  kill switch        : $([[ -e "$REPO/bot/data/KILL" ]] && echo 'PRESENT (sending disabled)' || echo absent)
  head_age_ms median : $(sort -n "$STATE/lag_today" 2>/dev/null | awk '{v[NR]=$1} END{if (NR) printf "%d ms (p90 %d ms, healthy WS ~ -500)", v[int((NR+1)/2)], v[int(NR*0.9+0.5)]; else print "?"}')

$( [[ -s "$STATE/liq_today" ]] && { echo 'liquidations observed:'; tail -n 20 "$STATE/liq_today"; } )
$( [[ -s "$STATE/unsent.log" ]] && echo "note: some alerts were not mailed (hourly budget); see $STATE/unsent.log" )"
    send_mail "daily summary $today" "$body" && mail_counted
    put n_blocks 0; put n_plans 0; put n_sent 0; put n_liq 0; : > "$STATE/liq_today"; : > "$STATE/lag_today"
  fi
}

case "${1:-}" in
  --test) send_mail "test mail" "If you can read this, the watchdog can reach you. $(tw) ($WATCH_TZ)"; exit $? ;;
  --once) check; exit 0 ;;
  "") ;;
  *) echo "usage: $0 [--once|--test]" >&2; exit 2 ;;
esac

# First run: start reading the log from its current end (do not mail old history).
[[ -e "$STATE/log_offset" ]] || put log_offset "$(stat -c %s "$LOG" 2>/dev/null || echo 0)"
echo "$(tw) watchdog started: log=$LOG every ${CHECK_SECS}s"
while true; do
  check
  sleep "$CHECK_SECS"
done
