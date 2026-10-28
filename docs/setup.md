# Setup

[繁體中文](setup.zh-TW.md)

Works on Linux and on WSL2 (Ubuntu 22.04 / 24.04). Nothing in this guide sends a mainnet
transaction; live mode is a separate, explicit step at the end that only you start.

## 1. Tools

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git curl ca-certificates chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   # Rust (edition 2024: stable >= 1.85)
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup  # forge, cast, anvil
```

`chrony` keeps the clock accurate (latency and block-time measurements depend on it). TLS is
rustls and SQLite is bundled, so no OpenSSL or system SQLite is needed.

## 2. Code

```bash
git clone --recurse-submodules <repo-url> liquidation-framework
cd liquidation-framework
git submodule update --init --recursive    # if you cloned without --recurse-submodules (forge-std)
cd contracts && forge build && cd ..        # also produces the artifacts the bot simulates with
                                            # (MorphoLiquidator, FlashLiquidator, AtlasLiquidationSolver, AtlasStub, GridQuoter)
```

## 3. RPC endpoints (`.env`)

RPC URLs carry API keys, so they live only in `.env` at the repo root (gitignored):

```bash
cp .env.example .env    # then put your real URLs in it
```

The config never contains a keyed URL: each `[[rpc]]` entry names the variable with `http_env`
(public endpoints may use `http` directly). Logs and errors only ever show the endpoint `name`.
A hosted RPC with archive state is enough; you do not need your own node. Check which methods each
endpoint supports (`eth_simulateV1`, large `eth_getLogs` ranges, WebSocket) and declare the gaps
with `skip_methods` / `max_log_blocks`.

## 4. Config

```bash
cd bot
cp config.example.toml config.local.toml   # config.local.toml is gitignored
```

Review at least: `[[rpc]]`, `[morpho]` (address, indexer API, the `[[morpho.markets]]` you want to
track), `[exec]` (keep `mode = "shadow"` for now), `[router]` (`mode` and your DEX factories).
Pick markets deliberately: an allowlist that excludes markets with unknown or fake tokens is a
safety control, not just a filter. The bot only accepts markets whose loan token it can price in
USD (USD stablecoins `USDC`, `USDbC`, `USDT`, `DAI`, `USDS`, and `WETH`); it refuses to start with
any other.

Other keys worth knowing:

- `exec.max_inflight` (default 3): liquidation txs in flight at once, with consecutive nonces; 1 =
  one at a time.
- `exec.kill_switch_file`: a relative path is resolved from the config file's directory, not from
  the directory you start the bot in.
- `accrual_lookahead_secs`: leave it unset to follow the measured block time (rounded up, 1 to 2 s).
- The config is checked at start: `exec.venue = "flash"` together with `router.mode = "direct"`, or
  with `[flashblocks] enabled = true` and `flashblocks.router = "direct"`, is refused (that venue
  takes aggregator exits only).

Liquidation contract (`[exec] venue`):

- `"callback"` (default): MorphoLiquidator, funded by Morpho's liquidate callback.
- `"flash"`: FlashLiquidator, which flash-borrows the repayment first (the path for protocols
  without a callback). `flash_funding = "morpho"` (Morpho flash loan) or `"v4"` (Uniswap V4 flash
  accounting, needs `pool_manager`); `flash_headroom_bps` is borrowed on top of the planned
  repayment; `flash_liquidator` = the deployed contract (unset: simulated with injected bytecode).
  Only aggregator exits are used with this venue (it has no packed-route entry).

`[atlas.solver] simulate = true` additionally simulates each valued SVR auction's top position as
an Atlas solver operation (journal only, nothing is signed or sent); see `docs/architecture.md`.

## 5. Build and run in shadow mode

```bash
cargo build --release
./target/release/bot -c config.local.toml scan --top 5   # one-shot: load positions, validate HF against the indexer
./target/release/bot -c config.local.toml watch          # shadow mode: plans + simulations are journaled, nothing is sent
```

Shadow mode writes `data/journal.jsonl`. Run it for a while and read it (`report`, `dashboard`)
before considering live mode. RPC budget: the block path reads the touched logs and one snapshot
per market every block. Deadlines and the interest lookahead adapt to the measured block time, but
with 200 ms blocks (Base's planned "Denim" upgrade) that work happens ~10x as often, so RPC usage
grows ~10x: check your plan's quota. `bot dashboard` (or `scripts/dashboard_loop.sh`, every minute) renders
`data/dashboard.html`; its times are UTC unless `BOT_DASH_TZ_HOURS` is set (e.g. `8` or `-5.5`).

## 6. Tests

```bash
cargo test                                      # unit tests (no network)
cargo test e2e -- --ignored --nocapture         # LOCAL anvil fork (needs anvil; BASE_FORK_URL optional): live send path,
                                                # several txs in flight, stuck-tx cancel, flash-funded
                                                # venue, Atlas solver op
cd ../contracts
forge test --no-match-contract Fork -vv         # tests that need no RPC
BASE_RPC_URL=<your url> forge test -j 1 -vv     # fork tests; -j 1 keeps free RPC plans under their rate limit
```

Fork tests default to a public endpoint when `BASE_RPC_URL` is unset. They are pinned to a fixed
block so results do not drift with the chain, so the endpoint must serve archive state
(`FORK_BLOCK=<n>` moves the pin, `FORK_BLOCK=0` = latest). Pass a keyed URL through the environment
only; never write it into a file that is committed.

## 7. Keys

The hot wallet only pays gas; profit goes to a separate receiver (a cold wallet) fixed in the
contract. Keys live only in an encrypted Foundry keystore:

```bash
cast wallet import bot-hot --interactive   # prompts for the private key and a password
```

Never put a private key or keystore password in code, config, `.env`, shell history or logs. The
bot reads the password from the env var named by `exec.keystore_password_env` (removed from the
process right after reading) or prompts for it. Keep only a small amount of ETH for gas on the hot
wallet.

## 8. Going live (operator only)

1. Deploy and configure the contract: `scripts/deploy_liquidator.sh <HOT> <COLD>`, then accept
   ownership from COLD (`scripts/accept_ownership.sh`). Both sign mainnet txs: run them yourself.
2. Before the first live run, set and double-check: `exec.max_priority_fee_wei` (priority-fee cap),
   `exec.daily_gas_budget_usd`, the market allowlist, `exec.kill_switch_file`, `exec.allowed_targets`
   (must match the contract allowlist), `exec.liquidator`, `exec.keystore`.
3. Set `exec.mode = "live"` and start `watch --live` yourself. The pre-flight check refuses to start
   if the contract, executor, allowances or balance are off. With `exec.venue = "flash"` it checks
   `exec.flash_liquidator` instead, that Morpho is one of its `allowedProtocol`s, and that its
   `MORPHO()` / `POOL_MANAGER()` match the configured `flash_funding`. There is no deploy script for
   FlashLiquidator: deploy it yourself with constructor
   `(morpho, poolManager, owner, executor, profitReceiver)`, then `setAllowedProtocol` the
   protocol's liquidation target and `setAllowedTarget` your aggregator router(s).
4. Emergency stop: create the kill-switch file (every decision becomes "skip"), or call `pause()` on
   the contract from the owner or executor.

If several bots sign with the same hot wallet, leave `exec.nonce_file` empty so they share the
per-wallet nonce lock (`~/.local/state/onchain-bots/nonce-<wallet>`).
