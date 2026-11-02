# Changelog

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
