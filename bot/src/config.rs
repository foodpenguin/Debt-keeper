use alloy::primitives::{Address, B256};
use anyhow::{Context, Result};
pub use onchain_core::config::{ResolvedRpc, RpcEntry, RpcSettings, SendSettings, anchored};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    /// `env_file`, `[[rpc]]` (or `rpc_http` / `rpc_ws`): see onchain_core::config::RpcSettings.
    #[serde(flatten)]
    pub rpc: RpcSettings,
    /// Max blocks per eth_getLogs call (the strictest endpoint's limit).
    #[serde(default = "default_log_chunk")]
    pub log_chunk: u64,
    /// Polling interval for new blocks in `watch` mode.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// How often to re-fetch the borrower list from the Morpho API.
    #[serde(default = "default_refresh_secs")]
    pub borrower_refresh_secs: u64,
    /// Positions below this HF go into the watchlist log.
    #[serde(default = "default_watch_hf")]
    pub watch_hf: f64,
    /// Minimum estimated gross bonus (in loan-token units) to record a candidate.
    #[serde(default)]
    pub min_bonus: f64,
    /// Positions with less debt than this (loan-token units) are ignored as dust.
    #[serde(default = "default_dust")]
    pub dust_debt: f64,
    /// Accrue interest this many seconds past the current block (~ the next block's timestamp),
    /// so positions that cross HF 1 purely through interest are flagged one block early. Unset =
    /// the observed block time, rounded up to whole seconds (2 s blocks: 2; 200 ms blocks: 1).
    #[serde(default)]
    pub accrual_lookahead_secs: Option<u64>,
    /// Max block range for one getLogs call; a bigger gap triggers a full borrower reload instead.
    #[serde(default = "default_log_range")]
    pub max_log_range: u64,
    pub journal_path: String,
    /// The lending protocol this bot tracks (Morpho Blue, the first impl). Another protocol gets
    /// its own section and `LendingProtocol` impl (the README lists the keys still Morpho-bound).
    pub morpho: MorphoConfig,
    pub exec: ExecConfig,
    #[serde(default)]
    pub router: RouterConfig,
    #[serde(default)]
    pub flashblocks: FlashblocksConfig,
    #[serde(default)]
    pub atlas: AtlasConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
}

/// `bot dashboard`: durable history (SQLite) + a local HTML page, built from the journal and the
/// run log. Paths are relative to the working directory (bot/).
#[derive(Debug, Deserialize, Clone)]
pub struct DashboardConfig {
    #[serde(default = "default_dash_db")]
    pub db_path: String,
    #[serde(default = "default_dash_html")]
    pub html_path: String,
    /// The `watch` run log (the tmux redirect target).
    #[serde(default = "default_dash_log")]
    pub log_path: String,
    /// `[[rpc]]` name for the dashboard's own lookups (wallet balance, Atlas winners of journaled
    /// liquidations). Pick one the bot barely uses. None: those panels stay empty.
    #[serde(default)]
    pub rpc: Option<String>,
    /// Daily copy of the database and the journal (e.g. on another disk).
    #[serde(default)]
    pub backup_dir: Option<String>,
    /// Per-block rows kept this long; hourly aggregates, events, liquidations, auctions and RPC
    /// usage are kept forever.
    #[serde(default = "default_dash_raw_days")]
    pub raw_block_days: u64,
    /// `watch` writes the positions closest to liquidation here every block (`bot live` serves it).
    #[serde(default = "default_live_path")]
    pub live_path: String,
    #[serde(default = "default_live_top")]
    pub live_top: usize,
    /// `bot live` listens on 127.0.0.1:live_port (this machine only).
    #[serde(default = "default_live_port")]
    pub live_port: u16,
    /// Contracts labeled in other liquidators' exit paths (routers, ...) beyond the built-in
    /// aggregators. In the config, not the code, so a DEX migration is a config edit.
    #[serde(default)]
    pub contracts: Vec<ContractLabel>,
    /// Pool factories labeled in exit paths beyond [router.factories] (e.g. V2-style ones).
    #[serde(default)]
    pub factories: Vec<ContractLabel>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContractLabel {
    pub name: String,
    pub address: Address,
    /// "router" | "aggregator" | "rfq" (contracts only; ignored for factories).
    #[serde(default = "default_label_kind")]
    pub kind: String,
}

fn default_label_kind() -> String {
    "router".into()
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self { db_path: default_dash_db(), html_path: default_dash_html(), log_path: default_dash_log(), rpc: None, backup_dir: None, raw_block_days: default_dash_raw_days(), contracts: vec![], factories: vec![], live_path: default_live_path(), live_top: default_live_top(), live_port: default_live_port() }
    }
}

