# liquidation-framework

[English](README.md)

用 Rust（[alloy](https://github.com/alloy-rs/alloy)）和 Solidity（[Foundry](https://getfoundry.sh)）
開發鏈上**清算 bot** 的框架。區塊迴圈、健康因子引擎、清算規劃、出場路由、模擬和送單都是通用的；
借貸協議、DEX 聚合器和清算合約透過 trait 接上。第一個實作是 **Base 上的 Morpho Blue**，搭配一個
不需要本金的清算合約（還款資金在 Morpho 的 liquidate callback 裡籌措）。

> **狀態：**實驗性。這裡沒有任何投資建議，也沒有替你調好的參數：範例設定用的是中性值。請先用
> shadow 模式跑、在 anvil fork 上測試送單，並有學習期間會虧 gas 的心理準備。

**目前的範圍。**通用的部分：HF 引擎、planner、venue / DEX / 資金來源三個接口和它們的 venue 實作、
executor 和 RPC 管線。仍然綁定 Morpho 的部分：`main.rs` 的應用迴圈（區塊與同區塊路徑、送單路徑、
pre-flight）、預言機價格模型、Atlas 估值、dashboard 和輸單分析。新的協議可以沿用通用核心，但目前
需要自己的應用迴圈（見 [docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)）。

## 功能

- **增量健康因子引擎**（`Engine<P, L: LendingProtocol>`）：倉位只載入一次，之後只重讀被事件影響的
  倉位；每個區塊用一次批次呼叫讀所有市場快照；從協議 indexer 找借款人的工作在背景執行。
- **與協議無關的 planner**（`Planner<L, V: LiquidationVenue<L>, A: Dex>`）：決定 seize 數量，透過
  自建的多池 V3/CL 路由（精確、受 gas 預算限制）和／或外部聚合器賣出抵押品，以實際會付的 fee
  模擬要送的交易，並以 USD 扣掉 gas 和 L1 費用後評分；出價在送出前會在它自己的 priority 下重新模擬。
- **同區塊路徑**（Base Flashblocks）：在正在產生的區塊裡就對預言機更新做出反應。
- **執行管線**：可同時有多筆清算交易在飛（nonce 連續）、候選同時規劃且各自一規劃完就送出、
  多端點 RPC 池（競速、故障轉移、每個端點的方法限制）、多條 WebSocket 的 newHeads、共用熱錢包的
  多個 bot 之間的 nonce 協調、失敗冷卻、預熱的廣播連線、緊急停止開關。
- **兩種籌措還款的方式**（`funding.rs`）：協議自己的清算 callback（Morpho，不用借錢），或給沒有
  callback 的協議用的閃電資金來源（`FlashLoan`：Morpho Blue；`FlashAccounting`：Uniswap V4），
  透過 `FlashLiquidator.sol` 和 `flash_venue.rs`。
- **Chainlink SVR / Atlas** 拍賣觀察與估值（只觀察），以及一個 solver venue：把規劃好的清算包成
  帶 ETH 出價的 Atlas solver 呼叫並模擬（`atlas_venue.rs`、`AtlasLiquidationSolver.sol`；尚未實作
  solver operation 的簽名與送出）。SQLite + HTML dashboard、即時倉位頁面。
- **附安全測試的合約**：`MorphoLiquidator.sol`（只允許 executor、callback 防護、swap 目標需在
  owner 白名單、最低利潤檢查、壓縮 calldata 入口；附 gas 測試）、`FlashLiquidator.sol`（閃電出資、
  資金 callback 只能一次、協議與 swap router 分開兩份白名單）、`AtlasLiquidationSolver.sol`（只接受
  Atlas 呼叫、ETH 出價、扣掉出價後檢查利潤）。

## 目錄

```
bot/src/
  protocol.rs   LendingProtocol：借貸協議要實作的介面
  engine.rs     通用健康因子引擎
  venue.rs      LiquidationVenue：清算交易如何編碼與模擬
  dex.rs        Dex：外部 swap 聚合器
  planner.rs    通用清算 planner
  funding.rs    閃電資金來源：FlashLoan / FlashAccounting（MorphoBlue、UniswapV4）
  morpho/       Morpho Blue（第一個 LendingProtocol 和 DirectLiquidation）
  sim.rs        MorphoLiquidator 作為 LiquidationVenue + 共用的模擬／合約注入
  flash_venue.rs  FlashLiquidator 作為 LiquidationVenue（沒有清算 callback 的協議）
  atlas_venue.rs  AtlasLiquidationSolver 作為 LiquidationVenue（SVR solver op，只模擬）
  router.rs     KyberSwap（第一個 Dex）
  v3router.rs   自建直連 V3/CL 路由（以 GridQuoter 報價）
  executor.rs   簽名、廣播、nonce 檔、冷卻
  rpc.rs heads.rs flashblocks.rs chain.rs num.rs   鏈的基礎管線
  oracle.rs risk.rs atlas.rs   預言機模型、風控閘門、SVR 拍賣
  dash/ live.rs loss.rs        dashboard、即時頁面、錯過的清算的事後分析
  testkit.rs    測試替身（玩具協議、mock venue、mock DEX）
contracts/src/
  MorphoLiquidator.sol        清算合約（由 Morpho callback 籌措還款）
  FlashLiquidator.sol         給沒有 callback 的協議用的清算合約（閃電出資）
  AtlasLiquidationSolver.sol  SVR 拍賣用的 Atlas solver
  sim/GridQuoter.sol          用 state override 注入的報價合約（只用於模擬）
  sim/AtlasStub.sol           用 state override 注入的 Atlas escrow 替身（只用於模擬）
docs/           架構與安裝（English / 繁體中文）
```

## 快速開始

完整步驟見 [docs/setup.zh-TW.md](docs/setup.zh-TW.md)。

```
cp .env.example .env                              # 你的 RPC URL（不進 git）
cd contracts && forge build && cd ..
cd bot && cp config.example.toml config.local.toml   # 選你的市場和上限
cargo build --release
./target/release/bot -c config.local.toml watch   # shadow 模式：不送任何交易
```

## 擴充

- **其他借貸協議**：實作 `LendingProtocol`（市場、批次讀取倉位與快照、事件、評估，以及規劃用的
  `exit_tokens` / `loan_decimals` / `repay_for_seize` / `collateral_value` / `loan_usd`），再寫它的
  應用迴圈（目前 `main.rs` 綁定 Morpho）。
- **其他聚合器**：實作 `Dex`（在 gas 預算內 `quote_within`、`amount_out`、`swap_gas`、`build`）。
- **沒有清算 callback 的協議**：實作 `DirectLiquidation`（`liquidation_call`），並搭配 0 手續費的
  `FlashSource` 使用 `FlashLiquidatorVenue`（Morpho 用 `exec.venue = "flash"`）。新的閃電資金來源
  = 一個 `FlashSource` 實作，加上 `FlashLiquidator.sol` 裡的一個分支。
- **其他清算合約**：實作 `LiquidationVenue`（`target`、含最低利潤位置的 `encode`、以傳入 fee 執行的
  `simulate`、`probe_gas`；沒有 packed 入口時加 `packed_routes`，swap 在別的合約執行時加
  `swap_recipient`）。

`testkit.rs` 用玩具實作示範每個介面，測試會用它們驅動引擎和 planner。完整教學在
[docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)。

## 安全

- 私鑰只放在加密 keystore（`cast wallet import`），絕不寫進程式碼、設定檔、`.env` 或 log。熱錢包
  只放 gas。
- RPC URL（含 API key）放在 `.env`；log 只顯示端點名稱。
- 實跑一定由操作者自己啟動。上線前請設好 priority fee 上限、每日 gas 上限、市場白名單（無許可
  借貸協議上有假幣或假預言機的市場）和緊急停止開關。
- 多個 bot 可以用同一個熱錢包簽名；它們透過每個錢包一個鎖檔協調 nonce（預設
  `~/.local/state/onchain-bots/nonce-<wallet>`；`exec.nonce_file` 請留空）。

發現漏洞？請見 [SECURITY.md](SECURITY.md)。

## 貢獻與授權

請見 [CONTRIBUTING.zh-TW.md](CONTRIBUTING.zh-TW.md)。採用
[GNU Affero General Public License v3.0 或更新版本](LICENSE)授權。

本軟體不提供任何擔保。在主網上使用可能造成資金損失。
