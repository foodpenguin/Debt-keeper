# Changelog

## v0.4.3 (2026-11-20)

Atlas bids are now submitted over the bid endpoint's WebSocket. **Fix for live bidding:** the first
live submissions were refused with -32601 "Method not found", because the SVR gateway's HTTP root
does not have `solver_submitSolverOperation`; only its solver WebSocket does.

- `[atlas.solver] endpoint` now defaults to `wss://svr-bid-endpoint.chain.link/ws/solver`, and an
  `http(s)://` endpoint is refused at config load when `submit = true`. A config that set the old
  HTTP endpoint explicitly must drop it or change it to the WebSocket.
- `atlas_submit::BidSocket`: one standing socket for submissions (separate from the auction feed),
  connected at start, pinged while idle and reconnected in the background; answers matched by
  JSON-RPC id (number or numeric string; an error with `"id": null` goes to the only request in
  flight). Each submission waits at most `SUBMIT_TIMEOUT` (1 s). Accepted = `"result": null` (or
  `"null"`); an `error` = refused; never written = not submitted; written but unanswered (no answer
  in time, or the socket lost after the write) = treated as submitted (journal
  `answer: "unanswered: <why>"`, slot and liability kept, Atlas result tracked). A late answer is
  logged (`ATLAS solver op refused after its submission timed out`).
- Dead-socket detection: a written request unanswered for 3 s, 15 s of silence (pings every 5 s),
  or a write not done in 1 s reconnects. Backoff (250 ms doubling to 10 s) is reset only by a
  connection that lasted 30 s, so an endpoint that accepts and closes is not hammered.
- An auction's remaining ops are not tried once one failed with the socket down (each would wait
  out the timeout).
- Start: `ATLAS bid socket not connected after 10 s` is logged loudly if the socket cannot connect
  (not fatal; it keeps reconnecting).
- Watchdog: mails `ATLAS solver op submission failed`, a late refusal, an unmatched endpoint error,
  `ATLAS bid socket not connected`, and the bid socket down for `BID_DOWN_SECS` (default 120 s) then
  back; before, refused submissions only showed up as "liquidations and none of our txs".
- Tests: mock-socket unit tests (answer, refusal, string/null ids, silence, late answer, drop after
  the write + reconnect, half-open reconnect, accept-then-close backoff, unreachable) and an
  `--ignored` network test that sends an op signed by a random unbonded key for an unknown auction
  to the real endpoint and requires "Auction not found" (parsed), not a method/format error.

## v0.4.2 (2026-11-19)

Atlas exit-gas optimisation: the batch solver op now chooses its exit by value net of gas and bids
what it really saves. No config or contract change.

