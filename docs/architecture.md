# Architecture

[繁體中文](architecture.zh-TW.md)

This framework builds liquidation bots for lending protocols. The chain-agnostic core (position
tracking, planning, simulation, sending) is written against three traits; a protocol, a liquidation
contract and a swap aggregator plug into them. **Morpho Blue on Base** is the first, complete
implementation and the reference for new ones.

## Scope: what is generic today

The crate is a library (`liquidation_framework`, `bot/src/lib.rs`, with the watch loop in
`runner.rs`) plus a thin Morpho bot binary (`bot/src/main.rs`, its CLI). The chain plumbing (RPC
pool, heads, Flashblocks stream, `eth_simulateV1`
simulation, executor and its task, send gates, bidding, the shared config sections) is the
`onchain-core` crate (repository bot-onchain-core, a git dependency pinned to a tag), shared with
Arb-seeker and re-exported under its usual module names (`liquidation_framework::executor`,
`::rpc`, ...).

Generic (written against the traits, no protocol named): the HF engine (`engine.rs`), the planner
(`planner.rs`), the venue / DEX / funding seams (`venue.rs`, `dex.rs`, `funding.rs`) and their
venues (`sim.rs`, `flash_venue.rs`, `atlas_venue.rs`), the risk gate (`risk.rs`) and everything
from `onchain-core`. Still Morpho-bound application code: the `runner.rs` watch loop (block path,
Atlas path, send path, pre-flight), the oracle price model (`oracle.rs`, MorphoChainlinkOracleV2),
the Atlas valuation, the dashboard and the loss analysis. A new protocol reuses the generic core
as it is, but today it needs its own application loop (start from `runner.rs`).

## One liquidation, end to end

```
new block (WS newHeads / HTTP poll / Flashblocks signal)
  -> Engine: read the touched positions + one snapshot per market, recompute health factors
  -> candidates with HF < 1, sized by the protocol (max seizable collateral)
  -> Planner: exit route (self-built V3 router and/or an aggregator), sized under a gas budget
  -> LiquidationVenue: encode the exact tx, simulate it (eth_simulateV1) at the next block,
     priced at the fee a non-bidding send would pay
  -> risk gate: min net profit (USD), priority fee against the observed competition, daily gas
     budget, kill switch
  -> a bid above the base priority is simulated again at its own priority before it is kept
  -> shadow mode: journal only | live mode: a message to the executor task, which signs and
     broadcasts (several txs may be in flight, one per market) and polls the receipts on every block
```

## The traits

| Trait | File | Responsibility | First impl |
|---|---|---|---|
| `LendingProtocol` | `bot/src/protocol.rs` | Load markets and positions, turn events into "touched" positions, read per-block snapshots, evaluate health and the liquidation options (pairs) | `morpho::Morpho`; `testkit::ToyPool` (account-level, tests) |
| `LiquidationVenue<L>` | `bot/src/venue.rs` | Encode a liquidation (with an exit route) into a sendable payload, simulate it, probe gas | `sim::Simulator` (MorphoLiquidator), `flash_venue::FlashLiquidatorVenue` (FlashLiquidator), `atlas_venue::AtlasBid` (AtlasLiquidationSolver, simulation only) |
| `Dex` | `bot/src/dex.rs` | External aggregator: quote under a gas budget, build the swap call | `router::Kyber` |
| `DirectLiquidation` | `bot/src/protocol.rs` | The protocol's own liquidation call for a caller that holds the repayment up front (no callback) | `morpho::Morpho` |
| `FlashLoan` / `FlashAccounting` | `bot/src/funding.rs` | Where a flash-funded liquidation borrows the repayment | `MorphoBlue` / `UniswapV4` |
| `FlashSource` | `bot/src/funding.rs` | What FlashLiquidator needs from a flash source: `name`, `holder`, `fee`, `mode` (its `funding` selector) | `MorphoBlue` (1), `UniswapV4` (2) |

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
- `evaluate` — pure: HF, debt, the account's main `Pair` (for logs while healthy), and its
  `LiquidationOption`s: one per (collateral, debt) `Pair` it can be liquidated on, each with its
  max seizable collateral, quote and bonus, best first. An isolated market (Morpho) has one pair
  and so at most one option.
- Planning hooks, all for a given `Pair` (the exit sells `pair.collateral` for `pair.debt`; profits
  are in the debt token): `token_decimals(market, token)`, `repay_for_seize(market, snap, pair,
  seize)`, `collateral_value(market, snap, pair, amount)` (in debt-token units at the protocol's
  price), `pairs(market)` (every pair the exit routes must cover), and `loan_usd(market, snap,
  debt_token, eth_usd)`: USD per debt token. Profits, gas trade-offs and bids are compared in USD;
  `None` means no price model and the liquidation is refused rather than mispriced. Morpho uses
  `morpho::loan_usd_model`: USD stablecoins (`USDC`, `USDbC`, `USDT`, `DAI`, `USDS`) at 1, `WETH` at
  the ETH price, anything else `None`. The binary also refuses such markets at start.
- `eth_usd` (optional) — ETH/USD from tracked markets, for gas accounting.

**Isolated vs account-level protocols.** The engine tracks (market, account) positions. For an
isolated protocol (Morpho Blue) a market is one collateral / one loan token. For an account-level
protocol (an Aave-style pool, a Compound-style comptroller) a "market" is the pool and a position
is the whole account across its reserves: `evaluate` computes health over all of them and returns
an option per (collateral, debt) pair. `PositionView` carries the best option (`pair`,
`max_seize`, `quote`, `bonus`).
`testkit::ToyPool` (two collateral and two debt reserves, close factor 50%, 5% bonus) is the
worked example: `account_level_tests` in `planner.rs` checks that the best pair is liquidated and
that the repayment, profit token and USD price all follow it.

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
- **Fee-priced simulation.** Every plan is simulated (and every direct-router grid quoted) at the fee
  a non-bidding send pays: `Planner::sim_fee` = max fee 2 x base fee + `priority_fee_wei`, priority
  `priority_fee_wei`. Venues that read `tx.gasprice` (priority-fee taxes, dynamic fees) answer what
  the real tx would get.
- **USD accounting.** `Plan::sim_profit` is in loan-token units; `profit_usd` = that x `loan_usd`;
  `gas_cost_usd` = L2 gas at the planning priority + the L1 data fee (`l1_fee_usd` on its own);
  `net_usd` = `profit_usd - gas_cost_usd`. A market without a loan price is a plan error.
- **Sanity checks** on the simulation (profit vs quote, block the node really simulated).
- `quote_bundle` values a whole market's liquidatable positions on one pair as one exit (used by
  the Atlas valuation, where one batch solver op liquidates them all); `gross_usd` is in USD and
  `gas_units` is the whole operation's estimated gas (liquidations + swap + overhead, from the
  `BundleGas` the caller passes: `atlas_venue::ATLAS_BUNDLE_GAS` for Atlas solver ops). It also
  produces an executable `BundlePlan` (the positions at full seize, the flash-loan size and the one
  full-size exit) that `atlas_venue::AtlasSolver::batch_inner` turns into the solver op's inner
  `liquidateBatch` call. v1 liquidates each position in full and sells 100 %, so a direct route is a
  candidate only when its allocation is full-size (route mode spends exactly the seized amount); the
  exits are weighed at the auction's gas price, where gas dominates. The bid is this estimate
  (`batch_est`: gross minus the whole metacall gas including `METACALL_FIXED_GAS`); the op's
  simulation only gates the send. Gas is calibrated on `contracts/test/AtlasBundleGas.fork.t.sol`
  (a 30-position batch ≈ 1.5M gas before its exit swap, ~34k per extra position).

