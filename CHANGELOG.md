# Changelog

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
