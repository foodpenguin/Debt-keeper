# Architecture

[繁體中文](architecture.zh-TW.md)

This framework builds liquidation bots for lending protocols. The chain-agnostic core (position
tracking, planning, simulation, sending) is written against three traits; a protocol, a liquidation
contract and a swap aggregator plug into them. **Morpho Blue on Base** is the first, complete
implementation and the reference for new ones.

## One liquidation, end to end

```
new block (WS newHeads / HTTP poll / Flashblocks signal)
  -> Engine: read the touched positions + one snapshot per market, recompute health factors
  -> candidates with HF < 1, sized by the protocol (max seizable collateral)
  -> Planner: exit route (self-built V3 router and/or an aggregator), sized under a gas budget
  -> LiquidationVenue: encode the exact tx, simulate it (eth_simulateV1) at the next block
  -> risk gate: min net profit, priority fee, daily gas budget, kill switch
  -> shadow mode: journal only | live mode: sign + broadcast, track the receipt
```

## The traits

| Trait | File | Responsibility | First impl |
|---|---|---|---|
| `LendingProtocol` | `bot/src/protocol.rs` | Load markets and positions, turn events into "touched" positions, read per-block snapshots, evaluate health and liquidation size | `morpho::Morpho` |
| `LiquidationVenue<L>` | `bot/src/venue.rs` | Encode a liquidation (with an exit route) into a sendable payload, simulate it, probe gas | `sim::Simulator` (MorphoLiquidator) |
| `Dex` | `bot/src/dex.rs` | External aggregator: quote under a gas budget, build the swap call | `router::Kyber` |

### `LendingProtocol`

Associated types: `Market`, `Position`, `Snapshot`, `Health`, `Quote`, `Liquidation`. Methods:

- `load_markets` — load and validate the configured markets (Morpho checks
  `keccak256(params) == id`).
- `borrower_job` — a self-contained future fetching borrower lists from the protocol's indexer,
  spawned so the block loop keeps running.
- `load_positions` + `has_debt` — batched position reads; debt-free positions are dropped.
- `touched` — the position-changing events and liquidations in a block range.
- `snapshots(prev)` — one snapshot per market per block; `prev` lets an implementation save a round
  trip (Morpho reuses the previous block's state for the IRM rate).
- `evaluate` — pure: HF, debt, max seizable collateral, liquidation quote, bonus.
- Planning hooks: `exit_tokens`, `loan_decimals`, `repay_for_seize`, `collateral_value`.
- `eth_usd` (optional) — ETH/USD from tracked markets, for gas accounting.

Every method keeps the batched, incremental shape of the hot path: one call for all touched
positions, one for all snapshots.

### `Engine<P, L>` (`bot/src/engine.rs`)

Owns the loop state and never names a protocol: tracked positions per market, market index,
incremental refresh (only what events touched is re-read), dust filtering, HF ordering, and the
indexer's HF for validation. `engine::view` builds a `PositionView<L>` (market, user, HF, debt,
max seize, quote).

### `Planner<L, V, A>` (`bot/src/planner.rs`)

Turns one liquidatable position (or a bundle of positions in one market) into a simulated plan.

- **Exit routers.** `direct` = the self-built V3 router; `aggregator` = the `Dex` (alias `kyber`);
  `best` = both in parallel, the higher simulated net wins and the loser is kept for comparison.
- **Sizing.** The aggregator path quotes several fractions of the max seize in parallel and keeps
  the best gross margin; while the route is too gas-heavy it retries with smaller sizes only. The
  direct path allocates over its grid under the same gas budget (`max_gas` minus a fixed
  liquidation overhead).
- **Sanity checks** on the simulation (profit vs quote, block the node really simulated).
- `quote_bundle` values a whole market's liquidatable positions as one exit (used by the Atlas
  valuation, where one solver tx liquidates them all).

### `LiquidationVenue<L>` (`bot/src/venue.rs`)