### `LiquidationVenue<L>` (`bot/src/venue.rs`)

`target()` (the contract the tx goes to), `swap_recipient()` (the contract that runs the exit swaps:
an aggregator route is built for it as sender and recipient; default `target()`, but a venue that
nests the liquidation in another contract differs), `encode(market, user, pair, seize, repay, route,
min_profit, deadline)` -> `Payload { to, calldata, min_profit slot }`, `simulate(p, calldata, block,
overrides, at, fee, probe)` (`fee: Option<SimFee>` = the `maxFeePerGas` / `maxPriorityFeePerGas` the
call carries), `probe_gas`, and `packed_routes()` (default `true`). `repay` is the debt the liquidation
repays (`LendingProtocol::repay_for_seize`): a venue that funds the repayment itself borrows it, a
callback venue ignores it. The `min_profit` slot tells the executor where to patch the final
minProfit at send time. An `ExitRoute` is either `Packed(bytes)` (the self-built router's compact
route) or `Call { target, calldata }` (an aggregator swap). A venue whose contract has no packed
entry returns `packed_routes() = false`: the planner then never gives it a packed route (`direct`
mode reports "this venue has no packed-route entry"; `best` falls back to the aggregator).

`sim::Simulator` implements it for MorphoLiquidator: a packed route goes through the compact
fallback entry, an external call becomes the single swap step of the ABI `liquidate`. Without a
deployed address the liquidator's runtime code is injected by state override at a placeholder
address, so the bot can plan before anything is deployed.

The pieces every venue shares live in `sim.rs` (on top of `onchain_core::sim`): `LiquidatorCode::build(path, ctor, from)` runs a
contract's constructor (address arguments in order) in an `eth_call` to get its runtime code with
the immutables filled in, `LiquidatorCode::account(executor, allowed)` turns it into a state
override (owner = executor, not paused, `allowedTarget` set; `account_with` sets several allowlists,
`has_slot` checks a label exists), and `simulate_call` / `probe_gas_call` run any calldata with
`eth_simulateV1` (profit-agnostic: each venue decodes its own return value). A fee-priced
`simulate_call` also gives the sender a balance override: Base nodes check that the sender can pay
gas x `maxFeePerGas` even with validation off. With validation off the simulated block's base fee
is 0, so `tx.gasprice` equals the priority fee, which is what priority-fee taxes are levied on.

### Funding the repayment (`bot/src/funding.rs`)

There are two ways a liquidation's repayment is paid:

- **The protocol's own callback.** Morpho Blue sends the collateral first and calls back
  `onMorphoLiquidate`; the collateral is sold inside the callback and Morpho then pulls the
  repayment. No borrowing at all. This is `sim::Simulator` / MorphoLiquidator.
- **A flash source**, for protocols whose liquidation pulls the repayment up front (Aave-style
  `liquidationCall`). The repayment is borrowed first and repaid from the sale of the collateral:
  `flash_venue::FlashLiquidatorVenue` / FlashLiquidator. Two models, deliberately two traits:
  - `FlashLoan` (`name`, `lender`, `fee`): a fixed amount, the lender calls back, repay amount + fee
    before the callback returns. `MorphoBlue` (0 fee).
  - `FlashAccounting` (`name`, `manager`, `fee`, default 0): take now, settle the net delta before
    `unlock` returns. `UniswapV4` (the PoolManager; no swap step may go through V4 while the
    liquidator holds the lock).

  `FlashSource` is the trait the venue uses (`name`, `holder`, `fee`, `mode()`: 1 = Morpho flash
  loan, 2 = V4 flash accounting), implemented by `MorphoBlue` and `UniswapV4`;
  `FlashLiquidatorVenue` holds a `Box<dyn FlashSource>`. A new source takes both sides: a
  `FlashSource` impl and a matching branch in `FlashLiquidator.sol` (its `funding` value, the call
  that starts the flash and its callback, guarded like the existing two). The contract repays
  exactly what it borrowed, so a source with a fee is refused until the contract repays amount + fee.

`DirectLiquidation` (in `protocol.rs`) is what a protocol provides for the flash path:
`liquidation_call(market, user, pair, seize, repay) -> LiquidationTx { target, calldata, debt_token,
collateral_token }`. Morpho implements it with `liquidate` and empty callback data, which simply
pulls the repayment from the caller after sending the collateral, so Morpho also works as a
callback-free protocol (that is how the flash path is tested on real contracts).

`FlashLiquidatorVenue<L: DirectLiquidation>` builds `FlashLiquidator.liquidate(funding, debtToken,
debtAmount, collateralToken, liquidation, swaps, minProfit, deadlineBlock)`: the debt amount is
`borrow_for(repay)` = repay + `headroom_bps` + 1 wei (interest accrued before inclusion cannot make
the protocol pull more than was borrowed; unspent headroom goes back to the funder with the loan),
the aggregator exit becomes the single swap step with `amountIn = type(uint256).max` (sell all the
collateral that actually arrived, which the contract resolves to its balance), and minProfit sits at
`FLASH_MIN_PROFIT`. It has
no packed entry (`packed_routes() = false`). Without `exec.flash_liquidator` the contract is
injected at a placeholder address, like MorphoLiquidator.

### `Dex` (`bot/src/dex.rs`)

`quote_within(token_in, token_out, amount_in, gas_budget)`, `amount_out`, `swap_gas`,
`build(quote, sender, recipient, slippage_bps) -> SwapCall { target, calldata }`. The planner never
trusts the returned target: it must be in `exec.allowed_targets`. `router::Kyber` uses a gas ladder
(two quotes at different gas prices, the best route that fits the budget) and scales Kyber's gas
estimate by a safety factor.

## Exit routing: the self-built V3 router (`bot/src/v3router.rs`)

- Routes collateral -> loan token through direct legs and connector legs (collateral -> WETH ->
  loan token), on Uniswap V3-style pools of the factories in `[[router.factories]]` (fee-tier or
  tick-spacing factories). Factory addresses live in config, never in code. Routes are discovered
  once per `Pair` of every market (`LendingProtocol::pairs`) and keyed by it.
- **Quoting:** `contracts/src/sim/GridQuoter.sol` is never deployed; it is injected by state
  override and quotes a grid of sizes for every leg in one `eth_call` (the Uniswap-quoter revert
  trick), with a per-hop gas cap so a pathological pool cannot blow the call's gas limit. Quote
  calls carry the planning fee (`Planner::sim_fee`) from a placeholder caller (`QUOTE_CALLER`) that
  gets a balance override, so pools that read `tx.gasprice` quote what the tx would get.
- **Allocation:** greedy on marginal output over a concave combined curve (a shared connector sink
  is not double-counted), compared against single-leg solutions because each extra leg costs gas;
  `allocate_within` enforces a gas budget. Leg gas is priced at `planning_priority_fee_wei` and
  converted to loan-token units with `loan_usd`.
- **Execution:** the packed route calls pools directly; pools are paid inside their own swap
  callback (only the pool currently swapping, once, at most its input, in the hop's input token
  read before the swap; source legs must sell the collateral). A pooled connector token split by
  share across sink hops (bit 2) can keep a rounding remainder, and a pool that runs out of
  liquidity takes less than its input: both the connector remainder and the unsold collateral are
  swept to `PROFIT_RECEIVER` after the liquidation, so nothing stays at rest.

## On-chain: `contracts/src/MorphoLiquidator.sol`

- `liquidate(params, borrower, seizedAssets, repaidShares, swaps, minProfit, deadlineBlock)` (ABI
  entry, aggregator swaps), `liquidateRoute` and a packed `fallback` (direct route, compact
  calldata). Only `executor` may call them.