fn default_dash_db() -> String {
    "data/metrics.db".into()
}
fn default_dash_html() -> String {
    "data/dashboard.html".into()
}
fn default_dash_log() -> String {
    "data/live_run.log".into()
}
fn default_dash_raw_days() -> u64 {
    14
}
fn default_live_path() -> String {
    "data/positions_live.json".into()
}
fn default_live_top() -> usize {
    50
}
fn default_live_port() -> u16 {
    8787
}

/// Chainlink SVR / Atlas solver auctions. Phase 0 = observe only: every Base auction for our
/// DappControl is valued (what we could have bid) and journaled; nothing is ever submitted.
/// Needs [flashblocks] enabled (it shares its price model and event loop).
#[derive(Debug, Deserialize, Clone)]
pub struct AtlasConfig {
    #[serde(default)]
    pub observe: bool,
    #[serde(default = "default_atlas_ws")]
    pub ws: String,
    /// Base DappControl of the Chainlink OEV auctions.
    #[serde(default = "default_atlas_control")]
    pub control: Address,
    /// Hypothetical bid = (bundle net value) x (1 - margin).
    #[serde(default = "default_atlas_margin")]
    pub margin: f64,
    /// Positions planned per auction (most valuable first).
    #[serde(default = "default_atlas_plans")]
    pub max_plans: usize,
    /// Gas one solver op may use (the DappControl's solverGasLimit, 6M on Base). The bundle's swap
    /// must fit what its liquidations leave of it.
    #[serde(default = "default_solver_gas")]
    pub solver_gas_limit: u64,
    /// Auction valuations running at once. They share the same-block path's RPC pool: a crash
    /// brings dozens of auctions within seconds, and unbounded valuations would starve it.
    #[serde(default = "default_atlas_concurrent")]
    pub max_concurrent: usize,
    /// Paper trading (dashboard): for every auction a winner was decoded for, what bidding the
    /// bundle net x (1 - m) would have done, for each m here: won (this bid above the winning one,
    /// in ETH), and won in time (valuation done within paper_deadline_ms of the auction).
    #[serde(default = "default_paper_margins")]
    pub paper_margins: Vec<f64>,
    #[serde(default = "default_paper_deadline")]
    pub paper_deadline_ms: u64,
    /// Atlas charges a solver its op's gas at the metacall's gas price plus a surcharge. Read from
    /// the Atlas escrow at start (`getAtlasSurchargeRate`, scale 1e4); this is the fallback.
    #[serde(default = "default_atlas_surcharge")]
    pub gas_surcharge: f64,
    #[serde(default)]
    pub solver: SolverConfig,
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self { observe: false, ws: default_atlas_ws(), control: default_atlas_control(), margin: default_atlas_margin(), max_plans: default_atlas_plans(), solver_gas_limit: default_solver_gas(), max_concurrent: default_atlas_concurrent(), paper_margins: default_paper_margins(), paper_deadline_ms: default_paper_deadline(), gas_surcharge: default_atlas_surcharge(), solver: SolverConfig::default() }
    }
}