`target()` (the contract the tx goes to, also the swap sender/recipient), `encode(market, user,
seize, route, min_profit, deadline)` -> `Payload { to, calldata, min_profit slot }`, `simulate`,
`probe_gas`. The `min_profit` slot tells the executor where to patch the final minProfit at send
time. An `ExitRoute` is either `Packed(bytes)` (the self-built router's compact route) or
`Call { target, calldata }` (an aggregator swap).

`sim::Simulator` implements it for MorphoLiquidator: a packed route goes through the compact
fallback entry, an external call becomes the single swap step of the ABI `liquidate`. Without a
deployed address the liquidator's runtime code is injected by state override at a placeholder
address, so the bot can plan before anything is deployed.

### `Dex` (`bot/src/dex.rs`)

`quote_within(token_in, token_out, amount_in, gas_budget)`, `amount_out`, `swap_gas`,
`build(quote, sender, recipient, slippage_bps) -> SwapCall { target, calldata }`. The planner never
trusts the returned target: it must be in `exec.allowed_targets`. `router::Kyber` uses a gas ladder
(two quotes at different gas prices, the best route that fits the budget) and scales Kyber's gas
estimate by a safety factor.

## Exit routing: the self-built V3 router (`bot/src/v3router.rs`)

- Routes collateral -> loan token through direct legs and connector legs (collateral -> WETH ->
  loan token), on Uniswap V3-style pools of the factories in `[[router.factories]]` (fee-tier or
  tick-spacing factories). Factory addresses live in config, never in code.
- **Quoting:** `contracts/src/sim/GridQuoter.sol` is never deployed; it is injected by state
  override and quotes a grid of sizes for every leg in one `eth_call` (the Uniswap-quoter revert
  trick), with a per-hop gas cap so a pathological pool cannot blow the call's gas limit.
- **Allocation:** greedy on marginal output over a concave combined curve (a shared connector sink
  is not double-counted), compared against single-leg solutions because each extra leg costs gas;
  `allocate_within` enforces a gas budget. Leg gas is priced at `planning_priority_fee_wei`.
- **Execution:** the packed route calls pools directly; pools are paid inside their own swap
  callback (only the pool currently swapping, once, at most its input).

## On-chain: `contracts/src/MorphoLiquidator.sol`

- `liquidate(params, borrower, seizedAssets, repaidShares, swaps, minProfit, deadlineBlock)` (ABI
  entry, aggregator swaps), `liquidateRoute` and a packed `fallback` (direct route, compact
  calldata). Only `executor` may call them.
- Morpho transfers the collateral, then calls `onMorphoLiquidate(repaidAssets, data)`, where the
  swaps run; Morpho then pulls the repayment (standing allowance set by `approveMorpho`).
- After the callback the contract checks the loan-token gain is at least `minProfit`, sends profit
  to the immutable `PROFIT_RECEIVER` and holds nothing at rest.
- Admin: `setExecutor`, `setAllowedTarget`, `approveMorpho`, `pause` (owner or executor) /
  `unpause` (owner), two-step `transferOwnership` / `acceptOwnership`, `rescue`.

### Security model

| Threat | Defence | Tests |
|---|---|---|
| Someone else calls the entry | executor-only entries; owner-only admin; two-step ownership | `test_onlyOwnerAdmin`, `test_twoStepOwnership`, `test_revert_notExecutor` |
| Forged callback | `msg.sender == MORPHO` and a liquidation of our own in flight (transient flag) | `test_callbackOutsideFlightBlocked`, `test_forgedCallbackDuringFlightBlocked`, `test_revert_callbackFromOutsider` |
| Re-entrancy from a swap target | a target is not the executor, so it cannot re-enter | `test_reentrancyFromRouterBlocked` |
| Poisoned route approving/transferring tokens | swap targets must be owner-allowlisted; token addresses are never allowlisted | `test_tokenApproveTrickBlocked`, `test_targetNotAllowlisted` |
| Router stealing collateral / pulling more than allowed | exact allowances reset to zero; Morpho reverts the whole tx if unpaid | `test_thiefRouterGainsNothing`, `test_greedyRouterCannotExceedAllowance` |
| Fee-on-transfer / rebasing collateral | the callback checks the collateral actually received | `test_feeOnTransferCollateralRejected` |
| Not enough profit | `minProfit` on the loan-token balance delta (fuzzed) | `test_minProfitEnforced`, `testFuzz_profitAtLeastMin` |
| Fake pool in a direct route | pools are only called for `swap`/`token0`/`token1` and paid inside their own callback, at most the hop input; nothing is held at rest | `test_route_poolAskingMoreThanInputGetsNothing`, `test_route_doubleCallbackRejected`, `test_swapCallbackFromStrangerRejected` |
| Route not spending all seized collateral | explicit source legs must sum exactly to `seizedAssets` (`RouteSpendMismatch`) | `test_route_mustSpendExactlySeized` |
| Pool reporting positive output | every hop's output must be a negative delta (`BadRoute`) | `test_route_positiveOutputRejected` |
| Malformed packed calldata | strict decoding, no selector collision with the ABI entries | `test_packed_malformedRejected`, `test_packed_noSelectorCollision`, `test_packedEntry_equalsAbiEntry` |
| Emergency | executor may `pause`, only the owner may resume | `test_pauseKillSwitch` |

Off-chain defences: aggregator targets must be in `allowed_targets`; market ids are re-derived from
params at start; implausible simulations (profit far from the quote) are rejected; RPC errors are
rewritten so URLs with API keys never reach logs (`errors_never_contain_urls`); the risk gate
(`bot/src/risk.rs`) applies the kill-switch file, minimum net profit, a priority fee proportional to
net profit with a hard cap, and a daily budget for gas burned by failed txs.

### Gas

Gas benchmarks live in `contracts/test/MorphoLiquidator.gas.fork.t.sol`
(`forge test --isolate --match-contract GasBench -vv`). The contract is gas-optimised without
dropping any check: packed entry (much smaller calldata), executor and paused in one slot,
immutable profit receiver, no redundant events, no balance pre-reads (nothing is held at rest),
standing Morpho allowance, `via_ir` with memory-safe assembly. On an L2 with a low base fee, gas
matters mostly for priority-fee competition: ordering is by priority fee per gas, so a cheaper tx
can bid more per gas for the same budget.

### Deployment checklist (operator)

1. `scripts/deploy_liquidator.sh <HOT> <COLD>`: deploys with `owner = HOT` (temporarily),
   `executor = HOT`, `profitReceiver = COLD` (immutable), calls `approveMorpho` for the loan
   token(s) and allowlists the aggregator router if you use one, then starts the ownership
   transfer to COLD.
2. COLD calls `acceptOwnership()` (`scripts/accept_ownership.sh`).
3. Set `exec.liquidator` and keep `allowed_targets` in sync with the on-chain allowlist.

## Off-chain modules (`bot/src/`)

| Module | Responsibility |
|---|---|
| `config` | TOML config + RPC resolution (URLs only from `.env`) |
| `protocol`, `engine` | the protocol seam and the generic HF engine |
| `morpho/` | Morpho Blue: on-chain reads, events, math (shares/assets with virtual shares, LIF, HF, seize <-> repay), indexer API |
| `planner`, `venue`, `dex` | planning and its two seams |
| `sim` | MorphoLiquidator encoding + `eth_simulateV1` simulation (profit, gas, L1 fee) at the next block's number and timestamp |
| `router`, `v3router` | Kyber aggregator client; self-built V3 router |
| `risk` | send/skip decision and priority fee |
| `executor` | keystore signing, broadcast, nonce, failure policy (below) |
| `rpc` | multi-endpoint pool (below) |
| `heads` | new-head signal: WS `newHeads` on several endpoints, HTTP fallback |
| `flashblocks` | Base Flashblocks stream: decoding, oracle updates from raw txs |
| `oracle` | local MorphoChainlinkOracleV2 price model, verified against `price()` at start |
| `atlas` | Chainlink SVR / Fastlane Atlas auction valuation (observe only) |
| `loss` | loss analysis: every on-chain liquidation in tracked markets vs what we saw |
| `dash/`, `live` | SQLite history + static dashboard; live HF page on localhost |

### Simulation

One `eth_simulateV1` call runs the whole liquidation and returns profit and real gas; the L1 fee
is added (`getL1FeeUpperBound`). Simulations run as the **next** block (block override of number
and timestamp), because candidates are evaluated with interest accrued to the next block.
`eth_estimateGas` is not used: some node implementations ignore block overrides there. Without a
real gas figure a plan is an error, never a guess. Plans pinned to a "pending" state check which
block the node actually simulated, because pending calls can run on a stale block.

### Executor (`bot/src/executor.rs`)

- **Signing:** only from an encrypted keystore; the password comes from an env var that is removed
  from the process environment right after reading, or from a hidden prompt.
- **One tx in flight.** minProfit is patched to `sim profit x min_profit_keep`, gas limit to
  `sim gas x gas_limit_buffer`; the wallet must afford worst-case gas.
- **Broadcast:** the same raw tx goes to every endpoint plus optional `extra_broadcast`; the first
  acceptance returns, the rest finish in the background. A dedicated HTTP client keeps connections
  warm (keep-alive pings), so a send never pays DNS/TLS setup.
- **Nonce:** tracked locally, re-synced from chain on drops / nonce errors / "replacement
  underpriced" (then retried once). Bots that sign with the same wallet share a lock file per
  wallet (`exec.nonce_file`, default `~/.local/state/onchain-bots/nonce-<wallet>`), held between
  signing and broadcast.
- **Failure policy:** `max_failures_in_window` failed or dropped txs within `failure_window_secs`
  pause sending for `failure_cooldown_secs`, then it resumes.
- **Deadline:** `deadline_secs` converted to blocks from the measured block time.
- **Live mode needs all of:** `exec.mode = "live"`, `watch --live`, `exec.liquidator`,
  `exec.keystore`, and a passing pre-flight (contract exists, its executor is the keystore address,
  not paused, profit receiver is not the hot wallet, Morpho allowances set, wallet balance at least
  `min_wallet_eth`).

### RPC pool (`bot/src/rpc.rs`)

Endpoints are declared in `[[rpc]]` (`http_env` names a variable in `.env`; public endpoints may
use `http`). The pool races or fails over by priority, honours per-endpoint `skip_methods` and log
range limits, switches endpoint on "method not found", rate limits and capacity errors (with a
cooldown), and treats "block not found / header not found" from a lagging node as *lag*: try the
next endpoint without cooling the laggard down, unless it lags repeatedly. Logs only ever show the
endpoint name.

### Same-block path (`bot/src/flashblocks.rs`, `bot/src/oracle.rs`)

Base streams ~200 ms sub-blocks (Flashblocks). The bot decodes Chainlink transmissions straight
from the raw txs in the stream (OCR report -> median observation), computes the new Morpho oracle
price locally, re-evaluates only that market and plans on the snapshot block with the new price
pinned by state override. SVR feeds are handled separately: the public price only takes effect once
the matching secondary round lands, or after the feed's cutoff. `[flashblocks] send = false` keeps
this path journal-only. Note: the planned Base "Denim" upgrade removes Flashblocks; the local price
model and single-market re-evaluation stay useful with faster blocks, so the data layer is not tied
to the Flashblocks API.

### Atlas / Chainlink SVR (`bot/src/atlas.rs`)

On SVR-enabled feeds, the right to liquidate after an oracle update is auctioned through Fastlane
Atlas. The module listens for auctions, values each one with the same planner (`quote_bundle` for
the whole market, then per-position plans), and journals what we could have bid. It never
submits anything. `value-auction` replays a past auction.

### Dashboard and live page

`bot dashboard` ingests the journal and run log into SQLite (`dashboard.db_path`) and renders a
static HTML page (no server): KPIs, latency, liquidations, auction valuations, exit-path analysis
of on-chain liquidations (decoded once from tx + receipt and cached), RPC usage. `bot live` serves
a read-only page on `127.0.0.1:live_port` with the positions closest to liquidation, from the
snapshot `watch` writes every block.

## Adding a protocol

1. Create `bot/src/<protocol>/` and implement `LendingProtocol` (see `morpho/mod.rs`). Choose the
   associated types; keep reads batched; make `evaluate` pure.
2. Implement `MarketInfo` for your market type (`key()` = a stable `B256`, `name()`).
3. Start from `bot/src/testkit.rs`: `Toy` is a complete non-Morpho protocol (position =
   (debt, collateral), snapshot = price) that the engine and planner tests run on. Copy its shape
   for unit tests of your own impl.
4. Bind the application layer: today `main.rs` uses `morpho::MorphoEngine`; add the same alias for
   yours, and a config section like `[morpho]`.
5. If the protocol has no liquidate callback, the liquidation must be funded by a flash loan inside
   your contract; see "Adding a venue".

## Adding a DEX (aggregator)

Implement `dex::Dex` (see `impl Dex for Kyber` in `router.rs`): `name()` becomes `Plan::router`,
`quote_within` must respect the gas budget, `build` returns the call the venue will make.
`testkit::MockDex` (pays 2 loan units per collateral unit, routed via `0x22..`) and the
`generic_tests` in `planner.rs` show the full aggregator path, including the allowlist check.
Add the router to `exec.allowed_targets` and to the contract allowlist.

## Adding a venue (liquidation contract)

Implement `venue::LiquidationVenue<L>` for your contract: `encode` must return the exact calldata
you will send and the byte slot of `minProfit`; `simulate` must run that calldata at the next block
(see `Simulator::simulate`). `testkit::MockVenue` tags packed vs call routes and never simulates.
Write a Foundry security suite in the spirit of the table above before any deployment.

## Commands

```bash
cd contracts && forge test -vv                      # contract tests (fork tests use BASE_RPC_URL)
cd bot && cp config.example.toml config.local.toml
cargo run --release -- -c config.local.toml scan --top 5     # one-shot scan + HF validation
cargo run --release -- -c config.local.toml watch            # shadow mode, journal only
cargo run --release -- -c config.local.toml report           # loss analysis over the journal
cargo run --release -- -c config.local.toml dashboard        # one dashboard pass
cargo run --release -- -c config.local.toml watch --from-block N   # replay (archive RPC)
cargo run --release -- -c config.local.toml plan --market <name> --user 0x... --price-drop-bps 300
cargo test                                          # unit tests
cargo test e2e -- --ignored --nocapture             # live path on a local anvil fork
```
