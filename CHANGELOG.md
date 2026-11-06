# Changelog

## v0.2.3 (unreleased)

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
