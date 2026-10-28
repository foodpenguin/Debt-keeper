#!/usr/bin/env bash
# Deploys MorphoLiquidator on Base with the HOT keystore and hands ownership to the COLD wallet.
# Run it YOURSELF in your own terminal (it signs mainnet transactions). The cold wallet's key is
# never needed: at the end you send one tx from the cold wallet's own app (data 0x79ba5097).
#
#   ./scripts/deploy_liquidator.sh <HOT address> <COLD address>
#   LIQ=0x... ./scripts/deploy_liquidator.sh <HOT> <COLD>   # resume on an already deployed contract
#
# Steps (each verified on-chain before the next; on resume, steps already done are skipped):
#   1. deploy  (owner = HOT for now, executor = HOT, profitReceiver = COLD, immutable)
#   2. approveMorpho(USDC, true)          standing allowance, checked by the bot's pre-flight
#   3. setAllowedTarget(Kyber router)     only needed for router.mode = "best" / "kyber"
#   4. transferOwnership(COLD)            COLD must then call acceptOwnership()
#
# Public RPCs are load-balanced and a node may lag a block or two behind the one that mined our tx,
# so every check retries for a while and nonces are tracked locally (passed with --nonce).
#
# The keystore password is read once (hidden) into a mode-600 file on /dev/shm (RAM, never on disk),
# passed as --password-file (so never in argv or on screen) and deleted on exit, error or Ctrl+C.
set -euo pipefail

HOT="${1:?usage: $0 <HOT address> <COLD address>}"
COLD="${2:?usage: $0 <HOT address> <COLD address>}"
ACCOUNT="${ACCOUNT:-bot-hot}"
RPC="${RPC:-https://mainnet.base.org}"
LIQ="${LIQ:-}"
MORPHO=0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
USDC=0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913
KYBER=0x6131B5fae19EA4f9D964eAc0408E4408b66337b5
MAX_UINT=115792089237316195423570985008687907853269984665640564039457584007913129639935

