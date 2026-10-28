# liquidation-framework — notes for AI coding assistants and contributors

A framework for building on-chain **liquidation bots** in Rust (alloy) + Foundry. The core
(health-factor engine, planner, executor, RPC pool) is protocol-agnostic; Morpho Blue on Base is the
first implementation. Read `docs/architecture.md` before changing core code.

## Layout
- `bot/` — Rust crate: the `liquidation_framework` library (`lib.rs`) and the Morpho bot binary
  (`main.rs`). Seams: `protocol.rs` (`LendingProtocol` with liquidation `Pair`s / options,
  `DirectLiquidation`), `venue.rs` (`LiquidationVenue`), `dex.rs` (`Dex`), `funding.rs`
  (`FlashLoan`, `FlashAccounting`). Generic core: `engine.rs`, `planner.rs`, `risk.rs`, and the
  shared contract injection in `sim.rs`. First implementations: `morpho/`, `sim.rs`
  (MorphoLiquidator), `flash_venue.rs` (FlashLiquidator), `atlas_venue.rs` + `atlas_submit.rs`
  (AtlasLiquidationSolver; EIP-712 solver ops), `router.rs` (KyberSwap), `v3router.rs` (self-built
  direct router). Test doubles: `testkit.rs` (`Toy` isolated, `ToyPool` account-level).
- `../onchain-core` — the plumbing (rpc, heads, Flashblocks stream, simulation, executor + its task,
  gates, bidding, shared config sections), a separate repository checked out next to this one and
  used by path. Changes there must keep this crate and `arbitrage-framework` building and passing.
- `contracts/` — Foundry. `MorphoLiquidator.sol`, `FlashLiquidator.sol`,
  `AtlasLiquidationSolver.sol`, and the simulation-only `sim/GridQuoter.sol` and `sim/AtlasStub.sol`.
- `docs/` — architecture and setup, in English and Traditional Chinese (`*.zh-TW.md`).

## Commands
- `cd bot && cargo check --all-targets` must stay at **0 warnings**; `cargo test` must pass.
- `cargo test e2e -- --ignored` runs the send path against a local **anvil fork** (needs anvil).
- `cd contracts && forge build && forge test` (fork tests need `BASE_RPC_URL`; use `-j 1` on
  free RPC plans).

## Conventions
- Code comments in English. Knowledge-base docs in English **and** Traditional Chinese; keep the
  two versions in sync when you change one.
- Match the surrounding code: naming, comment density, idioms.
- A new protocol / DEX / venue is a new trait implementation, not special cases in the core. Add a
  test that drives the core with it (see `testkit.rs`).
- Optimisations are relocated, never dropped: batched reads, incremental refresh, gas-budgeted
  routing and the same-block path are part of the design.
- Addresses of venues that can migrate belong in config, not code.

## Safety rules
- Private keys only in an encrypted keystore (`cast wallet import`). Never in code, config, `.env`
  or logs. Never ask for or handle a keystore password; signing is started by the operator.
- RPC URLs contain API keys: they live in `.env` (git-ignored); logs and errors show endpoint
  names only.
- Automated tooling never sends mainnet transactions. `watch --live` and deployments are started
  by the operator. Anything that sends is tested on a local anvil fork first, with
  `extra_broadcast = []`.
- The liquidator contract: only `executor` may call it; callbacks only during its own call; swap
  targets must be owner-allowlisted; a minimum profit is always enforced. Keep the security tests
  in `contracts/test/MorphoLiquidator.security.t.sol` green.
- Before live: priority-fee cap, daily gas cap, market allowlist (exclude markets with untrusted
  tokens or oracles), kill switch.
- Atlas solver ops are only submitted with `[atlas.solver] submit = true` in live mode; bonding
  atlETH is an operator step, never automated.