- Morpho transfers the collateral, then calls `onMorphoLiquidate(repaidAssets, data)`, where the
  swaps run; Morpho then pulls the repayment (standing allowance set by `approveMorpho`).
- `liquidateBatch(params, items, flashAssets, exit, minProfit, deadlineBlock)` (ABI entry and a
  packed `fallback` tag `0x01`), for an Atlas solver op: one Morpho `flashLoan` funds N `liquidate`
  calls on the same market (empty callback data, so Morpho pulls each repayment through the standing
  allowance), then a single `exit` (a direct route that must spend exactly the sum seized, or
  allowlisted aggregator steps) sells all the collateral. `onMorphoFlashLoan` runs only inside our
  own batch (an `inBatch` transient flag, the Morpho sender, once; `NotFunded` otherwise), and the
  collateral received must equal the sum seized.
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
| Fake pool in a direct route | pools are only called for `swap`/`token0`/`token1` and paid inside their own callback, at most the hop input, in the input token read before the swap (source legs: the collateral); nothing is held at rest. Within that a fake pool can still keep the profit above `minProfit`, so route pools come from configured factories (`getPool`) | `test_route_poolAskingMoreThanInputGetsNothing`, `test_route_doubleCallbackRejected`, `test_swapCallbackFromStrangerRejected`, `test_route_poolCannotSwitchItsPaymentToken`, `test_route_sourceLegMustSellCollateral` |
| Route not spending all seized collateral | explicit source legs must sum exactly to `seizedAssets` (`RouteSpendMismatch`); collateral a pool did not take (out of liquidity) is swept | `test_route_mustSpendExactlySeized`, `test_route_partialFillLeftoverIsSwept` |
| Connector left in the contract | the pooled connector's remainder is swept to `PROFIT_RECEIVER` | `test_route_pooledConnectorRemainderIsSwept` |
| Pool reporting positive output | every hop's output must be a negative delta (`BadRoute`) | `test_route_positiveOutputRejected` |
| Malformed packed calldata | strict decoding, no selector collision with the ABI entries | `test_packed_malformedRejected`, `test_packed_noSelectorCollision`, `test_packedEntry_equalsAbiEntry` |
| Emergency | the executor (hot wallet) or the owner may `pause` (an operator stop; the bot itself never calls it, its kill switch is a file), only the owner may resume | `test_pauseKillSwitch` |
| Forged / mistimed / repeated batch flash callback | `onMorphoFlashLoan` needs `msg.sender == MORPHO` and a batch of our own in flight (`inBatch`), cleared on entry so it runs once; `onMorphoLiquidate` stays blocked in batch mode (its own in-flight flag is never set) | `test_batch_flashCallbackAuth`, `test_batch_secondCallbackBlocked`, `test_batch_notFunded`, `test_batch_onMorphoLiquidateBlockedInBatch` |
| Batch seizing less than paid for, or a short exit | the collateral received must equal the sum seized (`CollateralShortfall`); a route exit must spend exactly it (`RouteSpendMismatch`); `minProfit`, the allowlist and no-funds-at-rest hold as for the single paths | `test_batch_collateralShortfall`, `test_batch_routeSpendMismatch`, `test_batch_minProfitEnforced`, `test_batch_swapTargetNotAllowlisted`, `test_batch_happyPath_steps` |

Off-chain defences: aggregator targets must be in `allowed_targets`; market ids are re-derived from
params at start; implausible simulations (profit far from the quote) are rejected; RPC errors are
rewritten so URLs with API keys never reach logs (`errors_never_contain_urls`); the risk gate
(`bot/src/risk.rs`) applies the kill-switch file, minimum net profit, a priority fee proportional to
net profit with a hard cap, and a daily budget for gas burned by failed txs. Its cost estimate is
L2 gas at the bid plus the plan's L1 data fee, compared with `profit_usd`.

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

## On-chain: `contracts/src/FlashLiquidator.sol`

For protocols without a liquidate callback. `liquidate(funding, debtToken, debtAmount,
collateralToken, liquidation, swaps, minProfit, deadlineBlock)` (executor only) flash-borrows
`debtAmount` of the debt token from Morpho (`FUND_MORPHO`, `flashLoan` -> `onMorphoFlashLoan`) or
from the Uniswap V4 PoolManager (`FUND_V4`, `unlock` -> `unlockCallback` -> `take` ... `sync` /
transfer / `settle`), then, whatever funded it: approves exactly `debtAmount` to the liquidation
target, makes the protocol's liquidation call, resets the allowance, requires collateral to have
arrived, sells it through the swap steps, and repays the funder exactly what was borrowed. The
debt-token balance left is the profit; it must reach `minProfit` and goes, with any leftover
collateral, to the immutable `PROFIT_RECEIVER`. The constructor takes `(morpho, poolManager, owner,
executor, profitReceiver)`; `poolManager = address(0)` disables V4 funding.

The contract repays exactly what it borrowed, so only 0-fee sources work: the bot refuses to build
the flash venue for a source whose `fee()` is not zero.

| Threat | Defence | Tests (`FlashLiquidator.security.t.sol`) |
|---|---|---|
| Someone else starts a liquidation | executor-only entry; deadline; `pause` | `test_onlyExecutorAndNotExpiredOrPaused` |
| Forged or stray funding callback | sender must be the funder AND a liquidation funded by that very source must be in flight (transient funding tag) | `test_callbacksRejectStrangersAndCallsOutsideAnOwnLiquidation`, `test_crossSourceAndSecondCallbacksAreRejected` |
| A second callback in the same liquidation | the tag is cleared on entry (one-shot) | `test_crossSourceAndSecondCallbacksAreRejected` |
| A funder that never calls back | `NotFunded` after the funding call returns | `test_funderThatNeverCallsBackReverts` |
| Poisoned liquidation call or swap | two owner allowlists: the liquidation call only to an `allowedProtocol`, swaps only to an `allowedTarget` (a router never gets the allowance meant for the protocol); exact allowances reset to zero | `test_liquidationAndSwapTargetsMustBeAllowlisted`, `test_anAllowlistedRouterIsNotAnAllowedProtocol`, `test_allowancesAreResetAfterTheCall` |
| A liquidation that seizes nothing | `NothingSeized` | `test_aLiquidationThatSeizesNothingReverts` |
| Not enough profit | `minProfit` on the debt token left after repaying the funder | `test_minProfitEnforced` |
| Unknown funding / V4 not configured | `FundingDisabled` | `test_unknownFundingAndDisabledV4Revert` |

`test_bothFundingSourcesLiquidateRepayAndPayTheReceiver` checks the happy path for both sources.
`FlashLiquidator.fork.t.sol` runs it on real Base contracts, with Morpho as the callback-free
protocol: `test_morphoFlashLoanFundsACallbackFreeLiquidation`,
`test_v4FlashAccountingFundsACallbackFreeLiquidation` (same profit as the Morpho flash loan),
`test_unprofitableExitReverts`, `test_protocolTargetMustBeAllowlisted`. The Rust encoding is
checked against the contract on an anvil fork by `e2e_flash_venue_simulates_a_flash_funded_liquidation`.

Deploying it is like MorphoLiquidator (owner, executor, profit receiver), plus: allowlist the
protocol's liquidation target (`setAllowedProtocol`) and the aggregator router(s)
(`setAllowedTarget`), then set `exec.flash_liquidator`.

## On-chain: `contracts/src/AtlasLiquidationSolver.sol` (simulation only)