export PATH="$PATH:$HOME/.foundry/bin"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
say() { printf '\n==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
lower() { tr '[:upper:]' '[:lower:]' <<<"$1"; }
same() { [[ "$(lower "$1")" == "$(lower "$2")" ]]; }
# cast send/forge print a receipt; require status 1.
sent_ok() { grep -qE '^status +1' <<<"$1"; }
# Read a getter, first word only ("123 [1.2e2]" -> "123"); empty on error (e.g. lagging node).
rd() { cast call "$1" "$2" "${@:3}" --rpc-url "$RPC" 2>/dev/null | cut -d' ' -f1 || true; }
# wait_eq <expected> <contract> <sig> [args...]: retry ~30 s until the getter returns <expected>.
wait_eq() {
  local want="$1" got=""; shift
  for _ in $(seq 1 15); do
    got=$(rd "$@"); same "$got" "$want" && return 0
    sleep 2
  done
  printf 'last read of %s: %s\n' "$2" "${got:-<error>}" >&2
  return 1
}
send() { # send <description> <cast send args...>
  local desc="$1" r; shift
  r=$(cast send "$@" --nonce "$NONCE" --rpc-url "$RPC" --account "$ACCOUNT" --password-file "$PWF") || die "$desc: cast send failed"
  sent_ok "$r" || die "$desc reverted: $(grep -E 'transactionHash|status' <<<"$r")"
  echo "tx $(awk '/^transactionHash/{print $2}' <<<"$r")"
  NONCE=$((NONCE + 1))
}

HOT=$(cast to-check-sum-address "$HOT") || die "bad HOT address"
COLD=$(cast to-check-sum-address "$COLD") || die "bad COLD address"
same "$HOT" "$COLD" && die "HOT and COLD must differ (profits must go to the cold wallet)"
[[ -z "$LIQ" ]] || LIQ=$(cast to-check-sum-address "$LIQ") || die "bad LIQ address"

say "chain"
CHAIN=$(cast chain-id --rpc-url "$RPC")
[[ "$CHAIN" == "8453" ]] || die "RPC is chain $CHAIN, not Base (8453)"
[[ "$(cast codesize "$MORPHO" --rpc-url "$RPC")" -gt 0 ]] || die "no Morpho at $MORPHO"
[[ "$(cast codesize "$COLD" --rpc-url "$RPC")" == "0" ]] || echo "note: COLD is a contract (fine for a Safe; it must be able to call acceptOwnership)"
echo "Base mainnet OK"

if [[ -n "$LIQ" ]]; then
  say "resume on $LIQ"
  for _ in $(seq 1 15); do [[ "$(cast codesize "$LIQ" --rpc-url "$RPC" 2>/dev/null || echo 0)" -gt 0 ]] && break; sleep 2; done
  [[ "$(cast codesize "$LIQ" --rpc-url "$RPC")" -gt 0 ]] || die "no contract at $LIQ"
  wait_eq "$HOT" "$LIQ" 'executor()(address)' || die "executor != HOT"
  wait_eq "$COLD" "$LIQ" 'PROFIT_RECEIVER()(address)' || die "PROFIT_RECEIVER != COLD"
  wait_eq "$MORPHO" "$LIQ" 'MORPHO()(address)' || die "MORPHO mismatch"
  OWNER=$(rd "$LIQ" 'owner()(address)')
  if same "$OWNER" "$COLD"; then
    echo "owner is already COLD: nothing left for this script."
    echo "USDC allowance max: $( [[ "$(rd "$USDC" 'allowance(address,address)(uint256)' "$LIQ" "$MORPHO")" == "$MAX_UINT" ]] && echo yes || echo NO)"
    echo "Kyber allowlisted : $(rd "$LIQ" 'allowedTarget(address)(bool)' "$KYBER")"
    exit 0
  fi
  same "$OWNER" "$HOT" || die "owner is $OWNER (neither HOT nor COLD)"
  echo "verified: owner=HOT executor=HOT receiver=COLD"
fi

say "keystore $ACCOUNT"
PWF=$(umask 077 && mktemp /dev/shm/liqdeploy.XXXXXX)
trap 'rm -f "$PWF"' EXIT
trap 'exit 130' INT TERM
read -rsp "keystore password for $ACCOUNT: " PW; echo
printf "%s" "$PW" > "$PWF"; unset PW
KS_ADDR=$(cast wallet address --account "$ACCOUNT" --password-file "$PWF") || die "could not decrypt keystore $ACCOUNT (wrong password?)"
same "$KS_ADDR" "$HOT" || die "keystore $ACCOUNT is $KS_ADDR, not HOT $HOT"
echo "keystore address matches HOT"

BAL=$(cast balance "$HOT" --rpc-url "$RPC")
echo "HOT balance: $(cast to-unit "$BAL" ether) ETH"
# deploy ~0.000014 ETH + setup ~0.000001 ETH at 0.005 gwei base fee; keep margin for fee spikes.
NEED=30000000000000; [[ -n "$LIQ" ]] && NEED=2000000000000
[[ "$BAL" -ge "$NEED" ]] || die "HOT needs at least $(cast to-unit $NEED ether) ETH on Base (fund it from the cold wallet first)"
# "pending" counts txs already accepted by this node; a lagging node can only make it LOWER, which
# fails safely ("nonce too low") rather than double-sending.
NONCE=$(cast nonce "$HOT" --block pending --rpc-url "$RPC")
echo "HOT nonce: $NONCE"

TODO=()
if [[ -z "$LIQ" ]]; then
  TODO+=("deploy MorphoLiquidator(morpho=$MORPHO, owner=HOT, executor=HOT, profitReceiver=$COLD)")
  DO_APPROVE=1; DO_TARGET=1; DO_TRANSFER=1
else
  DO_APPROVE=1; [[ "$(rd "$USDC" 'allowance(address,address)(uint256)' "$LIQ" "$MORPHO")" == "$MAX_UINT" ]] && DO_APPROVE=0
  DO_TARGET=1; [[ "$(rd "$LIQ" 'allowedTarget(address)(bool)' "$KYBER")" == "true" ]] && DO_TARGET=0
  DO_TRANSFER=1; same "$(rd "$LIQ" 'pendingOwner()(address)')" "$COLD" && DO_TRANSFER=0
fi
[[ "$DO_APPROVE" == 1 ]] && TODO+=("approveMorpho(USDC, true)")
[[ "$DO_TARGET" == 1 ]] && TODO+=("setAllowedTarget(Kyber, true)")
[[ "$DO_TRANSFER" == 1 ]] && TODO+=("transferOwnership($COLD)")
if [[ ${#TODO[@]} -gt 0 ]]; then
  printf '\nAbout to send %d MAINNET transaction(s) from HOT %s:\n' "${#TODO[@]}" "$HOT"
  printf '  - %s\n' "${TODO[@]}"
  [[ -z "$LIQ" ]] && echo "profitReceiver can never be changed after deployment."
  read -rp "type 'deploy' to continue: " OK
  [[ "$OK" == "deploy" ]] || die "aborted"
else
  echo "all HOT-side steps already done (pendingOwner = COLD)"
fi

if [[ -z "$LIQ" ]]; then
  say "build + deploy"
  (cd "$REPO/contracts" && forge build >/dev/null 2>&1) || die "forge build failed (run it in contracts/ to see why)"
  # The address is fixed by (HOT, nonce): save it BEFORE broadcasting, so a failure after the tx
  # went out (e.g. a receipt timeout) can be resumed instead of deploying a second contract.
  EXPECT=$(cast compute-address "$HOT" --nonce "$NONCE" | awk '{print $NF}')
  mkdir -p "$REPO/bot/data"
  echo "$EXPECT" > "$REPO/bot/data/liquidator_address.txt"
  echo "deploying to $EXPECT (if anything below fails, resume with: LIQ=$EXPECT $0 $HOT $COLD)"
  OUT=$(cd "$REPO/contracts" && forge create src/MorphoLiquidator.sol:MorphoLiquidator --rpc-url "$RPC" --account "$ACCOUNT" --password-file "$PWF" --broadcast \
    --nonce "$NONCE" --constructor-args "$MORPHO" "$HOT" "$HOT" "$COLD") || die "forge create failed; the tx may still have been sent: check $EXPECT, then resume with LIQ=$EXPECT"
  echo "$OUT" | grep -E 'Deployer|Deployed to|Transaction hash'
  LIQ=$(awk '/Deployed to:/{print $3}' <<<"$OUT")
  [[ -n "$LIQ" ]] || die "no 'Deployed to' in forge output; check $EXPECT, then resume with LIQ=$EXPECT"
  same "$LIQ" "$EXPECT" || die "deployed to $LIQ, expected $EXPECT (nonce moved?)"
  NONCE=$((NONCE + 1))
  wait_eq "$HOT" "$LIQ" 'owner()(address)' || die "owner != HOT"
  wait_eq "$HOT" "$LIQ" 'executor()(address)' || die "executor != HOT"
  wait_eq "$COLD" "$LIQ" 'PROFIT_RECEIVER()(address)' || die "PROFIT_RECEIVER != COLD"
  wait_eq "$MORPHO" "$LIQ" 'MORPHO()(address)' || die "MORPHO mismatch"
  echo "verified: owner=HOT executor=HOT receiver=COLD"
else
  mkdir -p "$REPO/bot/data"
  echo "$LIQ" > "$REPO/bot/data/liquidator_address.txt"
fi

if [[ "$DO_APPROVE" == 1 ]]; then
  say "approveMorpho(USDC)"
  send approveMorpho "$LIQ" 'approveMorpho(address,bool)' "$USDC" true
  wait_eq "$MAX_UINT" "$USDC" 'allowance(address,address)(uint256)' "$LIQ" "$MORPHO" || die "USDC allowance not max"
  echo "verified: USDC -> Morpho allowance = max"
fi

if [[ "$DO_TARGET" == 1 ]]; then
  say "setAllowedTarget(Kyber)"
  send setAllowedTarget "$LIQ" 'setAllowedTarget(address,bool)' "$KYBER" true
  wait_eq true "$LIQ" 'allowedTarget(address)(bool)' "$KYBER" || die "Kyber not allowlisted"
  echo "verified: Kyber router allowlisted"
fi

if [[ "$DO_TRANSFER" == 1 ]]; then
  say "transferOwnership(COLD)"
  send transferOwnership "$LIQ" 'transferOwnership(address)' "$COLD"
  wait_eq "$COLD" "$LIQ" 'pendingOwner()(address)' || die "pendingOwner != COLD"
  echo "verified: pendingOwner = COLD"
fi

rm -f "$PWF"
echo "HOT balance left: $(cast balance "$HOT" --ether --rpc-url "$RPC") ETH"
cat <<EOF

==> DONE. Liquidator: $LIQ   (also saved to bot/data/liquidator_address.txt)

Last step, in the COLD wallet's app (MetaMask/Rabby), network Base:
  send 0 ETH to   $LIQ
  hex data        0x79ba5097      (acceptOwnership)

Then check (should print $COLD):
  cast call $LIQ 'owner()(address)' --rpc-url $RPC
EOF
