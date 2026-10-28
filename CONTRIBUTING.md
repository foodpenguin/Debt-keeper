# Contributing

[繁體中文](CONTRIBUTING.zh-TW.md)

Thanks for helping. This project moves real money when someone runs it live, so correctness and
safety come before features.

## Before you start

- Read [docs/architecture.md](docs/architecture.md): the trait seams (`LendingProtocol`,
  `LiquidationVenue`, `Dex`) and why the core is shaped the way it is.
- For a larger change (a new protocol, a new venue, a change to the engine or the contract), open an
  issue first to agree on the approach.
- The chain plumbing (RPC pool, executor, simulation, gates, bidding) lives in `onchain-core`, a
  separate repository checked out next to this one; a change there is a pull request there.

## Development setup

See [docs/setup.md](docs/setup.md). In short: Rust (stable), Foundry, `git submodule update --init`,
an RPC endpoint in `.env` for fork tests.

## Checks every pull request must pass

```
cd bot && cargo check --all-targets     # 0 warnings
cd bot && cargo test
cd contracts && forge build && forge test   # fork tests need BASE_RPC_URL; -j 1 on free plans
```

Changes to anything that sends transactions must also pass `cargo test e2e -- --ignored` on a local
anvil fork.

## Guidelines

- **Extend through the traits**, not with special cases in the core. A new protocol, DEX or venue
  comes with a test that drives the engine and/or the planner with it (see `testkit.rs`).
- **Keep the hot path fast**: batched reads, incremental refresh, gas-budgeted routing and the
  same-block path are part of the design. If you must move an optimisation, move it, do not drop it.
- **Contracts**: every new external entry point or callback needs tests for who may call it and
  when (see `MorphoLiquidator.security.t.sol`). Keep the minimum-profit check and the target
  allowlist on every path.
- **Config, not code**, for addresses of venues that can migrate.
- **Comments in English**, matching the surrounding style. Docs come in English and Traditional
  Chinese (`*.zh-TW.md`): update both, or say in the PR that a translation is pending.
- **No secrets and no private strategy data** in commits: no keys, no RPC URLs with API keys, no
  personal wallet addresses, no tuned parameters you do not want to publish.
- Never send mainnet transactions from tests or tooling.

## License

By contributing you agree that your contribution is licensed under the
[GNU AGPL v3.0 or later](LICENSE), like the rest of the project.