/// `[atlas.solver]`: the AtlasLiquidationSolver venue (atlas_venue.rs). With `simulate`, the top
/// position of each valued auction (direct-router plan) is also wrapped into a solver operation
/// with the journaled bid and simulated against an Atlas stand-in; the net after the bid is
/// journaled. With `submit` (live only) that op is also signed and submitted.
#[derive(Debug, Deserialize, Clone)]
pub struct SolverConfig {
    #[serde(default)]
    pub simulate: bool,
    /// Deployed AtlasLiquidationSolver. Unset = injected bytecode.
    #[serde(default)]
    pub address: Option<Address>,
    /// Deployed MorphoLiquidator owned by the solver (executor = profit receiver = solver).
    #[serde(default)]
    pub liquidator: Option<Address>,
    /// The solver operation signer (Atlas `solverOpFrom`). Default: exec.executor.
    #[serde(default)]
    pub signer: Option<Address>,
    #[serde(default = "default_weth")]
    pub weth: Address,
    /// Uniswap V3-style router (exactOutputSingle) that buys the ETH bid with the loan token.
    #[serde(default = "default_bid_router")]
    pub bid_router: Address,
    /// Fee tier of the bid swap pool (loan token / WETH).
    #[serde(default = "default_bid_fee")]
    pub bid_fee: u32,
    #[serde(default = "default_solver_artifact")]
    pub artifact: String,
    #[serde(default = "default_atlas_stub_artifact")]
    pub stub_artifact: String,
    /// Sign the solver operation of each valued auction whose op succeeds with a profit after the
    /// bid, and submit it to `endpoint` (atlas_submit.rs). Only in live mode (exec.mode = "live"
    /// and `watch --live`), with the solver and its liquidator deployed (`address`, `liquidator`),
    /// the signer = the hot wallet, and at least `min_bond_eth` atlETH bonded by it.
    #[serde(default)]
    pub submit: bool,
    #[serde(default = "default_bid_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_min_bond")]
    pub min_bond_eth: f64,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            simulate: false,
            address: None,
            liquidator: None,
            signer: None,
            weth: default_weth(),
            bid_router: default_bid_router(),
            bid_fee: default_bid_fee(),
            artifact: default_solver_artifact(),
            stub_artifact: default_atlas_stub_artifact(),
            submit: false,
            endpoint: default_bid_endpoint(),
            min_bond_eth: default_min_bond(),
        }
    }
}

fn default_bid_endpoint() -> String {
    crate::atlas_submit::SVR_BID_ENDPOINT.into()
}
fn default_min_bond() -> f64 {
    0.01
}
fn default_atlas_surcharge() -> f64 {
    0.1
}
fn default_weth() -> Address {
    "0x4200000000000000000000000000000000000006".parse().unwrap()
}
fn default_bid_router() -> Address {
    "0x2626664c2603336E57B271c5C0b26F421741e481".parse().unwrap()
}
fn default_bid_fee() -> u32 {
    500
}
fn default_solver_artifact() -> String {
    "../contracts/out/AtlasLiquidationSolver.sol/AtlasLiquidationSolver.json".into()
}
fn default_atlas_stub_artifact() -> String {
    "../contracts/out/AtlasStub.sol/AtlasStub.json".into()
}

fn default_atlas_ws() -> String {
    "wss://svr-bid-endpoint.chain.link/ws/solver".into()
}
fn default_atlas_control() -> Address {
    "0xa5E1a36938769cbd5a26f5e19D8FCB379f597c83".parse().unwrap()
}
fn default_atlas_margin() -> f64 {
    0.05
}
fn default_atlas_plans() -> usize {
    5
}
fn default_solver_gas() -> u64 {
    6_000_000
}
fn default_atlas_concurrent() -> usize {
    1
}
fn default_paper_margins() -> Vec<f64> {
    vec![0.01, 0.05, 0.1]
}
fn default_paper_deadline() -> u64 {
    2000
}