A solver contract for Chainlink SVR auctions run by Fastlane Atlas, written against the Atlas
solver interface (`atlasSolverCall`), not derived from Atlas's own code. In the auction winner's
slot Atlas calls `atlasSolverCall(solverOpFrom, executionEnvironment, bidToken, bidAmount,
solverOpData, extraReturnData)`; `solverOpData` = `(SolverCall[] calls, SwapStep bidSwap, address
profitToken, uint256 minProfit)`. The solver runs the calls (liquidations through a
MorphoLiquidator deployed with the solver as executor and profit receiver), runs the bid swap
(loan token -> WETH, `target = address(0)` = none), unwraps WETH as needed (keeping aside any ETH
Atlas lent, so the bid never spends it) and pays the ETH bid to the execution environment, settles
with Atlas (repays that loan and approves the gas liability read at the end through `reconcile`),
checks `minProfit` on the profit token this call added AFTER the bid (a balance change: leftover
collateral its liquidator sent it earlier never counts), sends the whole profit-token balance and
any leftover WETH or ETH to `PROFIT_RECEIVER`, and returns the net profit. Constructor: `(atlas, weth, owner, executor,
profitReceiver)`; `executor` is the solver operation signer.

| Threat | Defence | Tests (`AtlasLiquidationSolver.security.t.sol`) |
|---|---|---|
| Someone other than Atlas calls it, or an op signed by someone else | `msg.sender == ATLAS` (`InvalidEntry`) and `solverOpFrom == executor` (`InvalidCaller`) | `test_onlyAtlasForTheSignersOperation` |
| Non-ETH bids; emergency | ETH bids only (`UnsupportedBidToken`); the executor (signer) or the owner may `pause`, only the owner may `unpause` | `test_ethBidsOnlyAndPauseStopsIt` |
| ETH that Atlas lent spent on the bid | the borrow liability (up to `msg.value`) is kept aside: WETH is unwrapped for bid + repayment, and the loan goes back through `reconcile` | `test_borrowedEthIsRepaidEvenWhenTheBidNeedsEth` |
| Poisoned calls or bid swap | every call target and the bid-swap router must be owner-allowlisted; exact allowance reset | `test_callAndBidSwapTargetsMustBeAllowlisted` |
| Bid larger than what the op earns | the bid must be bought and paid, and `minProfit` holds after it | `test_aBidTheSwapCannotFundReverts`, `test_minProfitIsCheckedAfterTheBid` |
| Tokens already in the solver counted as this op's profit | `minProfit` is checked on the profit-token balance change of this call; the old balance is swept with it | `test_tokensAlreadyHereDoNotCountAsProfit` |
| Admin by others | owner-only admin | `test_adminIsOwnerOnly` |

`test_runsTheLiquidationPaysTheEthBidSettlesAndKeepsTheRest` covers the happy path;
`AtlasLiquidationSolver.fork.t.sol` runs a real liquidation inside a solver op on a Base fork
(`test_solverLiquidatesPaysTheEthBidAndKeepsTheRest`, `test_aBidAboveTheProfitReverts`) with
`contracts/src/sim/AtlasStub.sol` standing in for the Atlas escrow, and checks the deployed Atlas
escrow has the settlement calls the solver makes with the semantics it assumes
(`test_theDeployedAtlasEscrowHasTheSettlementCallsTheSolverMakes`: `shortfall` is 0 outside a
metacall, `reconcile` reverts `WrongPhase` there, `getAtlasSurchargeRate` is readable). A full
metacall cannot be replayed (it needs the user's and the DApp's signatures).

## Off-chain modules (`bot/src/`)