- Aggregator exit: the gas ladder's rungs are now chosen by value **net of gas**, not by output. In
  an Atlas bundle (priced at the auction's ~15 gwei) each rung is scored like the direct route, so a
  forcing rung that gives up a little output for far less gas can win when the extra gas costs more
  than the extra output. Off the auction path (the block / single-exit path, no auction gas price)
  the behaviour is unchanged: the highest-output rung within the budget. New `Dex::quotes_within`
  and `Dex::rung_gas_price_wei` default trait methods (the trait names no protocol/vendor); the rungs
  and which one was selected are journaled in the bundle row as `agg_rungs`.
- Atlas bid: the batch op's bid is now computed from the **simulated** gas, not the conservative
  estimate. The estimate runs ~1.4× the real gas (measured on live batch ops: simulated/estimated
  median ~0.7, range 0.43–1.29), so an estimate-based bid gives up gas savings that could be bid.
  `batch_op` simulates the op once at the estimate bid to measure the real gas, reprices the bid on
  that simulated gas × `SIM_GAS_SAFETY` (raising it when the estimate ran loose, lowering it when the
  sim ran dearer), and **simulates the repriced op again** before it can be sent — raising the bid
  enlarges the bid swap, which the first simulation did not price, so the bid is never sent on faith;
  `minProfit` still guards the real net on-chain. The conservative estimate still backs the swap
  budget and the bond's gas liability; only the bid changed. The journal records both bases
  (`bid_basis_est_usd`, `bid_basis_sim_usd`) and which was used (`batch_est`/`batch_sim`). If the
  re-bid cannot be priced or does not simulate, the estimate-bid op (proven by the first simulation)
  is kept.

## v0.4.1 (2026-11-18)

Morpho markets whose oracle is a `MetaOracleDeviationTimelock` are now valued on the Atlas path, and
no market is left on the block path without a logged reason. No config or contract change.

- Oracle model: support Morpho's `MetaOracleDeviationTimelock`. A market whose oracle is a deviation
  timelock (detected by a non-reverting `currentOracle()`) is now modelled through whichever
  underlying oracle it currently selects (one level deep, still cross-checked against the
  meta-oracle's own `price()`), instead of being silently left on the block path. Its
  `currentOracle()` is re-read at a low frequency (`META_RECHECK_HEADS` heads; a primary↔backup
  switch has a public timelock of hours), and on a switch the market drops to the block path and is
  rebuilt against the new oracle.
- Oracle model: every market left on the block path is now logged at start with a reason (not a
  MorphoChainlinkOracleV2/meta oracle, an ERC-4626 vault conversion factor, a failed feed read, or a
  model that disagrees with `price()`), and the load line lists them under `unmodelled`. There is no
  longer any silent fallback.
- Log: in live mode with `[atlas.solver] submit`, the Atlas solver start-up line now says the ops are
  "simulated, then signed and submitted when profitable" instead of always claiming they are "never
  signed or sent".

## v0.4.0 (2026-11-17)

Atlas solver ops pay their own gas out of their proceeds, so the bond no longer shrinks with every
win. Needs onchain-core v0.1.5. **Breaking: the solver contract's ABI and the `[atlas.solver]` bond
keys changed — see Migration.**

- Contract: `AtlasLiquidationSolver`'s `solverOpData` gains a fifth field, `gasPayWei`:
  `abi.encode(calls, bidSwap, profitToken, minProfit, gasPayWei)`. The bid swap buys the bid plus
  the gas pay; the solver pays `min(gasPayWei, gasLiability)` in ETH through `reconcile` and approves
  only the rest of Atlas's liability against the bond, so `minProfit` now guards the real net after
  the bid AND the gas. Atlas credits the bond with what the op paid beyond its real gas charge and
  takes any shortfall from it; `gasPayWei = 0` is the previous behavior (all gas from the bond).
  `AtlasStub` reports a gas liability set in its storage slot 0 and keeps the ETH paid, so the
  simulated profit is net of the gas. New security tests (overpay capped at the liability, minProfit
  after the gas, a swap that cannot buy bid + gas reverts, nothing left at rest).
- Verified against the real Atlas escrow on a Base fork (`AtlasBondGas.fork.t.sol`): a real SVR
  metacall replayed with our op in the winner's slot and only AtlasVerification (signatures, call
  chain hash) mocked. The bond threshold Atlas checks before running an op matched the one measured
  on the original metacall (+0.6 %); paying the real gas charge in ETH moved the bond by 1.2 % of the
  charge; overpaying credited the bond and underpaying charged it by the difference; `gasPayWei = 0`
  charged the whole gas to the bond.
- Bot: the gas pay is estimated as (op gas + `METACALL_FIXED_GAS`) at the auction's max fee and
  Atlas's and the bundler's surcharges — exactly what every bid basis (batch and single-position op)
  subtracts, from one per-gas price — and recomputed at submission from the simulated gas plus the
  fixed gas: Atlas's expected charge, not the op's buffered gas limit, since the proceeds must buy it
  (any increase comes out of the profit before minProfit; an overpay is credited to the bond, a small
  shortfall taken from it). Before an op is submitted its maximum
  liability — (op gas + userOp gas + dappGasLimit + overhead) at the max fee and surcharges, the
  check Atlas makes before running it — must be covered by the bond (else `skipped:
  "bond_insufficient"`), replacing the daily bond budget; and valuations size each market's bundle
  to what the bond affords instead of refusing it whole. The userOp gas comes from the auction feed
  (`gas`) or `[atlas] user_op_gas`; the dappGasLimit and bundler surcharge are read from the
  DappControl.
- The bond band: `[atlas.solver] bond_min_eth` (below it no op is submitted, alert, nothing moves
  out) and `bond_max_eth` (above it the excess is due out: alert, or with `bond_auto_withdraw = true`
  the hot wallet unbonds it, waits for Atlas's `unbondingCompleteBlock` plus a margin, withdraws it
  and sends the ETH to the solver's PROFIT_RECEIVER — one step at a time through the executor, only
  while nothing else is in flight, live mode and kill switch off; paused while the signer holds
  unbonded atlETH it did not unbond, which `withdraw` would sweep). The bond tx in flight and the
  withdrawn amount waiting to be sent survive restarts, and a tx the executor stops tracking is
  reconciled from its receipt. Bonding stays an operator step.
- Ledger and dashboard: our ops' results are read from Atlas's `SolverTxResult` logs; the ledger's
  Atlas day keeps apart the gas won ops paid, the bond's credits and debits, failed ops' gas,
  deposits and unbonding, attributed from the chain state (bonded moved to unbonding — the bot's or
  an operator's unbond — is not a loss; a rise is a credit only alongside a won op, otherwise a
  deposit that does not offset losses). The daily loss stop counts the bond's net debits. The
  journal gains `atlas_bond`, `atlas_op_result`, `atlas_bond_tx` rows and `gas_pay_eth` /
  `net_after_bid_usd` in solver rows; the dashboard shows the bond against its band and the paper
  result of the one op each auction gets, with its simulated net after bid and gas.
- onchain-core v0.1.5: `send_with_value` (the bond's ETH is sent on through the same executor and
  nonce as every other hot-wallet tx), and `Inflight.value`, reserved by `spendable()`.

### Migration from v0.3.x

1. Redeploy `AtlasLiquidationSolver` (the `solverOpData` layout changed; a v0.3.x solver cannot
   decode v0.4.0 ops) and point `[atlas.solver] address` at it; redo its allowlist
   (`setAllowedTarget`: its MorphoLiquidator and the bid router). The MorphoLiquidator is unchanged.
2. Config `[atlas.solver]`: remove `min_bond_eth` and `atlas_bond_budget_usd` (the bot refuses to
   start while they are present, naming the replacement); with `submit = true`, set `bond_min_eth`
   and `bond_max_eth` (required, no defaults, `0 < min < max`), and optionally `bond_auto_withdraw`.
   Size the bond to cover the largest op you want to send (see docs/architecture.md, "The bond").
3. Optional `[atlas] user_op_gas` (default 500000) if your auction feed omits the userOp gas.
4. An existing ledger file loads as is (its Atlas day starts empty).

## v0.3.2 (2026-11-17)

Fixes from the v0.3.1 GCP shadow run (2026-11-08..13). Needs onchain-core v0.1.4.

- Restart & alerting: `debt-keeper-shadow.service` never gives up restarting
  (`StartLimitIntervalSec=0`) with a 15 s→5 min back-off (`RestartSteps`/`RestartMaxDelaySec`,
  systemd ≥ 254) — a flaky endpoint tripped a crash loop that hit the old 10-starts/10-min limit and
  stopped the bot for 4 days. `watchdog.sh` re-mails the downtime alert every `DOWN_REALERT_HOURS`
  and words it by `MODE` (shadow restarts itself; live needs a person with the keystore password).
- Survive a broken RPC without exiting: a head whose block number is implausibly far beyond the
  freshly read tip is dropped (`head_sanity_gap`) — a single endpoint reporting ~8.7M ahead tripped a
  full reload whose multicalls then all failed "from block > latest"; `load_markets` retries
  empty/garbage market data (a desynced endpoint the pool does not fail over); the full reload is
  non-fatal.
- Diagnostics: the solver error is no longer truncated to 200 chars — the revert data is read from
  the simulation error itself and decoded in full, recursing into the
  `CallFailed`/`SwapFailed`/`BidSwapFailed` nested reasons and `Error(string)`/`Panic`; only the
  error text journaled after it is capped (4096 chars). Quoted or punctuated revert hex in a node
  error is found too.
- Atlas op reliability: a Kyber build that fails on a route using an RFQ/PMM source
  (`kyber_rfq_exclude`) re-quotes over pools only — decided by the route's sources, not the error
  text, so a failure on a pools-only route (e.g. a WAF 403) costs no extra round trip, and a timeout
  is not retried; a fallback is tagged in the journal on the batch exit as on the single-position
  one. The aggregator exit's gas carries extra safety and a batch op whose *simulated* gas exceeds
  `bundle_gas_headroom × solver_gas_limit` is not sent (an under-estimated Kyber exit ran 5.95M
  against a 4.9M estimate, near the 6M cap); the bid swap picks the deepest loan-token/WETH pool per
  loan token (`bid_fee_tiers`/`bid_factory`) instead of one fixed fee.
- Latency: the batch op simulates concurrently with the per-position plans (which are diagnostics
  plus the single-position fallback), so slow plans no longer delay the op that gets sent; Kyber
  calls have a time budget (`kyber_timeout_ms`, 700 ms); the Atlas valuation pool hedges
  `eth_simulateV1` immediately (onchain-core `with_eager_hedge`), at most
  `eager_hedge_max_in_flight` (2) at a time so a crash burst does not double the pool's load. The
  journal gains `kyber_quote_ms`, `kyber_build_ms` and `batch_sim_ms`. `value_ms` now runs to the op
  being sendable (the batch sim included, timed from each sibling auction's own arrival), so it is
  later than v0.3.1's figure for the same auction; paper trading keeps timing with it.

## v0.3.1 (2026-11-09)

- onchain-core v0.1.3: a reverted simulation now reports its revert data (it read as a bare "0x").
- Atlas batch op: the aggregator (Kyber) exit route is built for the solver's own MorphoLiquidator,
  where the batch swaps, instead of the planner's main venue. Built for the main venue, the proceeds
  landed in another contract and Morpho could not pull the flash loan back, so every batch with an
  aggregator exit reverted in simulation (seen in shadow on the first 20-position cbXRP auction,
  2026-11-08). `Planner::quote_bundle` takes the exit holder (`None` = the venue's swap recipient);
  direct-route batches were not affected.

## v0.3.0 (2026-11-08)

Atlas batch solver: one solver op liquidates several positions of a market in one Morpho flash loan
and sells all the seized collateral in a single exit, so the bid and the op finally match the bundle
the valuation prices (before, the op carried only the first position's single-route liquidation, so
none of the multi-position auctions paper trading "won" could actually have been won).

- Contract (`MorphoLiquidator.liquidateBatch`, ABI + packed fallback tag 0x01): a Morpho `flashLoan`
  funds N `liquidate` calls (empty data, repaid through the standing allowance) on one market, then
  one exit (direct route, must spend exactly the sum seized; or allowlisted aggregator steps) sells
  all the collateral. The flash callback is guarded like `FlashLiquidator` (an `inBatch` transient
  flag, the Morpho sender, once, `NotFunded`); the received collateral must equal the sum seized;
  the same minProfit, allowlist and no-funds-at-rest guarantees as the single-position paths. New
  batch security tests; existing ones stay green.
- Gas (measured on the 2026-11-07 02:53 cbXRP auction, archive fork, `AtlasBundleGas.fork.t.sol`):
  a 30-position batch op costs **1.53M gas minus the exit swap** (~34k per extra position), vs 6.19M
  for the old per-position design (over the 6M `solverGasLimit`); with a real 2-hop cbXRP exit, 1 →
  773k, 30 → 1.76M. Constants recalibrated: `BUNDLE_OVERHEAD_GAS` 600k → 500k,
  `BUNDLE_GAS_PER_POSITION` 110k → 40k, new `METACALL_FIXED_GAS` 430k (the metacall gas outside the
  solver call that Atlas still charges the winner — a cost, not part of `solverGasLimit`).
- Bot: the valuation now builds an executable `BundlePlan` (positions at full seize, flash-loan
  size, one full-size exit) and the solver op is the batch op. The bid is the estimate
  (`bid_basis = "batch_est"`: gross − (gas estimate + metacall fixed) × the auction's gas price);
  the simulation only gates the send and its gas/profit are journaled to recalibrate the constants.
  Bonus floor (`[atlas] min_bonus_usd`, default 0 = off) and a gas floor (skip when the estimated
  gas already exceeds the gross); the exits are weighed at the auction's gas price, where gas
  dominates. New `[atlas]` keys: `max_bundle_positions` (20), `flash_headroom_bps` (5),
  `min_bonus_usd` (0). v1 does not partial-liquidate and does not merge markets.
- Kyber routing: the dead 100-gwei ladder rung becomes a forcing ~1000-gwei penalty that reliably
  collapses a split route to one path fitting the Atlas cap (at the real ~15 gwei auction price the
  old rung was too weak, and whether a route collapses turned out to depend mostly on transient
  `pmm-19` RFQ depth); `gasInclude=true` on every `/routes` call.

## v0.2.4 (2026-11-08)

- Dashboard backlog drain: `bot dashboard --drain` keeps looking up liquidation txs until the
  backlog is cleared, instead of one `LOOKUPS_PER_PASS` (40) batch. A busy day leaves hundreds of
  txs behind and the per-minute loop only clears 40 a pass, so winners for most auctions stayed
  `null` on a host that ran the pass once (it is not a decode bug). `scripts/dashboard_loop.sh`
  gains `BIN` / `WORKDIR` overrides (render a copied run, e.g. a shadow snapshot, with another
  build) and a one-shot `once` / `drain` mode. The periodic loop is unchanged: one gentle batch per
  pass.

## v0.2.3 (2026-11-06)

Fixes from the v0.2.2 post-Denim shadow run (GCP, 2026-11-06).

- SVR primaries in flight are queued per aggregator instead of a single slot. When a public primary
  for an aggregator arrived before the previous one had passed its cutoff, the newer one overwrote
  the older; the chain still served the older at its cutoff, so the model read the resulting price
  change as a missed oracle update (`oracle logs missed an update` — mostly ETH/USD in a fast
  market, i.e. when accurate prices matter most). The model now keeps each in-flight primary until
  it takes effect or a later round supersedes it, and each head serves the newest primary past its
  cutoff (`latestRoundData` only moves forward, so an older expired or an un-auctioned skipped round
  is never re-served). Regression tests cover two primaries in flight, a head gap past both cutoffs,
  an auction for the newer round, and a redelivered log. The local shadow snapshot ends before the
  window the run observed, so this is verified by unit test rather than by log replay.

## v0.2.2 (2026-11-06)

Fixes from the first post-Denim shadow run (GCP, 2026-11-02..06).

- Per-head snapshot traffic: the market snapshot `eth_call` (the bulk of the VM's egress under
  200 ms blocks) is reused on quiet heads instead of refetched every block. A fresh snapshot is
  taken only when a log touched a market, an oracle update landed (log or an SVR primary past its
  cutoff), or the last real snapshot is older than `[exec] snapshot_max_age_ms` (default 1000);
  other heads reuse it and accrue interest locally. Only a genuinely fetched snapshot feeds the
  Atlas cross-check, so throttling does not create false missed-update reports. A non-Chainlink
  (e.g. ERC-4626) rate may lag up to that bound.
- SVR cutoff from the chain: the window an unauctioned SVR primary waits before the proxy serves it
  is read per DualAggregator from its `CutoffTimeSet(uint32)` log (added to the `oracle_logs`
  subscription, same addresses) instead of trusting a single config constant. `[atlas]
  svr_cutoff_secs` is now only the startup default, used per aggregator until its first log. The
  aggregator exposes no getter for the value (checked on the Base DualAggregator bytecode), so the
  log is the source; the `ts > primary_ts + cutoff` comparison stays strict as measured on-chain and
  is re-verified on the next shadow run.

- Dashboard clock: the `blocks` table was never populated — the per-head log line is
  `runner: block block=...` but ingest matched a stale `bot: block block=` prefix — so times were
  extrapolated from `(0, now)` at a fixed 2 s and rendered blocks years in the future. Ingest now
  matches the real line, and the clock reads the real per-block timestamps (interpolating across the
  200 ms fork, extrapolating with the measured edge spacing rather than a constant 2 s).
- `head_age_ms`: block-number-derived (places each block within its second), so it reads the true
  lag under 200 ms blocks instead of jittering 0..1000 ms. The watchdog's `HEAD_LAG_MS` default and
  summary baseline were updated.
- Watchdog: `oracle logs missed an update` alerts only when over `MISSED_PER_HOUR` (default 3) land
  in a clock hour; "crash with no send" is gated to live mode (shadow never sends); the env-file
  reader accepts inline `# comments`.
- `setup_wsl.sh --install-service` and the README check the clock (`chronyc tracking`: Leap Normal,
  offset < 50 ms).
- `BOND_CHECK_HEADS` 50 -> 500: under 200 ms blocks the periodic Atlas-bond read stays near ~100 s
  instead of every ~10 s (`bond_dirty` still forces an immediate read when it matters).
- Expected under 200 ms blocks: the loop processes the newest head and skips the rest, so ~16 % of
  heads are skipped (about 4.3 heads/s observed). Head-counted windows now cover less wall time:
  `BLOCK_CLOCK_HEADS` 50 ~= 10 s (block-time estimate), `log_chunk` 100 ~= 20 s of catch-up per
  getLogs chunk. Time-based settings (`deadline_secs`, `bid_window_secs`, `replan_secs`) are
  unaffected; prefer them over the block-count equivalents.

## v0.2.1 (2026-11-02)

Same code as v0.2.0. `bot/Cargo.toml` now carries the release version (v0.2.0 still said 0.1.0).
From this release on, the package version and the tag match.

## v0.2.0 (2026-11-02)

Ready for Base Denim (no Flashblocks or `pending` state, 200 ms blocks). Needs onchain-core v0.1.1.

- `runner`: the watch loop is in the library. `Runner::build(inputs, auction, priority)` takes
  the two bidding policies (`policy::AuctionBidPolicy`, `policy::PriorityBidPolicy`; the defaults
  keep the previous behavior), so an operator's own binary can reuse the loop.
- Atlas without Flashblocks: the price model follows the aggregators' oracle logs on a WebSocket
  subscription (`oracle_logs`, `[atlas] logs_ws`), cross-checked against the previous block's
  oracle prices every head. `[flashblocks] enabled = true` is refused.
- `ledger`: the failed-gas budget, the block path's realized PnL and the Atlas bond's shrinkage
  over the UTC day, kept across restarts. New limits: `[exec] max_tx_cost_eth` (per-tx worst
  case), `daily_loss_limit_usd` (writes the kill switch), `[atlas.solver] atlas_bond_budget_usd`.
- Deadlines and replan intervals can be given in seconds (`replan_secs`), converted with the
  measured block time.
- Aero CL factories in `config.example.toml`.
- Operations:
  - the shadow unit is now `debt-keeper-shadow`;
  - `setup_wsl.sh` takes `BIN`, `CONFIG`, `LOG` and `WORKDIR` overrides;
  - the watchdog has new alerts;
  - `rpc_usage.bytes_out` is shown on the dashboard.
- Config keys that moved: `[flashblocks] atlas` and `svr_cutoff_secs` are refused with a message
  pointing at `[atlas]`.