/// Same-block mode: react to Chainlink updates inside the block they land in (Flashblocks,
/// ~200 ms sub-blocks) instead of after the block. Removed by Denim (~2026-11): keep it optional.
#[derive(Debug, Deserialize, Clone)]
pub struct FlashblocksConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Flashblocks stream (brotli-compressed JSON over WebSocket).
    #[serde(default = "default_fb_ws")]
    pub ws: String,
    /// `[[rpc]]` names for same-block quotes, simulations and pending receipts, in strict
    /// preference order: the first healthy one serves both a quote and its simulation, so they
    /// read the same node (nodes differ by up to a flashblock). They must support eth_simulateV1
    /// on "pending" (latency differs a lot between providers: measure yours); each
    /// plan also runs on the snapshot block, used whenever the pending state is stale.
    #[serde(default)]
    pub pending_rpc: Vec<String>,
    /// Also send what the risk gate approves (needs `watch --live`). Off: plans are only journaled.
    #[serde(default)]
    pub send: bool,
    /// Candidates planned per oracle update (most valuable first).
    #[serde(default = "default_fb_plans")]
    pub max_plans: usize,
    /// Exit router for same-block plans: "direct" (own V3 router, ~300 ms) or "best" (direct and
    /// Kyber in parallel: deeper exits via prop AMMs / RFQ makers, but Kyber's quote + build add
    /// ~0.5-1 s, several flashblocks).
    #[serde(default = "default_fb_router")]
    pub router: String,
    /// Atlas (Chainlink SVR auctions on Base): its metacalls carry the `transmitSecondary` that
    /// actually moves an SVR feed as Morpho reads it.
    #[serde(default = "default_fb_atlas")]
    pub atlas: Address,
    /// SVR primary with no auction: the proxy serves it once older than this (strictly). 10 s on
    /// the Base SVR feeds checked (an unauctioned update took effect ~12 s = 6 blocks later).
    #[serde(default = "default_svr_cutoff")]
    pub svr_cutoff_secs: u64,
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self { enabled: false, ws: default_fb_ws(), pending_rpc: vec![], send: false, max_plans: default_fb_plans(), router: default_fb_router(), atlas: default_fb_atlas(), svr_cutoff_secs: default_svr_cutoff() }
    }
}

fn default_fb_ws() -> String {
    "wss://mainnet.flashblocks.base.org/ws".into()
}
fn default_svr_cutoff() -> u64 {
    10
}
fn default_fb_atlas() -> Address {
    "0x583dcFef0D240DC80753F0F0B26513feE27D9B77".parse().unwrap() // Atlas v1.6.4 on Base
}
fn default_fb_plans() -> usize {
    3
}
fn default_fb_router() -> String {
    "direct".into()
}

#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    /// "best" (direct + the aggregator in parallel, keep the better), "direct", or "aggregator"
    /// ("kyber" is accepted as an alias: Kyber is the aggregator `Dex` this bot ships).
    #[serde(default = "default_router_mode")]
    pub mode: String,
    #[serde(default = "default_quoter_artifact")]
    pub quoter_artifact: String,
    /// Intermediate tokens for 2-hop paths (collateral -> connector -> loan).
    #[serde(default = "default_connectors")]
    pub connectors: Vec<Address>,
    /// Size grid resolution: sizes k * max_seize / grid_points.
    #[serde(default = "default_grid_points")]
    pub grid_points: u64,
    #[serde(default = "default_max_paths")]
    pub max_paths: usize,
    #[serde(default = "default_factories")]
    pub factories: Vec<crate::v3router::FactoryEntry>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            mode: default_router_mode(),
            quoter_artifact: default_quoter_artifact(),
            connectors: default_connectors(),
            grid_points: default_grid_points(),
            max_paths: default_max_paths(),
            factories: default_factories(),
        }
    }
}

