# liquidation-framework

[繁體中文](README.zh-TW.md)

A framework for building on-chain **liquidation bots** in Rust ([alloy](https://github.com/alloy-rs/alloy))
and Solidity ([Foundry](https://getfoundry.sh)). The block loop, health-factor engine, liquidation
planner, exit routing, simulation and transaction sending are generic; a lending protocol, a DEX
aggregator and a liquidation contract plug in through traits. **Morpho Blue on Base** is the first
implementation, with a capital-free liquidation contract (the repayment is funded inside Morpho's
liquidate callback).

> **Status:** experimental. Nothing here is financial advice and nothing is tuned for you: the
> example configuration uses neutral values. Run it in shadow mode, test sends on an anvil fork,
> and expect to lose gas while you learn.

**Scope today.** Generic: the HF engine, the planner, the venue / DEX / funding seams and their
venues, the executor and the RPC plumbing. Still Morpho-bound: the application loop in `main.rs`
(block and same-block paths, send path, pre-flight), the oracle price model, the Atlas valuation,
the dashboard and the loss analysis. A new protocol reuses the generic core, but needs its own
application loop for now (see [docs/architecture.md](docs/architecture.md#scope-what-is-generic-today)).

## What you get

- **Incremental health-factor engine** (`Engine<P, L: LendingProtocol>`): positions are loaded once,
  then only the ones touched by events are re-read, with one batched call per block for all market
  snapshots; borrower discovery from the protocol's indexer runs in the background.
- **Protocol-neutral planner** (`Planner<L, V: LiquidationVenue<L>, A: Dex>`): sizes the seize,
  routes the collateral sale through a self-built multi-pool V3/CL router (exact, gas-budgeted) and/or
  an external aggregator, simulates the exact transaction at the fee it will pay and scores it in
  USD net of gas and L1 fees; a bid is re-simulated at its own priority before it is sent.
- **Same-block path** on Base Flashblocks: react to oracle updates inside the block being built.
- **Execution plumbing**: several liquidation txs in flight at once (consecutive nonces), candidates
  planned concurrently and each sent as soon as its plan is ready, multi-endpoint RPC pool (race,
  failover, per-endpoint method limits), newHeads over several WebSockets, nonce coordination
  between bots that share a hot wallet, failure cooldown, warm broadcast connections, kill switch.
- **Two ways to fund the repayment** (`funding.rs`): the protocol's own liquidate callback
  (Morpho, no borrowing), or a flash source for protocols without one (`FlashLoan`: Morpho Blue;
  `FlashAccounting`: Uniswap V4) through `FlashLiquidator.sol` and `flash_venue.rs`.
- **Chainlink SVR / Atlas** auction observation and valuation (observe only), plus a solver venue
  that wraps a planned liquidation into an Atlas solver call with an ETH bid and simulates it
  (`atlas_venue.rs`, `AtlasLiquidationSolver.sol`; signing and submitting solver operations are not
  implemented). A SQLite + HTML dashboard and a live position page.
- **Contracts with security suites**: `MorphoLiquidator.sol` (executor-only, callback-guarded,
  owner-allowlisted swap targets, minimum-profit check, packed calldata entry; gas tests),
  `FlashLiquidator.sol` (flash-funded, one-shot funding callbacks, separate allowlists for the
  protocol and the swap routers) and `AtlasLiquidationSolver.sol` (Atlas-only entry, ETH bid, profit checked after the bid).

## Layout

```
bot/src/
  protocol.rs   LendingProtocol: the seam a lending protocol implements
  engine.rs     generic health-factor engine
  venue.rs      LiquidationVenue: how a liquidation is encoded and simulated
  dex.rs        Dex: an external swap aggregator
  planner.rs    generic liquidation planner
  funding.rs    flash sources: FlashLoan / FlashAccounting (MorphoBlue, UniswapV4)
  morpho/       Morpho Blue (first LendingProtocol and DirectLiquidation)
  sim.rs        MorphoLiquidator as LiquidationVenue + shared simulation / contract injection
  flash_venue.rs  FlashLiquidator as LiquidationVenue (protocols without a liquidate callback)
  atlas_venue.rs  AtlasLiquidationSolver as LiquidationVenue (SVR solver ops, simulation only)
  router.rs     KyberSwap (first Dex)
  v3router.rs   self-built direct V3/CL router (GridQuoter-based quoting)
  executor.rs   signing, broadcasting, nonce file, cooldown
  rpc.rs heads.rs flashblocks.rs chain.rs num.rs   chain plumbing
  oracle.rs risk.rs atlas.rs   oracle model, risk gate, SVR auctions
  dash/ live.rs loss.rs        dashboard, live page, post-mortems of missed liquidations
  testkit.rs    test doubles (toy protocol, mock venue, mock DEX)
contracts/src/
  MorphoLiquidator.sol        liquidation contract (Morpho callback funds the repayment)
  FlashLiquidator.sol         liquidation contract for callback-free protocols (flash-funded)
  AtlasLiquidationSolver.sol  Atlas solver for SVR auctions
  sim/GridQuoter.sol          quoter injected by state override (simulation only)
  sim/AtlasStub.sol           Atlas escrow stand-in injected by state override (simulation only)
docs/           architecture and setup (English / 繁體中文)
```

## Quick start

See [docs/setup.md](docs/setup.md) for the full guide.

```
cp .env.example .env                              # your RPC URLs (git-ignored)
cd contracts && forge build && cd ..
cd bot && cp config.example.toml config.local.toml   # choose your markets and limits
cargo build --release
./target/release/bot -c config.local.toml watch   # shadow mode: nothing is sent
```

## Extending

- **Another lending protocol**: implement `LendingProtocol` (markets, batched position and snapshot
  reads, events, evaluation, and the planning hooks `exit_tokens` / `loan_decimals` /
  `repay_for_seize` / `collateral_value` / `loan_usd`), then its application loop (today `main.rs`
  is Morpho-bound).
- **Another aggregator**: implement `Dex` (`quote_within` a gas budget, `amount_out`, `swap_gas`,
  `build`).
- **A protocol without a liquidate callback**: implement `DirectLiquidation` (`liquidation_call`)
  and use `FlashLiquidatorVenue` with a 0-fee `FlashSource` (`exec.venue = "flash"` for Morpho). A
  new flash source = a `FlashSource` impl plus a branch in `FlashLiquidator.sol`.
- **Another liquidation contract**: implement `LiquidationVenue` (`target`, `encode` with the
  minimum-profit slot, `simulate` at the given fee, `probe_gas`, `packed_routes` if it has no packed
  entry, `swap_recipient` if the swaps run in another contract).

`testkit.rs` shows each seam implemented by a toy, and tests drive the engine and the planner with
them. The walkthrough is in [docs/architecture.md](docs/architecture.md).

## Safety

- Private keys only in an encrypted keystore (`cast wallet import`), never in code, config, `.env`
  or logs. The hot wallet holds gas only.
- RPC URLs (they contain API keys) live in `.env`; logs show endpoint names only.
- Live mode is always started by the operator. Before going live set a priority-fee cap, a daily
  gas cap, a market allowlist (permissionless lending has markets with fake tokens or oracles) and
  a kill switch.
- Several bots may sign with one hot wallet; they coordinate nonces through one lock file per
  wallet (`~/.local/state/onchain-bots/nonce-<wallet>` by default; leave `exec.nonce_file` empty).

Found a vulnerability? See [SECURITY.md](SECURITY.md).

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the
[GNU Affero General Public License v3.0 or later](LICENSE).

This software is provided without any warranty. Using it on mainnet can lose funds.