| Module | Responsibility |
|---|---|
| `config` | TOML config + RPC resolution (URLs only from `.env`) |
| `protocol`, `engine` | the protocol seam and the generic HF engine |
| `morpho/` | Morpho Blue: on-chain reads, events, math (shares/assets with virtual shares, LIF, HF, seize <-> repay), indexer API |
| `planner`, `venue`, `dex` | planning and its two seams |
| `sim` | MorphoLiquidator encoding; shared `simulate_call` / `probe_gas_call` (`eth_simulateV1`: profit, gas, L1 fee, at the next block's number and timestamp) and `LiquidatorCode` (injected contracts) |
| `funding` | flash sources: `FlashLoan` / `FlashAccounting`, `MorphoBlue` / `UniswapV4`, the `FlashSource` trait |
| `flash_venue` | `FlashLiquidatorVenue`: FlashLiquidator as a venue for `DirectLiquidation` protocols |
| `atlas_venue` | `AtlasSolver` / `AtlasBid`: AtlasLiquidationSolver as a venue (simulation only) |
| `router`, `v3router` | Kyber aggregator client; self-built V3 router |
| `risk` | send/skip decision and priority fee |
| `executor`, `actor` (onchain-core) | keystore signing, broadcast, nonce, failure policy; the executor task (below) |
| `rpc` (onchain-core) | multi-endpoint pool (below) |
| `heads` (onchain-core) | new-head signal: WS `newHeads` on several endpoints, HTTP fallback |
| `gates`, `bid` (onchain-core) | kill switch, daily budget, deadlines; bidding against observed winners |
| `runner`, `policy` | the watch loop as a library (`Runner`), the two bidding policies |
| `ledger` | the day's failed-gas spend, Atlas bond and PnL, persisted in live mode; the loss stop |
| `oracle_logs` | Chainlink updates from canonical-block logs (the Atlas price model's feed) |
| `oracle` | local MorphoChainlinkOracleV2 price model, verified against `price()` at start |
| `flashblocks` | Flashblocks stream decoding: examples only (Denim removes Flashblocks) |
| `atlas` | Chainlink SVR / Fastlane Atlas auction feed and valuation |
| `atlas_submit` | solver operations: EIP-712 signing, submission (live, opt-in) |
| `atlas_bond` | the solver's atlETH bond: state, band, auto-withdraw steps, our ops' Atlas results |
| `loss` | loss analysis: every on-chain liquidation in tracked markets vs what we saw |
| `dash/`, `live` | SQLite history + static dashboard; live HF page on localhost |

### Simulation

One `eth_simulateV1` call runs the whole liquidation and returns profit and real gas; the L1 fee
is added (`getL1FeeUpperBound`). Plans are simulated at the planning fee (see `Planner`); once the
risk gate has picked a bid above the base priority, `checked_priority` (in `runner.rs`) simulates the
plan again at that priority and keeps the bid only if the net still clears `min_net_usd` there,
else the base priority goes; the send's minProfit uses the lower of the two profits. A
re-simulation that shows no profit at all means the plan is not sent. Simulations run as the
**next** block (block override of number and timestamp), because candidates are evaluated with
interest accrued to the next block.
`eth_estimateGas` is not used: some node implementations ignore block overrides there. Without a
real gas figure a plan is an error, never a guess. Plans pinned to a "pending" state check which
block the node actually simulated, because pending calls can run on a stale block.

### Executor (`onchain_core::executor`, `onchain_core::actor`)

- **Its own task.** `watch` starts the executor as a task (`actor::spawn`): sends are messages
  (`ExecHandle::send`, answered when an endpoint accepted the tx), its state is read synchronously
  (`can_send`, `is_inflight`, `spendable`), it polls receipts on every block the loop announces,
  and settled txs come back on a channel that the loop drains each block (journal `outcome`, the
  reservation released, the daily budget charged for reverts), after waiting briefly
  (`wait_polled`, at most `RECEIPT_WAIT`) for the new head's receipts so a tx included in it no
  longer counts as in flight. No receipt or broadcast ever blocks planning.
- **Signing:** only from an encrypted keystore; the password comes from an env var that is removed
  from the process environment right after reading, or from a hidden prompt.
- **Several txs in flight.** Up to `exec.max_inflight` (default 1; set it higher for crashes, when
  many positions become liquidatable within a few blocks) with consecutive nonces. Each tx has a
  `TxKey` (group = market, id = borrower): the same position is never sent twice, and no second tx
  goes to a market that already has one in flight (plans are simulated without the txs in flight,
  and one in the same market sells the same collateral through the same pools first, so the second
  plan's price would not hold). The block path and the flashblock events it handles while planning
  send concurrently, so this is enforced by the executor task itself (`exclusive_group`), not only
  by the `is_group_inflight` check before the send. Each send reserves its worst case (gas limit
  at its max fee, plus the L1 fee) in the daily budget until it settles, so several sends at once
  cannot each fit the cap and overrun it together. minProfit is patched to
  `sim profit x min_profit_keep`, gas limit to `sim gas x gas_limit_buffer`. The wallet must afford
  the worst-case gas: `spendable` = the balance read in the background (with the nonce, by
  `spawn_nonce_watch`) minus what the txs in flight may still burn, so the send path pays no RPC
  call for it.
- **Settling.** `poll` fetches every in-flight receipt in parallel. A tx with no receipt
  `DROP_GRACE_BLOCKS` (2) past its deadline is dropped, and so is every later-nonce tx still
  without a receipt (none of them can land before it). A dropped tx still sitting in the mempool
  (e.g. its max fee fell below a rising base fee) would block every later nonce, so `cancel_stuck`
  first replaces each one still pending with a 0-value self-transfer at the same nonce and fees
  bumped past the replacement threshold (it would only revert `Expired` anyway); then the nonce is
  resynced from the chain (if that read fails, the outcomes are still reported and the next send
  resyncs).
- **Broadcast:** the same raw tx goes to every endpoint plus optional `extra_broadcast`; the first
  acceptance returns, the rest finish in the background. A dedicated HTTP client keeps connections
  warm (keep-alive pings), so a send never pays DNS/TLS setup.
- **Nonce:** tracked locally, re-synced from chain on drops / nonce errors / "replacement
  underpriced" (then retried once). Bots that sign with the same wallet share a lock file per
  wallet (`exec.nonce_file`, default `~/.local/state/onchain-bots/nonce-<wallet>`), held between
  signing and broadcast. The lock fails closed: if the file cannot be locked, nothing is sent
  (another bot on the wallet could otherwise sign with the same nonce). The file's format is
  versioned (`v1 <next nonce> <unix ms>`); an unknown format is never trusted, so bots built on
  incompatible `onchain-core` versions refuse to send rather than collide. A stale line in the
  unversioned format of earlier releases is treated as empty (no manual cleanup after an upgrade).
- **Failure policy:** `max_failures_in_window` failed or dropped txs within `failure_window_secs`
  pause sending for `failure_cooldown_secs`, then it resumes.
- **Deadline and lookahead:** `deadline_secs` converted to blocks from the measured block time;
  interest is accrued `accrual_lookahead_secs` ahead (unset = the measured block time rounded up,
  1 to 2 s), so both follow a block-time change (2 s -> 200 ms) without config edits.
- **Kill switch:** a relative `exec.kill_switch_file` is anchored at the config file's directory,
  so it is the same file whatever directory the bot is started from.
- **Live mode needs all of:** `exec.mode = "live"`, `watch --live`, the venue's deployed contract
  (`exec.liquidator`, or `exec.flash_liquidator` with `exec.venue = "flash"`), `exec.keystore`, and
  a passing pre-flight: contract exists, its executor is the keystore address, not paused, profit
  receiver is not the hot wallet, wallet balance at least `min_wallet_eth`, and per venue: the
  standing Morpho allowances (`approveMorpho`) for the callback venue; for the flash venue, Morpho
  as an `allowedProtocol` and the contract's `MORPHO()` / `POOL_MANAGER()` matching `flash_funding`.

### RPC pool (`onchain_core::rpc`)

Endpoints are declared in `[[rpc]]` (`http_env` names a variable in `.env`; public endpoints may
use `http`). The pool races or fails over by priority, honours per-endpoint `skip_methods` and log
range limits, switches endpoint on "method not found", rate limits and capacity errors (with a
cooldown), and treats "block not found / header not found" from a lagging node as *lag*: try the
next endpoint without cooling the laggard down, unless it lags repeatedly. Capacity errors are
recognised by phrases (rate, plan, range and availability limits such as "rate limit", "block
range", "free plan"), never by single words; deterministic errors of the call itself (revert, gas
limit, insufficient funds, nonce, fee cap...) never fail over or cool an endpoint down. Logs only
ever show the endpoint name. Requests carry a `liquidation-framework/<version>` User-Agent.

### The runner (`bot/src/runner.rs`) and bidding policies (`bot/src/policy.rs`)

`Runner::build(RunnerInputs, auction, priority)` connects to the configured endpoints, builds the
Morpho engine and refuses markets whose loan token it cannot price; `run()` is the watch loop until
an error (or `RunnerInputs::shutdown`). The bot binary (`runner.rs`) only parses the CLI, loads the
signer before the async runtime exists (`load_signer_pre_runtime`) and calls them, so a private
binary can run the same loop with its own policies:

- `AuctionBidPolicy::bid_usd(market, net_usd, eth_usd)`: the bid in an SVR auction (default
  `DefaultAuctionBid`: net x (1 - `[atlas] margin`)); used for the journaled valuation and the
  submitted op alike;
- `PriorityBidPolicy::bid(market, plan, competition, eth_usd)`: the block path's priority fee
  (default `DefaultPriorityBid`: the `[exec]` bid curve below).

The spending limits are applied by the runner after the policy, so a policy decides how much to
bid, never how much may be spent.

### Block path and send timing

On each head the engine refreshes the touched positions, and the most valuable candidates
(`max_plans_per_block`) are planned all at once; each one is journaled, gated and sent the moment
its own plan is ready (a slow aggregator quote of one never delays another), and the sends of a
batch run concurrently (their bid re-simulations in parallel; the executor task broadcasts them in
turn), at most one per market per batch.

SVR auctions and oracle log events (below) arrive on one side-event channel that the loop serves
while waiting for a head, while fetching, and while a batch plans (`plan_and_send`'s side events),
on the previous block's snapshots: an auction never waits behind the block path. The block time
is measured over the last 50 heads (`BlockClock`), so deadlines, `replan_secs` and the accrual
lookahead follow a block-time change (Base Denim: 2 s -> 0.2 s) without a setting to switch.

### Bidding (`bot/src/risk.rs`, `onchain_core::bid`)

The priority fee is set against the competition seen on-chain. Every liquidation in a tracked
market (the loss analysis already looks each one up) records what its winner paid as priority fee
per gas per USD of gross it took, per market, for `bid_window_secs` (`WinningBids`; seeded at start
from the journal's complete liquidation rows: gross USD, block time and the tx's `to`). Our own
liquidations and Atlas metacalls (whose priority fee is the bundler's; the race was the auction)
are left out. For a plan, `BidCurve::priority`:

- predicts the competitor's priority fee: the `bid_quantile` (default 0.9) of those ratios x this
  plan's gross USD (a market with fewer than `bid_min_samples` winners uses all markets');
- bids `bid_outbid_bps` (default 5%) above it,
- but never more than the base priority plus a share of the net: `priority_fee_share` for small
  opportunities, rising linearly to `bid_share_large` at `bid_large_net_usd` (a large liquidation
  may bid most of its profit); never above `max_priority_fee_wei`, never below `priority_fee_wei`.

Without enough observations it bids the affordable share, like a plain share-of-profit bidder.
The decision journals the competitor priority it bid against (`competitor_wei`). A bid above the
base priority is then simulated again at its own priority before it is sent (see Simulation):
some pools (Aero) tax the priority fee.

### Spending limits (`bot/src/ledger.rs`)

- **Per tx** (`[exec] max_tx_cost_eth`, default 0.002): a block-path tx's worst case, `gas_limit x
  (2 x base fee + priority) + L1 fee`, must fit. The bid is cut to fit; if the cut bid falls below
  the base priority or at/below the predicted competitor, the tx is not sent (it would be sent to
  lose). Checked once, in `send_approved`, after the policy.
- **Failed gas per UTC day** (`[exec] daily_gas_budget_usd`): a tx in flight holds its worst case;
  a revert charges its real cost.
- **Atlas bond band** (`[atlas.solver] bond_min_eth` / `bond_max_eth`, no defaults, required with
  `submit`): below the min no op is submitted; each op must also have its maximum liability covered
  by the bond (see "The bond" below). Read every `BOND_CHECK_HEADS` heads and after an accepted op.
- **Daily loss** (`[exec] daily_loss_limit_usd`): realized PnL = what settled block-path txs paid
  `PROFIT_RECEIVER` (their receipts' ERC20 transfers of the tracked loan tokens) minus their gas,
  minus the bond's net debits (failed ops' gas, won ops that paid less gas than charged; the bot's
  own unbonding is not a loss). Atlas income is not counted (an op's profit goes to
  `PROFIT_RECEIVER` from inside someone else's metacall), so the PnL is a lower bound and the stop
  can only fire early. Past the limit the kill switch file is written; sending stops until a person
  removes it.

Refusals by these limits are journaled (`kind: "refused"`, `reason`, `worst_eth`, `cap_eth`). In
live mode the day's failed-gas spend, the Atlas day (below), the last bond reading, ETH withdrawn
from the bond and not yet sent on, and the PnL are kept in a ledger file (`v1` line + JSON; `[exec]
ledger_file`, default next to the shared nonce files) and read back at start; a file the bot cannot
read refuses live mode (a ledger from before v0.4.0 reads with an empty Atlas day).

### Oracle logs and the price model (`bot/src/oracle_logs.rs`, `bot/src/oracle.rs`)

Atlas auctions are valued at the auction's median: `PriceModel::preview` replaces one aggregator's
answer and recomputes every market using it, so the model must know every feed's current answer.
It is loaded at start (verified against each oracle's `price()`) and kept current from canonical-block
logs of the watched aggregators.

A `MorphoChainlinkOracleV2` is modelled directly. A `MetaOracleDeviationTimelock` (a deviation
timelock that selects between a primary and a backup oracle, detected by a non-reverting
`currentOracle()`) is modelled through whichever underlying oracle it currently selects, one level
deep, and still cross-checked against the meta-oracle's own `price()`. A primary<->backup switch has
a public timelock of hours, so a low-frequency re-read of `currentOracle()` (`recheck_meta_oracles`,
every `META_RECHECK_HEADS` heads) is enough to notice it and rebuild the market's model against the
new oracle. A market the model cannot reproduce (a non-V2/meta oracle, an ERC-4626 vault conversion
factor, a feed read that failed, or a model that disagrees with `price()`) is left on the block path
and logged at start with the reason, and listed under `unmodelled` in the load line — never dropped
silently.

The log updates applied to the loaded model:

- `NewTransmission(round, answer)`: a public transmit. A plain feed's answer is in effect at once;
  an SVR feed's is kept pending with its round and block time (Morpho does not read it yet).
- `SecondaryRoundIdUpdated(round)`: the SVR secondary written by the auction winner's Atlas
  metacall. It carries the round only; the pending answer of that round takes effect.
- A pending SVR answer no auction carried takes effect once older than `[atlas] svr_cutoff_secs`
  (checked on each head).

`AnswerUpdated` is not read: an SVR feed emits it at the public transmit, before the price is in
effect (checked on the XRP/USD DualAggregator, fixture `tests/fixtures/oracle_logs/`). The block
time of a log is its `blockTimestamp` (Base nodes include it), else the head's. Each head also
cross-checks the previous block's snapshot against the model and resyncs a market that differs
(a missed event, a reverted transmit, a swapped aggregator: then the subscription follows the new
address); three misses in a row reconnect the logs. A log removed by a reorg drops the pending
answer and forces the check. The logs WebSocket is `[atlas] logs_ws`, else the first `ws` endpoint.

`bot/src/flashblocks.rs` (the same-block path from the Flashblocks stream) is not used by the
watch loop any more (Base Denim removes Flashblocks); `[flashblocks] enabled = true` is refused.

### Atlas / Chainlink SVR (`bot/src/atlas.rs`)

On SVR-enabled feeds, the right to liquidate after an oracle update is auctioned through Fastlane
Atlas. The module listens for auctions (`[atlas] observe`), values each one with the same planner
(`quote_bundle` for the whole market, then per-position plans) on the price model above, and
journals it (`atlas_auction`, with `queued_ms` / `bundle_ms` / `value_ms`); it submits only with
`[atlas.solver] submit` in live mode (below). `value-auction` replays a past auction. Auctions the
feed drops on a full side channel are journaled as `atlas_dropped`. The bid basis is what the solver would
keep after Atlas charges it for gas: per market `atlas_net_usd` = the bundle's `gross_usd` minus its
`gas_units` at the auction's `maxFeePerGas` x (1 + surcharge), summed into `bundle_net_usd`; the
journaled bid is the auction policy's on that net (default x (1 - `atlas.margin`)). The surcharge is read from the escrow at start
(`getAtlasSurchargeRate`, scale 1e4); `[atlas] gas_surcharge` (default 0.1) is the fallback.

**Solver venue (`bot/src/atlas_venue.rs`, simulation only).** With `[atlas.solver] simulate = true`,
each market of a valued auction also gets a `solver` field in the journal (`solver_op` in
`runner.rs`): the top position's direct-router plan is wrapped into `atlasSolverCall` with a bid of
(`profit_usd` minus the op's gas at the Atlas rate above) x (1 - `atlas.margin`), and simulated as
Atlas would call it, against `AtlasStub.sol` injected at the Atlas address (reporting, in its
storage slot 0, the gas liability the bot computes for the op), with the solver and its
MorphoLiquidator injected unless deployed; the result includes the L1 fee of the call. The op pays
its gas in ETH out of the proceeds (`gasPayWei`, see "The bond"), so the journal's net after the bid
(`net_after_bid`, `net_after_bid_usd`) is also net of the gas (`gas_pay_eth`). `solver_op`
wraps only direct-router plans (an existing plan's aggregator calldata was built for the direct
liquidator's address), and nothing when `exec.venue = "flash"`. `AtlasSolver::calldata` builds the
call (the bid plus the gas pay are bought with one `exactOutputSingle` on `bid_router` unless the
loan token is WETH; minProfit at `SOLVER_MIN_PROFIT`); `AtlasBid { solver, bid_wei, gas_pay_wei,
gas_liability_wei }` implements
`LiquidationVenue<Morpho>` with `swap_recipient()` = the solver's MorphoLiquidator, so the planner can
also plan through it, aggregator routes included (they are built to deliver to that liquidator). `e2e_atlas_solver_op_pays_its_bid_out_of_the_liquidation`
checks it on an anvil fork.

**Submitting solver operations (`bot/src/atlas_submit.rs`).** With `[atlas.solver] submit = true`
in live mode, a solver op that simulates with a profit after its bid is built, signed and
submitted to the auction (`submit_solver_op` in `runner.rs`):

- the op: `from` = the hot wallet (it must be the solver's `executor`), `to` = Atlas, `gas` = the
  simulated gas x `gas_limit_buffer` within the DappControl's `getSolverGasLimit()`, `maxFeePerGas`
  and `deadline` = the auction's, `solver` = the deployed AtlasLiquidationSolver, `control` = the
  DappControl, `userOpHash` = the auction's, `bidToken` = ETH (`address(0)`), `bidAmount` = the bid,
  `data` = the solver's `solverOpData` (`AtlasSolver::op_data`), rebuilt at submission: `gasPayWei`
  recomputed from the simulated gas (without the `gas_limit_buffer`: see "The bond") and bought
  together with the bid, any increase over the simulated gas pay taken out of the profit, and
  minProfit = that net x `min_profit_keep`;
- the signature: EIP-712 over `SolverOperation(address from,address to,uint256 value,uint256 gas,
  uint256 maxFeePerGas,uint256 deadline,address solver,address control,bytes32 userOpHash,address
  bidToken,uint256 bidAmount,bytes data)` with the domain read at start from the deployed
  AtlasVerification (`Atlas.VERIFICATION()`, `eip712Domain()`: name `AtlasVerification`, its
  version, chain id, verifying contract); raw `r || s || v` with v = 27 / 28, as Fastlane's
  atlas-sdk-go signs. The digest is checked equal to `AtlasVerification.getSolverPayload(op)` on
  Base (a recorded value in a unit test, a random op in `e2e_solver_op_digest_matches_atlas_verification`);
- the submission: JSON-RPC `solver_submitSolverOperation` with `[{ auction_id, auction_solution }]`
  to `[atlas.solver] endpoint` (default the Chainlink SVR bid endpoint), the op in the SDK's
  `SolverOperationRaw` form (hex quantities). Tested against a local mock endpoint;
- the rules: one op per block per EOA (Atlas keeps only one; a second auction for the same block
  needs another EOA), so an auction's ops are tried most profitable first (profit after the bid)
  until the endpoint accepts one; a refused op gives the block's slot back to the next, and the
  others are journaled with the op that was submitted; the kill-switch file stops
  submissions; and an op is only submitted while the bond is at least `bond_min_eth` and covers
  the op's maximum liability (else `skipped: "bond_insufficient"` with the liability and the
  bonded amount). The journal's `solver` field records what was submitted (`submitted`: gas, gas
  pay, liability, bonded).

At start (`build_submitter`), the bot refuses to submit unless the solver and its liquidator are
deployed (`[atlas.solver] address`, `liquidator`), the solver's `executor()` is the hot wallet and
its `PROFIT_RECEIVER()` is not, the hot wallet has at least `bond_min_eth` atlETH bonded
(`Atlas.balanceOfBonded`), and the Atlas domain and the DappControl's gas terms (solver and dApp gas
limits, bundler surcharge) can be read.

### The bond: gas paid from the proceeds (`bot/src/atlas_bond.rs`, v0.4.0)

How Atlas charges a solver (checked on the deployed Atlas, `AtlasBondGas.fork.t.sol` replays a real
Base SVR metacall with our op in the winner's slot):

- **Before it runs an op**, Atlas requires `bonded >= maximum liability` (else the op is skipped
  with InsufficientEscrow). The liability is computed from gas *limits*: (the op's `gas` + the
  userOp's gas + the DappControl's dappGasLimit + the metacall overhead) x (1 + Atlas and bundler
  surcharges) x the gas price. Paying gas in ETH does not lower it: the bond still has to cover the
  largest op. The bot computes it (`OpGasPrice::liability_wei`, `non_solver_gas`) at the auction's
  max fee; the userOp gas comes from the feed (`gas`) or `[atlas] user_op_gas`, the dappGasLimit
  and bundler surcharge from the DappControl. The overhead constant (`LIABILITY_OVERHEAD_GAS`) is
  above the one measured on the fork, so the check errs on refusing.
- **The winning op** settles: it calls `reconcile{value: repay + gasPay}(liability - gasPay)`. At
  the end of the metacall Atlas charges the real gas (used x surcharges x gas price); the ETH the
  op paid beyond that is credited to the bond, a shortfall is taken from it. So an op that pays
  about its real gas leaves the bond where it was (fork: within ~1 % of the charge).
- **A failed op**'s gas is still taken from the bond.

What the bot does:

- **Gas pay.** Valuation: `gasPayWei` = (estimated op gas + `METACALL_FIXED_GAS`) x max fee x (1 +
  Atlas's and the bundler's surcharges) — exactly the gas every bid basis (batch and single op)
  already subtracts, from the same per-gas price; the bid swap buys bid + gas pay; the contract
  pays `min(gasPayWei, Atlas's liability)` (a wrong value cannot overpay). Submission: recomputed
  from the simulated gas + `METACALL_FIXED_GAS` — Atlas's expected charge for the gas the op uses,
  not its gas limit (simulated x `gas_limit_buffer`): the proceeds must buy the gas pay, so paying
  for the buffered limit would take value out of thin ops for nothing; an overpay is credited to
  the bond, a small shortfall taken from it. `gasPayWei = 0` is the previous behavior (all gas
  from the bond).
- **Bundle size from the bond.** A valuation plans an op for at most what the bond affords
  (`bond_capped_solver_gas`: the largest op gas whose liability the bonded atlETH covers, divided
  by `gas_limit_buffer`), so a market's bundle shrinks to fit the bond instead of being refused
  whole. Only in live mode with `submit` (the bond is read there).
- **The band.** Below `bond_min_eth`: no op is submitted, an alert is logged, and nothing moves
  out of the bond. Above `bond_max_eth` (overpaid gas accumulates): an alert, or with
  `bond_auto_withdraw = true` the hot wallet unbonds the excess, waits until Atlas releases it
  (`unbondingCompleteBlock()`: ESCROW_DURATION blocks after the last change to the bond, which a
  charge during the wait restarts, plus a few blocks of margin), withdraws it and sends the ETH on
  to the solver's `PROFIT_RECEIVER` — one step at a time (`next_step`), each a tx through the
  executor at the normal priority fee, only while nothing else is in flight, in live mode with the
  kill switch off. Unbonded atlETH the bot did not unbond (an operator's `deposit` not yet bonded)
  pauses auto-withdraw with an alert: `withdraw` takes that balance first, so the bot would sweep
  it. The bond tx in flight (its hash) and the withdrawn amount waiting to be sent are kept in the
  ledger: a tx the executor stops tracking (a restart, or one reported dropped that still landed)
  is reconciled from its receipt, so a withdrawal is never left in the hot wallet unnoticed and a
  send never repeated. Bonding stays an operator step.
- **Results and the ledger.** At each bond reading the bot reads Atlas's `SolverTxResult` logs for
  its signer (`atlas_op_result`): won ops (and the gas each paid, matched to its submission),
  failed ones. The ledger's Atlas day attributes each reading's change from the chain state alone:
  atlETH that moved from bonded to unbonding (an unbond, the bot's or an operator's) is not a loss;
  a remaining drop is a debit (failed-op gas if the interval had failed ops and no win); a rise is a
  credit (overpaid gas) only in an interval with a won op, else a deposit, which does not offset the
  day's losses. Journal: `atlas_bond` per reading (bonded, unbonding, the band, the next step),
  `atlas_bond_tx` per settled bond tx (`via: "receipt"` when reconciled that way).
- **Dashboard.** The bond card shows the bonded amount against the band and our ops' results; the
  paper table adds the result of the one op an auction gets (the submitted one, else the market
  whose simulated op nets the most), won if that op's own bid beats the winner's, with its
  simulated net after bid and gas (`net_after_bid_usd`) as the profit.

### Bidding in SVR auctions: operator steps

The bot never puts funds into Atlas; the operator does, from their own terminal (the bot only takes an excess back out of the bond, and only with `bond_auto_withdraw`):

1. Deploy `AtlasLiquidationSolver(atlas, weth, owner, executor = hot wallet, profitReceiver =
   cold wallet)` and a `MorphoLiquidator` whose executor and profit receiver are the solver;
   allowlist the liquidator and the bid router on the solver (`setAllowedTarget`), and the exit
   routers on the liquidator; `approveMorpho` the loan tokens on the liquidator.
2. Bond atlETH from the hot wallet (the EOA that signs the ops): enough to cover the maximum
   liability of the largest op you want to send (see "The bond"; a smaller bond means smaller
   bundles), e.g. `cast send <ATLAS> 'depositAndBond(uint256)' <WEI> --value <WEI> --account bot-hot`.
   Check with `cast call <ATLAS> 'balanceOfBonded(address)(uint256)' <HOT>`. Won ops pay their own
   gas; failed ops' gas is taken from the bond.
3. Set `[atlas.solver] simulate = true`, `submit = true`, `address`, `liquidator`, `bond_min_eth`
   and `bond_max_eth` (optionally `bond_auto_withdraw`); start `watch --live` with `exec.mode =
   "live"`.
4. Withdraw by hand: `unbond(amount)`, wait until `unbondingCompleteBlock(<HOT>)` has passed
   (`ESCROW_DURATION()` blocks, 64 on Base when checked, after the bond's last change), then
   `withdraw(amount)` (or `redeem` then `withdraw`).

Our ops' results are read from Atlas's `SolverTxResult` logs (see "The bond"); Fastlane's query API
(`solver_getSolverOperationResult`, see the Chainlink searcher docs) is not used.

### Dashboard and live page

`bot dashboard` ingests the journal and run log into SQLite (`dashboard.db_path`) and renders a
static HTML page (no server): KPIs, latency, liquidations, auction valuations, exit-path analysis
of on-chain liquidations (decoded once from tx + receipt and cached), RPC usage. `bot live` serves
a read-only page on `127.0.0.1:live_port` with the positions closest to liquidation, from the
snapshot `watch` writes every block.

### Shadow-run hardening (v0.3.2)

From the first multi-day GCP shadow run:

- **Surviving a broken RPC.** A single endpoint can report a wildly wrong block number; a head more
  than `head_sanity_gap` beyond the freshly read tip is dropped (`runner::head_plausible`), so a bad
  number is never passed as the query block (which would make every multicall fail "from block >
  latest"). `load_markets` retries empty/garbage market data a desynced endpoint returns with a 200
  (which the pool does not fail over), and the "fell behind" full reload is non-fatal — all so one
  bad endpoint logs and retries instead of exiting.
- **Revert reasons.** A reverted solver/liquidator sim is decoded by `bot/src/revert.rs` (the custom
  errors, recursing into the `CallFailed`/`SwapFailed`/`BidSwapFailed` nested reason, plus
  `Error(string)`/`Panic`). The revert data is read from the `SimReverted` in the error chain and
  decoded in full whatever its size; only the error text journaled after it is capped (4096 chars).
- **Op reliability.** A Kyber build that fails on a route using an RFQ/PMM source (`kyber_rfq_exclude`)
  re-quotes over pools only — decided by the route's own sources, not the error text, so an unrelated
  failure on a pools-only route costs no extra round trip, and a timeout is not retried; the
  aggregator exit's gas carries extra safety (`BundleGas::agg_swap_safety`) and a batch op whose
  *real* simulated gas exceeds `bundle_gas_headroom × solver_gas_limit` is not sent; the bid swap
  uses the deepest loan-token/WETH pool per loan token (probed at startup).
- **Latency.** The batch op simulates concurrently with the per-position plans (diagnostics plus the
  single-position fallback), so slow plans do not delay the op that gets sent; Kyber calls have a
  time budget; the Atlas valuation pool hedges `eth_simulateV1` immediately
  (`onchain_core::rpc::RpcPool::with_eager_hedge`) for at most `eager_hedge_max_in_flight` sims at a
  time, so a crash burst does not double the pool's load. The journal records `kyber_quote_ms`,
  `kyber_build_ms` and `batch_sim_ms`; `value_ms` (each auction's own arrival → op sendable, the
  batch sim included) is what paper trading times with.
- **Future work (not yet done): pre-quoting.** The per-auction Kyber quote + build + sim is on the
  critical path. Pre-quoting the likely exits for tracked markets on each head (before an auction
  arrives) and reusing a fresh-enough quote would cut the auction-time latency, at the cost of
  speculative Kyber load; it needs a freshness bound tight enough for a price that moved in the
  auctioned update.

## Adding a protocol

1. Create `bot/src/<protocol>/` and implement `LendingProtocol` (see `morpho/mod.rs`). Choose the
   associated types; keep reads batched; make `evaluate` pure.
2. Implement `MarketInfo` for your market type (`key()` = a stable `B256`, `name()`).
3. Start from `bot/src/testkit.rs`: `Toy` is a complete isolated-market protocol (position =
   (debt, collateral), snapshot = price) and `ToyPool` a complete account-level one (two collateral
   and two debt reserves per account, one option per pair) that the engine and planner tests run
   on. Copy their shape for unit tests of your own impl.
4. Implement `loan_usd`: every debt token you allow needs a USD price model (or its liquidations
   are refused).
5. Bind the application layer: today `runner.rs` (the watch loop, oracle model, Atlas valuation)
   and the dashboard are Morpho-bound and uses `morpho::MorphoEngine`; add the same alias for yours, a
   config section like `[morpho]`, and your own loop (see "Scope" above).
6. If the protocol has no liquidate callback (its liquidation pulls the repayment up front),
   implement `DirectLiquidation` (`liquidation_call`) and use `flash_venue::FlashLiquidatorVenue`
   with a 0-fee `FlashSource` (`MorphoBlue` or `UniswapV4`; a new source also needs a
   FlashLiquidator.sol branch): the planner, simulation and FlashLiquidator.sol are reused as they
   are. `testkit::Toy` implements `DirectLiquidation`; see the `flash_venue` and `planner` tests.

## Adding a DEX (aggregator)

Implement `dex::Dex` (see `impl Dex for Kyber` in `router.rs`): `name()` becomes `Plan::router`,
`quote_within` must respect the gas budget, `build` returns the call the venue will make.
`testkit::MockDex` (pays 2 loan units per collateral unit, routed via `0x22..`) and the
`generic_tests` in `planner.rs` show the full aggregator path, including the allowlist check.
Add the router to `exec.allowed_targets` and to the contract allowlist.

## Adding a venue (liquidation contract)

Implement `venue::LiquidationVenue<L>` for your contract: `encode` must return the exact calldata
you will send and the byte slot of `minProfit`; `simulate` must run that calldata at the next block
with the given `fee` (reuse `sim::simulate_call`, and `LiquidatorCode` to inject the contract
before it is deployed); return `packed_routes() = false` if the contract has no packed-route entry,
and override `swap_recipient()` if the swaps run in a contract other than `target()`. `testkit::MockVenue`
tags packed vs call routes and never simulates; `FlashLiquidatorVenue` and `AtlasBid` are two
complete examples. Write a Foundry security suite in the spirit of the tables above before any
deployment.

## Commands

```bash
cd contracts && forge test -vv                      # contract tests (fork tests use BASE_RPC_URL)
cd contracts && forge test --no-match-contract Fork # security suites only, no RPC needed
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