fn default_router_mode() -> String {
    "best".into()
}
fn default_quoter_artifact() -> String {
    "../contracts/out/GridQuoter.sol/GridQuoter.json".into()
}
fn default_connectors() -> Vec<Address> {
    vec!["0x4200000000000000000000000000000000000006".parse().unwrap()] // WETH
}
fn default_grid_points() -> u64 {
    20
}
fn default_max_paths() -> usize {
    8
}
/// No built-in DEX factories: venue addresses live in config as `[[router.factories]]`, so a DEX
/// migration (e.g. Aerodrome -> Aero) is a config edit, not a
/// rebuild. Without entries the direct router is disabled and only the aggregator is used.
fn default_factories() -> Vec<crate::v3router::FactoryEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_top_level_morpho_keys_get_a_migration_message() {
        let legacy: toml::Table = "morpho = \"0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb\"\nmorpho_api = \"x\"\n[[markets]]\nname = \"a\"\n".parse().unwrap();
        let err = Config::reject_legacy_layout_of(&legacy).unwrap_err().to_string();
        assert!(err.contains("moved into [morpho]") && err.contains("morpho_api") && err.contains("\"morpho\""), "{err}");
        let current: toml::Table = std::fs::read_to_string("config.example.toml").unwrap().parse().unwrap();
        assert!(Config::reject_legacy_layout_of(&current).is_ok());
    }

    #[test]
    fn shipped_example_parses_with_the_morpho_section() {
        let cfg = Config::load("config.example.toml").unwrap();
        assert_eq!(cfg.morpho.address, "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb".parse::<Address>().unwrap());
        assert_eq!(cfg.morpho.markets.len(), 1);
        assert!(cfg.morpho.markets.iter().any(|m| m.name == "WETH/USDC"));
    }

    #[test]
    fn missing_env_skips_keyed_endpoints_but_keeps_public_ones() {
        let cfg: Config = toml::from_str(
            r#"
            chain_id = 8453
            journal_path = "x"
            env_file = "definitely-missing.env"
            morpho = { address = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb", api = "x", markets = [] }
            [[rpc]]
            name = "keyed"
            http_env = "BOT_TEST_NO_SUCH_VAR"
            [[rpc]]
            name = "public"
            http = "https://example.invalid"
            [exec]
            executor = "0x000000000000000000000000000000000000dEaD"
            liquidator_artifact = "x"
            kyber_api = "x"
            kyber_client_id = "x"
            slippage_bps = 50
            priority_fee_wei = 1
            replan_blocks = 1
            max_plans_per_block = 1
            eth_usd_fallback = 1.0
            "#,
        )
        .unwrap();
        let r = cfg.resolve_rpc().unwrap();
        assert_eq!(r.http.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["public"]);
    }

    #[test]
    fn relative_kill_switch_paths_follow_the_config_file() {
        assert_eq!(anchored("/srv/bot/config.local.toml", "data/KILL"), "/srv/bot/data/KILL");
        assert_eq!(anchored("config.toml", "data/KILL"), "./data/KILL");
        assert_eq!(anchored("/srv/bot/config.toml", "/var/KILL"), "/var/KILL", "absolute stays");
        assert_eq!(anchored("/srv/bot/config.toml", ""), "", "unset stays unset");
    }

    #[test]
    fn default_nonce_path_is_per_wallet_and_cwd_independent() {
        let exec: ExecConfig = toml::from_str(
            r#"
            executor = "0x000000000000000000000000000000000000dEaD"
            liquidator_artifact = "x"
            kyber_api = "x"
            kyber_client_id = "x"
            slippage_bps = 50
            priority_fee_wei = 1
            replan_blocks = 1
            max_plans_per_block = 1
            eth_usd_fallback = 1.0
            "#,
        )
        .unwrap();
        let w: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let p = exec.nonce_path(w);
        assert!(p.ends_with("onchain-bots/nonce-0x1111111111111111111111111111111111111111"), "{p:?}");
        let explicit = ExecConfig { send: SendSettings { nonce_file: "/tmp/x".into(), ..exec.send.clone() }, ..exec };
        assert_eq!(explicit.nonce_path(w), std::path::PathBuf::from("/tmp/x"));
    }

    #[test]
    fn router_typos_are_rejected() {
        let base = r#"
            chain_id = 8453
            journal_path = "x"
            morpho = { address = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb", api = "x", markets = [] }
            [exec]
            executor = "0x000000000000000000000000000000000000dEaD"
            liquidator_artifact = "x"
            kyber_api = "x"
            kyber_client_id = "x"
            slippage_bps = 50
            priority_fee_wei = 1
            replan_blocks = 1
            max_plans_per_block = 1
            eth_usd_fallback = 1.0
        "#;
        let parse = |extra: &str| toml::from_str::<Config>(&format!("{base}\n{extra}")).unwrap();
        assert!(parse("").validate().is_ok(), "defaults are valid");
        assert!(parse("[flashblocks]\nrouter = \"best\"").validate().is_ok());
        let err = parse("[flashblocks]\nrouter = \"drect\"").validate().unwrap_err().to_string();
        assert!(err.contains("flashblocks.router"), "{err}");
    }

    #[test]
    fn venue_settings_are_checked() {
        let base = r#"
            chain_id = 8453
            journal_path = "x"
            morpho = { address = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb", api = "x", markets = [] }
            [exec]
            executor = "0x000000000000000000000000000000000000dEaD"
            liquidator_artifact = "x"
            kyber_api = "x"
            kyber_client_id = "x"
            slippage_bps = 50
            priority_fee_wei = 1
            replan_blocks = 1
            max_plans_per_block = 1
            eth_usd_fallback = 1.0
        "#;
        let parse = |extra: &str| toml::from_str::<Config>(&format!("{base}\n{extra}")).unwrap();
        let d = parse("");
        assert_eq!((d.exec.venue.as_str(), d.exec.flash_funding.as_str(), d.exec.flash_headroom_bps), ("callback", "morpho", 10));
        assert!(d.validate().is_ok());
        assert!(parse("venue = \"flash\"").validate().is_ok(), "Morpho flash loan needs nothing else");
        let direct = "venue = \"flash\"\n[router]\nmode = \"direct\"";
        assert!(parse(direct).validate().unwrap_err().to_string().contains("aggregator exits only"));
        let fb_direct = "venue = \"flash\"\n[flashblocks]\nenabled = true";
        assert!(parse(fb_direct).validate().unwrap_err().to_string().contains("flashblocks.router"), "same-block router defaults to direct");
        assert!(parse("venue = \"flsh\"").validate().unwrap_err().to_string().contains("exec.venue"));
        assert!(parse("venue = \"flash\"\nflash_funding = \"v4\"").validate().unwrap_err().to_string().contains("pool_manager"));
        assert!(parse("venue = \"flash\"\nflash_funding = \"v4\"\npool_manager = \"0x498581fF718922c3f8e6A244956aF099B2652b2b\"").validate().is_ok());
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecConfig {
    /// The keys every bot shares (wallet, mode, keystore, broadcast, kill switch, nonce file,
    /// daily gas budget, failure policy, max_inflight, min_wallet_eth): onchain_core SendSettings.
    /// Reachable directly on ExecConfig (`exec.keystore`) through Deref.
    #[serde(flatten)]
    pub send: SendSettings,
    /// Deployed MorphoLiquidator. Unset = simulate with the bytecode injected via state override.
    pub liquidator: Option<Address>,
    /// Liquidation contract (see funding.rs): "callback" = MorphoLiquidator (the protocol funds the
    /// repayment inside its liquidate callback; no borrowing) or "flash" = FlashLiquidator (the
    /// repayment is flash-borrowed first: the path for protocols without a callback).
    #[serde(default = "default_venue")]
    pub venue: String,
    /// Deployed FlashLiquidator (venue = "flash"). Unset = simulate with injected bytecode.
    #[serde(default)]
    pub flash_liquidator: Option<Address>,
    #[serde(default = "default_flash_artifact")]
    pub flash_liquidator_artifact: String,
    /// Flash source of the "flash" venue: "morpho" (Morpho Blue flash loan) or "v4" (Uniswap V4
    /// flash accounting; needs pool_manager).
    #[serde(default = "default_flash_funding")]
    pub flash_funding: String,
    /// Uniswap V4 PoolManager (FlashLiquidator constructor; flash_funding = "v4").
    #[serde(default)]
    pub pool_manager: Option<Address>,
    /// Flash-borrowed on top of the planned repayment, in bps (interest accrued before inclusion).
    #[serde(default = "default_flash_headroom")]
    pub flash_headroom_bps: u32,
    pub liquidator_artifact: String,
    pub kyber_api: String,
    pub kyber_client_id: String,
    pub slippage_bps: u32,
    pub priority_fee_wei: u128,
    /// How long a liquidation tx stays valid. 4 s = 2 blocks today, 20 blocks after Denim.
    #[serde(default = "default_deadline_secs")]
    pub deadline_secs: f64,
    /// minProfit sent on-chain = simulated profit x this share (protects against state changes).
    #[serde(default = "default_keep")]
    pub min_profit_keep: f64,
    /// gas limit = simulated gas used x this buffer (only gas actually used is paid).
    #[serde(default = "default_gas_buffer")]
    pub gas_limit_buffer: f64,
    /// Plan a given candidate at most once per this many blocks.
    pub replan_blocks: u64,
    /// Max candidates planned per block (largest bonus first).
    pub max_plans_per_block: usize,
    /// Plans needing more gas than this are rejected (Base per-tx cap is ~16.7M).
    #[serde(default = "default_max_gas")]
    pub max_gas: u64,
    /// Swap targets the contract may call (must match the owner's on-chain allowlist).
    #[serde(default = "default_allowed_targets")]
    pub allowed_targets: Vec<Address>,
    /// Reject a plan when simulated profit and the exit quote disagree by more than this share
    /// (fee-on-transfer, manipulated pool, stale quote...).
    #[serde(default = "default_quote_gap")]
    pub max_sim_quote_gap: f64,
    // ---- risk gate (risk.rs)
    #[serde(default = "default_min_net")]
    pub min_net_usd: f64,
    #[serde(default = "default_max_prio")]
    pub max_priority_fee_wei: u128,
    /// Share of the expected net profit a bid may use (on top of priority_fee_wei; see risk.rs).
    #[serde(default = "default_prio_share")]
    pub priority_fee_share: f64,
    /// The share rises linearly from priority_fee_share to this at bid_large_net_usd net (unset =
    /// priority_fee_share throughout): a large liquidation may bid most of its profit.
    #[serde(default)]
    pub bid_share_large: Option<f64>,
    #[serde(default = "default_bid_large")]
    pub bid_large_net_usd: f64,
    /// The competition: the bid_quantile of what past winners paid per USD of gross (within
    /// bid_window_secs) x this plan's gross, outbid by bid_outbid_bps. Fewer than bid_min_samples
    /// winners seen = no model (the share alone).
    #[serde(default = "default_bid_quantile")]
    pub bid_quantile: f64,
    #[serde(default = "default_bid_outbid")]
    pub bid_outbid_bps: u32,
    #[serde(default = "default_bid_samples")]
    pub bid_min_samples: usize,
    #[serde(default = "default_bid_window")]
    pub bid_window_secs: u64,
    /// Priority fee assumed when deciding whether an extra route leg pays for its gas.
    #[serde(default = "default_planning_prio")]
    pub planning_priority_fee_wei: u128,
}

impl std::ops::Deref for ExecConfig {
    type Target = SendSettings;
    fn deref(&self) -> &SendSettings {
        &self.send
    }
}

/// `[morpho]`: the Morpho Blue deployment and the markets to watch.
#[derive(Debug, Deserialize, Clone)]
pub struct MorphoConfig {
    /// The Morpho Blue singleton.
    pub address: Address,
    /// Morpho's GraphQL indexer (borrower discovery).
    pub api: String,
    /// Market whitelist, `[[morpho.markets]]`.
    pub markets: Vec<MarketEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MarketEntry {
    pub name: String,
    pub id: B256,
}

fn default_poll_ms() -> u64 {
    1000
}
fn default_refresh_secs() -> u64 {
    // Events catch new borrowers; the full API walk (~20 s) is only a safety net.
    21_600
}
fn default_watch_hf() -> f64 {
    1.02
}
fn default_log_chunk() -> u64 {
    100
}
fn default_venue() -> String {
    "callback".into()
}
fn default_flash_artifact() -> String {
    "../contracts/out/FlashLiquidator.sol/FlashLiquidator.json".into()
}
fn default_flash_funding() -> String {
    "morpho".into()
}
fn default_flash_headroom() -> u32 {
    10
}
fn default_allowed_targets() -> Vec<Address> {
    // KyberSwap MetaAggregationRouterV2 on Base
    vec!["0x6131B5fae19EA4f9D964eAc0408E4408b66337b5".parse().unwrap()]
}
fn default_quote_gap() -> f64 {
    0.25
}
fn default_min_net() -> f64 {
    0.5
}
fn default_max_prio() -> u128 {
    1_000_000_000 // 1 gwei
}
fn default_prio_share() -> f64 {
    0.3
}
fn default_bid_large() -> f64 {
    1_000.0
}
fn default_bid_quantile() -> f64 {
    0.9
}
fn default_bid_outbid() -> u32 {
    500
}
fn default_bid_samples() -> usize {
    5
}
fn default_bid_window() -> u64 {
    7 * 86_400
}
fn default_deadline_secs() -> f64 {
    4.0
}
fn default_keep() -> f64 {
    0.5
}
fn default_gas_buffer() -> f64 {
    // gasUsed from eth_simulateV1 is after refunds (up to 20% lower than the peak the tx needs)
    1.5
}
fn default_planning_prio() -> u128 {
    50_000_000 // 0.05 gwei
}
fn default_max_gas() -> u64 {
    8_000_000
}
fn default_dust() -> f64 {
    0.01
}
fn default_log_range() -> u64 {
    1000
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        // A syntax error is left to the real parse below, which reports it with its position.
        if let Ok(t) = text.parse::<toml::Table>() {
            Self::reject_legacy_layout_of(&t).with_context(|| format!("parsing {path}"))?;
        }
        let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
        cfg.exec.send.anchor(path);
        cfg.rpc.anchor(path);
        cfg.validate().with_context(|| format!("checking {path}"))?;
        Ok(cfg)
    }

    /// A config in the pre-`[morpho]` layout (older versions of this bot) would otherwise fail
    /// with an opaque type error on `morpho`, or have its markets silently ignored.
    fn reject_legacy_layout_of(t: &toml::Table) -> Result<()> {
        let mut legacy: Vec<&str> = ["morpho_api", "markets"].into_iter().filter(|k| t.contains_key(*k)).collect();
        if t.get("morpho").is_some_and(|v| v.is_str()) {
            legacy.push("morpho");
        }
        anyhow::ensure!(
            legacy.is_empty(),
            "top-level {legacy:?} moved into [morpho]: morpho -> [morpho] address, morpho_api -> [morpho] api, [[markets]] -> [[morpho.markets]] (see config.example.toml)"
        );
        Ok(())
    }

    /// Values the planner would otherwise misread silently (Planner::plan treats any unknown
    /// router mode as "best").
    fn validate(&self) -> Result<()> {
        self.exec.send.validate()?;
        const ROUTERS: [&str; 4] = ["best", "direct", "aggregator", "kyber"];
        for (key, v) in [("router.mode", &self.router.mode), ("flashblocks.router", &self.flashblocks.router)] {
            anyhow::ensure!(ROUTERS.contains(&v.as_str()), "{key} = {v:?}: must be one of {ROUTERS:?}");
        }
        let e = &self.exec;
        anyhow::ensure!(["callback", "flash"].contains(&e.venue.as_str()), "exec.venue = {:?}: must be \"callback\" or \"flash\"", e.venue);
        anyhow::ensure!(["morpho", "v4"].contains(&e.flash_funding.as_str()), "exec.flash_funding = {:?}: must be \"morpho\" or \"v4\"", e.flash_funding);
        anyhow::ensure!(e.venue != "flash" || e.flash_funding != "v4" || e.pool_manager.is_some(), "exec.flash_funding = \"v4\" needs exec.pool_manager");
        // The flash venue has no packed-route entry: a "direct" exit could never be planned for it.
        if e.venue == "flash" {
            anyhow::ensure!(self.router.mode != "direct", "exec.venue = \"flash\" takes aggregator exits only: router.mode must be \"best\" or \"aggregator\"");
            anyhow::ensure!(!self.flashblocks.enabled || self.flashblocks.router != "direct", "exec.venue = \"flash\" takes aggregator exits only: flashblocks.router must be \"best\" or \"aggregator\"");
        }
        Ok(())
    }

    /// Seconds to accrue interest ahead of the current block at this block time.
    pub fn lookahead_secs(&self, block_time_secs: f64) -> u64 {
        self.accrual_lookahead_secs.unwrap_or_else(|| (block_time_secs.ceil() as u64).clamp(1, 2))
    }

    pub fn resolve_rpc(&self) -> Result<ResolvedRpc> {
        self.rpc.resolve()
    }
}
