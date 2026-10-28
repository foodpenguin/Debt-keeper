# 架構

[English](architecture.md)

這個框架用來開發借貸協議的清算 bot。與鏈和協議無關的核心（倉位追蹤、規劃、模擬、送單）是針對三個 trait 寫的；協議、清算合約和 swap 聚合器各自實作對應的 trait 接進來。**Base 上的 Morpho Blue** 是第一個完整的實作，也是開發新實作時的參考。

## 範圍：目前哪些是通用的

通用（針對 trait 撰寫，不指名任何協議）：HF 引擎（`engine.rs`）、planner（`planner.rs`）、venue / DEX / 資金來源三個接口（`venue.rs`、`dex.rs`、`funding.rs`）和它們的 venue 實作（`sim.rs`、`flash_venue.rs`、`atlas_venue.rs`）、executor、RPC 連線池、heads 和 flashblocks 串流。仍然綁定 Morpho 的應用層程式：`main.rs` 的 watch 迴圈（區塊路徑、同塊路徑、送單路徑、pre-flight）、預言機價格模型（`oracle.rs`，MorphoChainlinkOracleV2）、Atlas 估值、儀表板和輸單分析。新的協議可以直接沿用通用核心，但目前需要自己的應用迴圈（從 `main.rs` 改起）。

## 一筆清算的完整流程

```
新區塊（WS newHeads / HTTP 輪詢 / Flashblocks 訊號）
  -> Engine：讀取被事件動到的倉位，加上每個市場一份快照，重算 health factor
  -> HF < 1 的候選倉位，由協議決定清算量（最大可沒收抵押品）
  -> Planner：出場路由（自建 V3 路由和/或聚合器），在 gas 預算內決定數量
  -> LiquidationVenue：編碼出要送的交易，在下一個區塊上模擬（eth_simulateV1），以不加價送出時的 fee 計價
  -> 風控閘門：最低淨利（USD）、priority fee、每日 gas 預算、緊急停止
  -> 出價高於基本 priority 時，先在那個 priority 下重新模擬，通過才保留
  -> 影子模式：只寫 journal｜實跑模式：簽名、廣播（可同時有多筆交易在飛）、追蹤 receipt
```

## Trait

| Trait | 檔案 | 職責 | 第一個實作 |
|---|---|---|---|
| `LendingProtocol` | `bot/src/protocol.rs` | 載入市場和倉位、把事件轉成「被動到的倉位」、讀取每個區塊的快照、評估健康度和清算量 | `morpho::Morpho` |
| `LiquidationVenue<L>` | `bot/src/venue.rs` | 把一筆清算（含出場路由）編碼成可送出的 payload、模擬、探測 gas | `sim::Simulator`（MorphoLiquidator）、`flash_venue::FlashLiquidatorVenue`（FlashLiquidator）、`atlas_venue::AtlasBid`（AtlasLiquidationSolver，只模擬） |
| `Dex` | `bot/src/dex.rs` | 外部聚合器：在 gas 預算內報價、產生 swap 呼叫 | `router::Kyber` |
| `DirectLiquidation` | `bot/src/protocol.rs` | 給「先持有還款資金」的呼叫者用的協議清算呼叫（沒有 callback） | `morpho::Morpho` |
| `FlashLoan` / `FlashAccounting` | `bot/src/funding.rs` | 以閃電方式籌措清算還款的資金來源 | `MorphoBlue` / `UniswapV4` |
| `FlashSource` | `bot/src/funding.rs` | FlashLiquidator 需要的資金來源資訊：`name`、`holder`、`fee`、`mode`（合約的 `funding` 選擇碼） | `MorphoBlue`（1）、`UniswapV4`（2） |

### `LendingProtocol`

關聯型別：`Market`、`Position`、`Snapshot`、`Health`、`Quote`、`Liquidation`。方法：

- `load_markets`：載入並驗證設定的市場（Morpho 會檢查 `keccak256(params) == id`）。
- `borrower_job`：自給自足的 future，從協議的 indexer 抓借款人清單；會被 spawn 出去，區塊迴圈不必等它。
- `load_positions` + `has_debt`：批次讀取倉位；沒有負債的倉位會被移除。
- `touched`：一段區塊範圍內會改變倉位的事件，以及發生的清算。
- `snapshots(prev)`：每個區塊、每個市場一份快照；`prev` 讓實作可以少一次往返（Morpho 用上一塊的狀態計算 IRM 利率）。
- `evaluate`：純函式，算出 HF、負債、最大可沒收抵押品、清算報價、獎勵。
- 規劃用的 hook：`exit_tokens`、`loan_decimals`、`repay_for_seize`、`collateral_value`，以及 `loan_usd(market, snap, eth_usd)`：每單位借款代幣的美元價格。利潤、gas 取捨和出價都以 USD 比較；回傳 `None` 表示沒有價格模型，市場會被拒絕，而不是被錯誤計價。Morpho 用 `morpho::loan_usd_model`：USD 穩定幣（`USDC`、`USDbC`、`USDT`、`DAI`、`USDS`）以 1 計、`WETH` 以 ETH 價格計，其他一律 `None`。程式啟動時也會拒絕這類市場。
- `eth_usd`（可選）：從追蹤中的市場推出 ETH/USD，用來計算 gas 成本。

每個方法都維持熱路徑上「批次、增量」的形狀：所有被動到的倉位一次讀完，所有快照一次讀完。

### `Engine<P, L>`（`bot/src/engine.rs`）

持有迴圈狀態，從不指名任何協議：每個市場追蹤中的倉位、市場索引、增量更新（只重讀事件動到的部分）、粉塵過濾、HF 排序，以及 indexer 回報的 HF（用來驗證）。`engine::view` 產生 `PositionView<L>`（市場、使用者、HF、負債、最大可沒收量、報價）。

### `Planner<L, V, A>`（`bot/src/planner.rs`）

把一個可清算的倉位（或同一市場的一整包倉位）變成經過模擬的計畫。

- **出場路由**：`direct` 是自建 V3 路由；`aggregator` 是 `Dex`（別名 `kyber`）；`best` 兩者並行，模擬淨利高的勝出，另一個保留作比較。
- **決定數量**：聚合器路徑對最大可沒收量的幾個比例平行報價，取毛利最高的；路線太耗 gas 時只用更小的比例重試。direct 路徑在同樣的 gas 預算內（`max_gas` 減掉固定的清算開銷）對報價 grid 做分配。
- **以 fee 計價的模擬**：每個計畫的模擬（以及 direct 路由的每次 grid 報價）都帶上不加價送出時的 fee：`Planner::sim_fee` = max fee 為 2 × base fee + `priority_fee_wei`、priority 為 `priority_fee_wei`。會讀 `tx.gasprice` 的 venue（priority fee 稅、動態手續費）回應的就是實際交易會拿到的結果。
- **以 USD 記帳**：`Plan::sim_profit` 是借款代幣單位；`profit_usd` = 它 × `loan_usd`；`gas_cost_usd` = 以規劃 priority 計的 L2 gas + L1 data fee（`l1_fee_usd` 單獨列出）；`net_usd` = `profit_usd - gas_cost_usd`。沒有借款代幣價格的市場會成為計畫錯誤。
- 對模擬結果做**合理性檢查**（利潤和報價的差距、節點實際模擬的區塊）。
- `quote_bundle` 把一個市場所有可清算倉位當成一次出場來估值（Atlas 估值用；一筆 solver 交易會清算全部）；`gross_usd` 以 USD 計，`gas_units` 是整個 solver op 的估計 gas（清算 + swap + 固定開銷）。

### `LiquidationVenue<L>`（`bot/src/venue.rs`）

`target()`（交易送往的合約）、`swap_recipient()`（執行出場 swap 的合約：聚合器路線會以它為付款方和收款方；預設就是 `target()`，但把清算包在另一個合約裡的 venue 會不同）、`encode(market, user, seize, repay, route, min_profit, deadline)` -> `Payload { to, calldata, minProfit 的位置 }`、`simulate(p, calldata, block, overrides, at, fee, probe)`（`fee: Option<SimFee>` 是呼叫帶的 `maxFeePerGas` / `maxPriorityFeePerGas`）、`probe_gas`，以及 `packed_routes()`（預設 `true`）。`repay` 是這筆清算要還的債（`LendingProtocol::repay_for_seize`）：自己籌措還款資金的 venue 會借這個數量，callback 型的 venue 則忽略它。minProfit 的位置讓 executor 在送出時把最終的 minProfit 寫進去。`ExitRoute` 是 `Packed(bytes)`（自建路由的壓縮路線）或 `Call { target, calldata }`（聚合器的 swap）兩種之一。合約沒有 packed 入口的 venue 會回傳 `packed_routes() = false`：planner 就不會給它 packed 路線（`direct` 模式回報 "this venue has no packed-route entry"；`best` 改用聚合器）。

`sim::Simulator` 是 MorphoLiquidator 的實作：packed 路線走精簡的 fallback 入口，外部呼叫則變成 ABI `liquidate` 唯一的 swap 步驟。沒有已部署的地址時，會用 state override 把清算合約的 runtime code 注入到佔位地址，所以部署前就能規劃。

所有 venue 共用的部分放在 `sim.rs`：`LiquidatorCode::build(path, ctor, from)` 在一次 `eth_call` 裡執行合約的 constructor（地址參數依序傳入），取得填好 immutable 的 runtime code；`LiquidatorCode::account(executor, allowed)` 把它轉成 state override（owner = executor、未暫停、設定好 `allowedTarget`；`account_with` 可設定多份白名單，`has_slot` 檢查某個變數存在）；`simulate_call` / `probe_gas_call` 用 `eth_simulateV1` 執行任何 calldata（不解讀利潤：每個 venue 自己解碼回傳值）。帶 fee 的 `simulate_call` 還會給發送者一個餘額覆寫：Base 的節點即使關閉 validation，也會檢查發送者付得起 gas × `maxFeePerGas`。關閉 validation 時模擬區塊的 base fee 是 0，所以 `tx.gasprice` 等於 priority fee，正好就是 priority fee 稅的課稅基礎。

### 籌措還款資金（`bot/src/funding.rs`）

清算的還款有兩種付法：

- **協議自己的 callback。**Morpho Blue 先送出抵押品，再呼叫 `onMorphoLiquidate`；抵押品在 callback 裡賣掉，之後 Morpho 收回還款。完全不用借錢。這是 `sim::Simulator` / MorphoLiquidator。
- **閃電資金來源**，給清算時先拉走還款的協議（Aave 式的 `liquidationCall`）。先借到還款，再用賣抵押品的錢還：`flash_venue::FlashLiquidatorVenue` / FlashLiquidator。兩種模型，刻意分成兩個 trait：
  - `FlashLoan`（`name`、`lender`、`fee`）：借固定數量，出借方回呼，callback 結束前還本金加手續費。`MorphoBlue`（0 手續費）。
  - `FlashAccounting`（`name`、`manager`、`fee`，預設 0）：先拿，在 `unlock` 結束前結清淨額。`UniswapV4`（PoolManager；清算合約持有鎖時，swap 步驟不能經過 V4）。

  `FlashSource` 是 venue 使用的 trait（`name`、`holder`、`fee`、`mode()`：1 = Morpho 閃電貸，2 = V4 閃電記帳），由 `MorphoBlue` 和 `UniswapV4` 實作；`FlashLiquidatorVenue` 持有一個 `Box<dyn FlashSource>`。新增來源要兩邊都做：一個 `FlashSource` 實作，以及 `FlashLiquidator.sol` 裡對應的分支（它的 `funding` 值、啟動閃電的呼叫和它的 callback，防護方式比照現有兩種）。合約只會原數歸還借款，所以有手續費的來源會被拒絕，直到合約能歸還本金加手續費。

`DirectLiquidation`（在 `protocol.rs`）是協議要為閃電路徑提供的東西：`liquidation_call(market, user, seize, repay) -> LiquidationTx { target, calldata, debt_token, collateral_token }`。Morpho 用不帶 callback 資料的 `liquidate` 實作它：送出抵押品後直接從呼叫者拉走還款，所以 Morpho 也能當成沒有 callback 的協議（閃電路徑就是這樣在真實合約上測試的）。

`FlashLiquidatorVenue<L: DirectLiquidation>` 產生 `FlashLiquidator.liquidate(funding, debtToken, debtAmount, collateralToken, liquidation, swaps, minProfit, deadlineBlock)`：借款數量是 `borrow_for(repay)` = repay + `headroom_bps` + 1 wei（避免交易上鏈前累積的利息讓協議拉走比借來更多的錢；沒用到的餘裕會隨借款還給資金方），聚合器出場變成唯一的 swap 步驟，`amountIn = type(uint256).max`（賣掉實際收到的全部抵押品，合約會換成自己的餘額），minProfit 放在 `FLASH_MIN_PROFIT`。它沒有 packed 入口（`packed_routes() = false`）。沒有設定 `exec.flash_liquidator` 時，合約會注入到佔位地址，和 MorphoLiquidator 一樣。

### `Dex`（`bot/src/dex.rs`）

`quote_within(token_in, token_out, amount_in, gas_budget)`、`amount_out`、`swap_gas`、`build(quote, sender, recipient, slippage_bps) -> SwapCall { target, calldata }`。planner 從不直接相信回傳的 target：它必須在 `exec.allowed_targets` 裡。`router::Kyber` 使用 gas 階梯（用兩種 gas 價格各報一次價，選出符合預算的最佳路線），並把 Kyber 的 gas 估計乘上安全係數。

## 出場路由：自建 V3 路由（`bot/src/v3router.rs`）

- 透過直達 leg 和 connector leg（抵押品 -> WETH -> 借款代幣），在 `[[router.factories]]` 列出的 Uniswap V3 類池子上把抵押品換成借款代幣（依 fee tier 或 tick spacing 區分的 factory）。factory 地址放在設定檔，不寫死在程式碼裡。
- **報價**：`contracts/src/sim/GridQuoter.sol` 不會部署，而是用 state override 注入，用一次 `eth_call`（Uniswap quoter 的 revert 技巧）報出每條 leg 的一整排數量；每一跳都有 gas 上限，避免異常的池子把整個呼叫的 gas 用光。報價呼叫帶規劃用的 fee（`Planner::sim_fee`），由一個有餘額覆寫的佔位呼叫者（`QUOTE_CALLER`）送出，所以會讀 `tx.gasprice` 的池子報出的是交易實際會拿到的量。
- **分配**：在合成的凹曲線上依邊際產出做 greedy（共用的 connector sink 不會重複計算），再和「只用單一 leg」比較，因為每多一條 leg 都有 gas 成本；`allocate_within` 會遵守 gas 預算。leg 的 gas 用 `planning_priority_fee_wei` 計價，並用 `loan_usd` 換算成借款代幣單位。
- **執行**：packed 路線直接呼叫池子；池子在自己的 swap callback 裡收款（只有正在交易的那個池子、只收一次、最多收該跳的輸入量）。以比例分給多個 sink 的共用 connector 代幣（bit 2）可能留下捨入餘額：清算結束後會清到 `PROFIT_RECEIVER`，平常不留存任何資金。

## 鏈上：`contracts/src/MorphoLiquidator.sol`

- `liquidate(params, borrower, seizedAssets, repaidShares, swaps, minProfit, deadlineBlock)`（ABI 入口，聚合器 swap）、`liquidateRoute` 和 packed `fallback`（direct 路線，精簡 calldata）。只有 `executor` 能呼叫。
- Morpho 先轉出抵押品，再呼叫 `onMorphoLiquidate(repaidAssets, data)`，swap 在這裡執行；之後 Morpho 收回還款（由 `approveMorpho` 設定的常設授權）。
- callback 結束後，合約檢查借款代幣的增加量 ≥ `minProfit`，把利潤送到 immutable 的 `PROFIT_RECEIVER`，平常不留存任何資金。
- 管理功能：`setExecutor`、`setAllowedTarget`、`approveMorpho`、`pause`（owner 或 executor）/ `unpause`（owner）、兩步驟的 `transferOwnership` / `acceptOwnership`、`rescue`。

### 安全模型

| 威脅 | 防護 | 測試 |
|---|---|---|
| 別人呼叫入口 | 入口只限 executor；管理功能只限 owner；owner 轉移要兩步驟 | `test_onlyOwnerAdmin`、`test_twoStepOwnership`、`test_revert_notExecutor` |
| 偽造 callback | 必須 `msg.sender == MORPHO`，而且正處於自己的清算中（transient 旗標） | `test_callbackOutsideFlightBlocked`、`test_forgedCallbackDuringFlightBlocked`、`test_revert_callbackFromOutsider` |
| swap 目標重入 | 目標不是 executor，無法重入 | `test_reentrancyFromRouterBlocked` |
| 被汙染的路線對代幣做 approve/transfer | swap 目標必須在 owner 的白名單裡；代幣地址永遠不會被加入白名單 | `test_tokenApproveTrickBlocked`、`test_targetNotAllowlisted` |
| router 偷抵押品或拉走超過授權的量 | 只給剛好的授權，用完歸零；Morpho 沒收到還款就整筆 revert | `test_thiefRouterGainsNothing`、`test_greedyRouterCannotExceedAllowance` |
| 轉帳收費或 rebase 的抵押品 | callback 檢查實際收到的抵押品 | `test_feeOnTransferCollateralRejected` |
| 利潤不足 | `minProfit` 檢查借款代幣的餘額差（含 fuzz） | `test_minProfitEnforced`、`testFuzz_profitAtLeastMin` |
| direct 路線裡的假池子 | 只呼叫池子的 `swap`/`token0`/`token1`，只在它自己的 callback 裡付款、最多付該跳輸入；平常不留存資金 | `test_route_poolAskingMoreThanInputGetsNothing`、`test_route_doubleCallbackRejected`、`test_swapCallbackFromStrangerRejected` |
| 路線沒用完沒收的抵押品 | 明確金額的來源 leg 加總必須剛好等於 `seizedAssets`（`RouteSpendMismatch`） | `test_route_mustSpendExactlySeized` |
| connector 留在合約裡 | 共用 connector 的餘額會清到 `PROFIT_RECEIVER` | `test_route_pooledConnectorRemainderIsSwept` |
| 池子回報正的產出 | 每一跳的產出必須是負的 delta（`BadRoute`） | `test_route_positiveOutputRejected` |
| 格式錯誤的 packed calldata | 嚴格解碼，不和 ABI 入口的 selector 衝突 | `test_packed_malformedRejected`、`test_packed_noSelectorCollision`、`test_packedEntry_equalsAbiEntry` |
| 緊急狀況 | executor（熱錢包）或 owner 可以 `pause`（給操作者手動停用；bot 本身從不呼叫，它的緊急停止是一個檔案），只有 owner 能恢復 | `test_pauseKillSwitch` |

鏈下的防護：聚合器的 target 必須在 `allowed_targets` 裡；啟動時從 params 重新推出市場 id；不合理的模擬（利潤和報價差太多）會被拒絕；RPC 錯誤訊息會改寫，含 API key 的 URL 不會進 log（`errors_never_contain_urls`）；風控閘門（`bot/src/risk.rs`）負責緊急停止檔案、最低淨利、依淨利比例出價且有硬上限的 priority fee，以及失敗交易的每日 gas 預算。它估計的成本是以出價計的 L2 gas 加上計畫的 L1 data fee，再和 `profit_usd` 比較。

### Gas

gas 基準測試在 `contracts/test/MorphoLiquidator.gas.fork.t.sol`（`forge test --isolate --match-contract GasBench -vv`）。合約在保留所有檢查的前提下做了 gas 優化：packed 入口（calldata 小很多）、executor 和 paused 放同一個 slot、immutable 的利潤接收者、沒有多餘的 event、不預先讀餘額（平常不留存資金）、對 Morpho 的常設授權、`via_ir` 加 memory-safe 的 assembly。在 base fee 很低的 L2 上，gas 主要影響 priority fee 競價：排序依「每單位 gas 的 priority fee」，所以同樣的預算下，越省 gas 的交易每單位能出更高的價。

### 部署清單（由 operator 執行）

1. `scripts/deploy_liquidator.sh <HOT> <COLD>`：部署時 `owner = HOT`（暫時）、`executor = HOT`、`profitReceiver = COLD`（immutable），對借款代幣呼叫 `approveMorpho`，有用聚合器的話把它的 router 加入白名單，最後發起把 owner 轉給 COLD。
2. COLD 呼叫 `acceptOwnership()`（`scripts/accept_ownership.sh`）。
3. 設定 `exec.liquidator`，並讓 `allowed_targets` 和鏈上的白名單保持一致。

## 鏈上：`contracts/src/FlashLiquidator.sol`

給沒有清算 callback 的協議用。`liquidate(funding, debtToken, debtAmount, collateralToken, liquidation, swaps, minProfit, deadlineBlock)`（只限 executor）先向 Morpho（`FUND_MORPHO`，`flashLoan` -> `onMorphoFlashLoan`）或 Uniswap V4 PoolManager（`FUND_V4`，`unlock` -> `unlockCallback` -> `take` ... `sync` / 轉帳 / `settle`）閃電借入 `debtAmount` 的債務代幣，接著不論資金從哪來都做同樣的事：只授權剛好 `debtAmount` 給清算目標、呼叫協議的清算函式、把授權歸零、確認抵押品已到帳、透過 swap 步驟賣掉抵押品，最後原數還給資金方。剩下的債務代幣就是利潤：必須達到 `minProfit`，連同剩餘的抵押品一起送到 immutable 的 `PROFIT_RECEIVER`。constructor 參數是 `(morpho, poolManager, owner, executor, profitReceiver)`；`poolManager = address(0)` 會停用 V4 資金。

合約只會原數歸還借款，所以只支援 0 手續費的來源：資金來源的 `fee()` 不是 0 時，bot 會拒絕建立這個 venue。

| 威脅 | 防護 | 測試（`FlashLiquidator.security.t.sol`） |
|---|---|---|
| 別人發起清算 | 入口只限 executor；期限；`pause` | `test_onlyExecutorAndNotExpiredOrPaused` |
| 偽造或亂入的資金 callback | sender 必須是資金方，而且必須正在進行一筆由「這個」來源出資的清算（transient 資金標記） | `test_callbacksRejectStrangersAndCallsOutsideAnOwnLiquidation`、`test_crossSourceAndSecondCallbacksAreRejected` |
| 同一筆清算裡的第二次 callback | 進入時就清除標記（只能一次） | `test_crossSourceAndSecondCallbacksAreRejected` |
| 資金方從不回呼 | 資金呼叫返回後 `NotFunded` | `test_funderThatNeverCallsBackReverts` |
| 被汙染的清算呼叫或 swap | 兩份 owner 白名單：清算呼叫只能打 `allowedProtocol`，swap 只能打 `allowedTarget`（router 永遠拿不到給協議的授權）；只給剛好的授權並歸零 | `test_liquidationAndSwapTargetsMustBeAllowlisted`、`test_anAllowlistedRouterIsNotAnAllowedProtocol`、`test_allowancesAreResetAfterTheCall` |
| 沒有沒收到任何東西的清算 | `NothingSeized` | `test_aLiquidationThatSeizesNothingReverts` |
| 利潤不足 | 還完資金方之後剩下的債務代幣要 ≥ `minProfit` | `test_minProfitEnforced` |
| 未知的資金方式／未設定 V4 | `FundingDisabled` | `test_unknownFundingAndDisabledV4Revert` |

`test_bothFundingSourcesLiquidateRepayAndPayTheReceiver` 檢查兩種來源的正常流程。`FlashLiquidator.fork.t.sol` 在真實的 Base 合約上跑，以 Morpho 當作沒有 callback 的協議：`test_morphoFlashLoanFundsACallbackFreeLiquidation`、`test_v4FlashAccountingFundsACallbackFreeLiquidation`（利潤和 Morpho 閃電貸相同）、`test_unprofitableExitReverts`、`test_protocolTargetMustBeAllowlisted`。Rust 的編碼由 `e2e_flash_venue_simulates_a_flash_funded_liquidation` 在 anvil fork 上和合約對照。

部署方式和 MorphoLiquidator 相同（owner、executor、利潤接收者），另外要把協議的清算目標加進 `setAllowedProtocol`、聚合器 router 加進 `setAllowedTarget`，再設定 `exec.flash_liquidator`。

## 鏈上：`contracts/src/AtlasLiquidationSolver.sol`（只模擬）

給 Fastlane Atlas 舉辦的 Chainlink SVR 拍賣用的 solver 合約，依 Atlas 的 solver 介面（`atlasSolverCall`）自行撰寫，不是從 Atlas 的程式碼改來的。在得標者的時段裡，Atlas 會呼叫 `atlasSolverCall(solverOpFrom, executionEnvironment, bidToken, bidAmount, solverOpData, extraReturnData)`；`solverOpData` = `(SolverCall[] calls, SwapStep bidSwap, address profitToken, uint256 minProfit)`。solver 依序執行 calls（透過一個以 solver 為 executor 和利潤接收者的 MorphoLiquidator 清算）、執行出價用的 swap（借款代幣 -> WETH；`target = address(0)` 表示不需要）、視需要把 WETH 換回 ETH（Atlas 借出的 ETH 會先保留，出價不會動用）並把 ETH 出價付給 execution environment、和 Atlas 結算（透過 `reconcile` 還清借款，並核准最後讀到的 gas 負債）、檢查**扣掉出價後**剩下的利潤代幣 ≥ `minProfit`、把它和剩餘的 WETH 或 ETH 送到 `PROFIT_RECEIVER`，並回傳淨利潤。constructor：`(atlas, weth, owner, executor, profitReceiver)`；`executor` 是 solver operation 的簽名者。

| 威脅 | 防護 | 測試（`AtlasLiquidationSolver.security.t.sol`） |
|---|---|---|
| Atlas 以外的人呼叫，或別人簽的 op | `msg.sender == ATLAS`（`InvalidEntry`）且 `solverOpFrom == executor`（`InvalidCaller`） | `test_onlyAtlasForTheSignersOperation` |
| 非 ETH 出價；緊急狀況 | 只接受 ETH 出價（`UnsupportedBidToken`）；executor（簽名者）或 owner 可以 `pause`，只有 owner 能 `unpause` | `test_ethBidsOnlyAndPauseStopsIt` |
| Atlas 借出的 ETH 被拿去付出價 | 借款負債（最多到 `msg.value`）會先保留：WETH 換回足夠付出價加還款的 ETH，借款再透過 `reconcile` 還回 | `test_borrowedEthIsRepaidEvenWhenTheBidNeedsEth` |
| 被汙染的呼叫或出價 swap | 每個呼叫目標和出價 swap 的 router 都必須在 owner 白名單裡；授權用完歸零 | `test_callAndBidSwapTargetsMustBeAllowlisted` |
| 出價超過這個 op 賺到的 | 出價必須買得到、付得出，而且扣掉後仍要 ≥ `minProfit` | `test_aBidTheSwapCannotFundReverts`、`test_minProfitIsCheckedAfterTheBid` |
| 別人做管理操作 | 管理功能只限 owner | `test_adminIsOwnerOnly` |

`test_runsTheLiquidationPaysTheEthBidSettlesAndKeepsTheRest` 涵蓋正常流程；`AtlasLiquidationSolver.fork.t.sol` 在 Base fork 上、在 solver op 裡跑一筆真實的清算（`test_solverLiquidatesPaysTheEthBidAndKeepsTheRest`、`test_aBidAboveTheProfitReverts`），用 `contracts/src/sim/AtlasStub.sol` 代替 Atlas escrow；並確認鏈上部署的 Atlas escrow 具備 solver 會呼叫的結算函式，語意也和 solver 的假設一致（`test_theDeployedAtlasEscrowHasTheSettlementCallsTheSolverMakes`：metacall 之外 `shortfall` 為 0、`reconcile` 以 `WrongPhase` revert、可讀到 `getAtlasSurchargeRate`）。完整的 metacall 無法重播（需要使用者和 DApp 的簽名）。

## 鏈下模組（`bot/src/`）

| 模組 | 職責 |
|---|---|
| `config` | TOML 設定和 RPC 解析（URL 只從 `.env` 來） |
| `protocol`、`engine` | 協議的接口和通用的 HF 引擎 |
| `morpho/` | Morpho Blue：鏈上讀取、事件、數學（含 virtual shares 的 shares/assets 換算、LIF、HF、seize <-> repay）、indexer API |
| `planner`、`venue`、`dex` | 規劃和它的兩個接口 |
| `sim` | MorphoLiquidator 的編碼；共用的 `simulate_call` / `probe_gas_call`（`eth_simulateV1`：利潤、gas、L1 fee，用下一塊的高度和時間）和 `LiquidatorCode`（注入的合約） |
| `funding` | 閃電資金來源：`FlashLoan` / `FlashAccounting`、`MorphoBlue` / `UniswapV4`、`FlashSource` trait |
| `flash_venue` | `FlashLiquidatorVenue`：把 FlashLiquidator 當作 `DirectLiquidation` 協議的 venue |
| `atlas_venue` | `AtlasSolver` / `AtlasBid`：把 AtlasLiquidationSolver 當作 venue（只模擬） |
| `router`、`v3router` | Kyber 聚合器客戶端；自建 V3 路由 |
| `risk` | 送或不送的決定，以及 priority fee |
| `executor` | keystore 簽名、廣播、nonce、失敗處理（見下方） |
| `rpc` | 多端點連線池（見下方） |
| `heads` | 新區塊訊號：多個端點的 WS `newHeads`，HTTP 備援 |
| `flashblocks` | Base Flashblocks 串流：解碼、從原始交易解出預言機更新 |
| `oracle` | 本地的 MorphoChainlinkOracleV2 價格模型，啟動時和 `price()` 核對 |
| `atlas` | Chainlink SVR / Fastlane Atlas 拍賣估值（只觀察） |
| `loss` | 輸單分析：追蹤市場裡每一筆鏈上清算，和我們當時看到的比較 |
| `dash/`、`live` | SQLite 歷史資料 + 靜態儀表板；localhost 上的即時 HF 頁面 |

### 模擬

一次 `eth_simulateV1` 跑完整筆清算，回傳利潤和實際 gas，再加上 L1 fee（`getL1FeeUpperBound`）。計畫以規劃用的 fee 模擬（見 `Planner`）；風控閘門選出高於基本 priority 的出價後，`checked_priority`（在 `main.rs`）會在那個 priority 下重新模擬一次，淨利仍達到 `min_net_usd` 才保留出價，否則改用基本 priority；送單時的 minProfit 取兩次利潤中較低的一個。重新模擬時完全沒有利潤，這個計畫就不送。模擬是以**下一個**區塊執行（覆寫區塊高度和時間），因為候選倉位是把利息累積到下一塊來評估的。不使用 `eth_estimateGas`：有些節點實作在 estimateGas 裡會忽略 block override。拿不到真實 gas 的計畫一律視為錯誤，絕不用猜的。綁在 "pending" 狀態的計畫會檢查節點實際模擬的是哪個區塊，因為 pending 呼叫可能跑在舊的區塊上。

### Executor（`bot/src/executor.rs`）

- **簽名**：只從加密 keystore 解密；密碼從環境變數讀取（讀完立刻從 process 環境中刪除），或用不回顯的提示輸入。
- **可同時有多筆交易在飛**：最多 `exec.max_inflight` 筆（預設 3；設 1 就是一次一筆），nonce 連續，因為暴跌會讓很多倉位在幾個區塊內同時變得可清算；同一個倉位不會重複送（`can_send`、`is_inflight`）；某個市場已經有交易在飛時，也不會再送第二筆到同一個市場（`is_market_inflight`：計畫是在不含在飛交易的狀態上模擬的，同市場的那筆會先把同樣的抵押品賣進同樣的池子，第二筆計畫的價格就不成立了）。minProfit 改成「模擬利潤 × `min_profit_keep`」，gas limit 改成「模擬 gas × `gas_limit_buffer`」。錢包必須付得起最壞情況的 gas：`spendable` = 背景讀到的餘額（由 `spawn_nonce_watch` 和 nonce 一起讀）減去在飛交易最多還可能花掉的 gas，所以送單路徑不必為此多打一次 RPC。
- **結算**：`poll` 平行抓取所有在飛交易的 receipt。過了期限 `DROP_GRACE_BLOCKS`（2）個區塊還沒有 receipt 的交易視為被丟棄，nonce 在它之後、也還沒有 receipt 的交易一併視為丟棄（它們都不可能比它先上鏈）。被丟棄但仍留在 mempool 的交易（例如 base fee 上升後 max fee 不夠）會擋住後面所有 nonce，所以 `cancel_stuck` 會先把每筆仍在 pending 的交易，用同一個 nonce、fee 調高到超過替換門檻的 0 元自轉帳取代（反正它上鏈也只會 revert `Expired`）；接著從鏈上重新同步 nonce。
- **廣播**：同一筆 raw tx 送給所有端點，加上可選的 `extra_broadcast`；第一個接受就回傳，其他的在背景完成。專用的 HTTP client 會保持連線暖機（keep-alive ping），送單時不必再付 DNS/TLS 建立連線的時間。
- **nonce**：在本地追蹤；交易被丟棄、遇到 nonce 錯誤或 "replacement underpriced" 時，從鏈上重新同步並重送一次。用同一個錢包簽名的多個 bot 共用每個錢包一份的鎖檔（`exec.nonce_file`，預設 `~/.local/state/onchain-bots/nonce-<錢包>`），在簽名到廣播之間持有。鎖檔採 fail closed：鎖不到就不送單（否則同一錢包的另一個 bot 可能用同一個 nonce 簽名）。
- **失敗處理**：`failure_window_secs` 內累積 `max_failures_in_window` 筆失敗或被丟棄的交易，就暫停送單 `failure_cooldown_secs`，之後自動恢復。
- **期限與提前量**：`deadline_secs` 依實測的出塊時間換算成區塊數；利息會預先累積 `accrual_lookahead_secs`（不設定時 = 實測出塊時間無條件進位，介於 1 到 2 秒），所以出塊時間改變（2 秒 -> 200 ms）時不必改設定。
- **緊急停止**：相對路徑的 `exec.kill_switch_file` 以設定檔所在目錄為基準，不論從哪個目錄啟動 bot 都是同一個檔案。
- **實跑要同時滿足**：`exec.mode = "live"`、`watch --live`、所選 venue 的已部署合約（`exec.liquidator`；`exec.venue = "flash"` 時是 `exec.flash_liquidator`）、`exec.keystore`，以及 pre-flight 檢查通過：合約存在、它的 executor 是 keystore 的地址、沒被暫停、利潤接收者不是熱錢包、錢包餘額 ≥ `min_wallet_eth`，再依 venue 檢查：callback venue 要有對 Morpho 的常設授權（`approveMorpho`），flash venue 要把 Morpho 列在 `allowedProtocol` 裡，而且合約的 `MORPHO()`／`POOL_MANAGER()` 必須和設定的資金來源一致。

### RPC 連線池（`bot/src/rpc.rs`）

端點在 `[[rpc]]` 宣告（`http_env` 指向 `.env` 裡的變數名稱；公共端點可以直接用 `http`）。連線池依優先順序競速或備援，遵守每個端點的 `skip_methods` 和 log 範圍限制；遇到「方法不存在」、限流和容量錯誤會換端點（並進入冷卻）；節點落後時回的 "block not found / header not found" 視為*落後*：換下一個端點，但不讓落後的端點進入冷卻，除非它一再落後。容量錯誤以片語判斷（限流、方案、範圍和可用性的限制，例如 "rate limit"、"block range"、"free plan"），不用單一字詞；呼叫本身的確定性錯誤（revert、gas limit、餘額不足、nonce、fee cap……）絕不換端點，也不讓端點進入冷卻。log 只會出現端點名稱。請求帶有 `liquidation-framework/<版本>` 的 User-Agent。

### 區塊路徑與送單時機

每個新區塊，engine 更新被動到的倉位，最有價值的候選（`max_plans_per_block`）會同時開始規劃；每一筆在自己的計畫完成的那一刻就寫 journal、過風控、送出（某一筆的聚合器報價慢，不會拖累其他筆）。下面的同塊路徑也一樣。兩者仍在主迴圈裡執行：一批在規劃時，下一個區塊或 flashblock 要等（它們不是獨立的 task）。

### 同塊路徑（`bot/src/flashblocks.rs`、`bot/src/oracle.rs`）

Base 會串流約 200 ms 一個的子區塊（Flashblocks）。bot 直接從串流裡的原始交易解出 Chainlink 的 transmit（OCR report -> observations 的中位數），在本地算出新的 Morpho 預言機價格，只重算那個市場。每個候選平行規劃兩次：一次在快照區塊上、用 state override 鎖定新價格；一次在正在產生的 "pending" 區塊上，**不做任何價格覆寫**，並在同一個狀態裡讀預言機（`PriceProbe`）證明更新已經生效（transmit revert，或 SVR cutoff 還差一個區塊時，讀到的會是舊價格）。狀態落後（舊區塊或舊價格）時，pending 計畫會短暫重試。新鮮且已證明的 pending 計畫勝出；否則用快照計畫。SVR feed 另外處理：公開價格要等對應的 secondary round 上鏈，或過了該 feed 的 cutoff 才生效。`may_send(path, state)`：公開的 transmit（`Primary`）兩種計畫都可以送；SVR secondary 或過了 cutoff 的 SVR primary 只能用新鮮的 pending 計畫送。`[flashblocks] send = false` 讓這條路徑只寫 journal。注意：Base 計畫中的 "Denim" 升級會移除 Flashblocks、改成 200 ms 出塊；本地價格模型和單一市場重算仍然有用，期限和利息提前量也會跟著實測的出塊時間調整，所以資料層沒有和 Flashblocks API 綁死。

### Atlas / Chainlink SVR（`bot/src/atlas.rs`）

在啟用 SVR 的 feed 上，預言機更新後的清算權會透過 Fastlane Atlas 拍賣。這個模組監聽拍賣，用同一個 planner 對每一場估值（先對整個市場 `quote_bundle`，再逐倉位規劃），把我們可以出的價寫進 journal。它從不送出任何東西。`value-auction` 可以重播過去的拍賣。出價的基礎是 solver 在 Atlas 收完 gas 費之後能留下的部分：每個市場的 `atlas_net_usd` = 這包的 `gross_usd` 減去它的 `gas_units` × 拍賣的 `maxFeePerGas` × (1 + 附加費)，加總成 `bundle_net_usd`；寫進 journal 的出價是它 × (1 - `atlas.margin`)。附加費在啟動時從 escrow 讀取（`getAtlasSurchargeRate`，基數 1e4）；讀不到時用 `[atlas] gas_surcharge`（預設 0.1）。

**Solver venue（`bot/src/atlas_venue.rs`，只模擬）。**設定 `[atlas.solver] simulate = true` 時，每場被估值的拍賣裡，每個市場在 journal 裡會多一個 `solver` 欄位（`main.rs` 的 `solver_op`）：把最大倉位的直接路由計畫包進 `atlasSolverCall`，出價是（`profit_usd` 減去這個 op 以上述 Atlas 費率計的 gas）× (1 - `atlas.margin`)，並像 Atlas 那樣呼叫來模擬。Atlas 地址上注入 `AtlasStub.sol`（沒有 gas 負債），solver 和它的 MorphoLiquidator 沒部署時也會注入；結果包含這次呼叫的 L1 fee。journal 會記錄出價、op 是否成功，以及扣掉出價後的淨利。`solver_op` 只包直接路由的計畫（既有計畫的聚合器 calldata 是為直接清算合約的地址產生的）；`exec.venue = "flash"` 時整個跳過。`AtlasSolver::calldata` 產生呼叫（借款代幣不是 WETH 時，在 `bid_router` 上用 `exactOutputSingle` 買出價要用的 WETH；minProfit 在 `SOLVER_MIN_PROFIT`）；`AtlasBid { solver, bid_wei }` 實作 `LiquidationVenue<Morpho>`，其 `swap_recipient()` 是 solver 的 MorphoLiquidator，所以 planner 也能直接透過它規劃，包含聚合器路線（路線會送到那個清算合約）。`e2e_atlas_solver_op_pays_its_bid_out_of_the_liquidation` 在 anvil fork 上驗證它。**尚未實作：**組出 `SolverOperation` 並做 EIP-712 簽名，以及把它送到拍賣端點。

### 儀表板和即時頁面

`bot dashboard` 把 journal 和執行 log 匯入 SQLite（`dashboard.db_path`），產生一個靜態 HTML 頁面（不需要伺服器）：KPI、延遲、清算、拍賣估值、鏈上清算的出場路徑分析（從 tx 和 receipt 解碼一次後快取）、RPC 用量。`bot live` 在 `127.0.0.1:live_port` 提供唯讀頁面，顯示最接近被清算的倉位，資料來自 `watch` 每個區塊寫出的快照。

## 加入一個協議

1. 建立 `bot/src/<protocol>/`，實作 `LendingProtocol`（參考 `morpho/mod.rs`）。選好關聯型別；讀取要批次；`evaluate` 要是純函式。
2. 為你的市場型別實作 `MarketInfo`（`key()` 是穩定的 `B256`，`name()`）。
3. 從 `bot/src/testkit.rs` 開始：`Toy` 是一個完整的非 Morpho 協議（position = (負債, 抵押品)，snapshot = 價格），engine 和 planner 的測試都用它跑。照它的形狀為自己的實作寫單元測試。
4. 實作 `loan_usd`：你允許的每一種借款代幣都要有 USD 價格模型（否則市場會被拒絕）。
5. 接上應用層：目前 `main.rs`（watch 迴圈、預言機模型、Atlas 估值、儀表板）綁定 Morpho，使用 `morpho::MorphoEngine`；為你的協議加同樣的別名、一個像 `[morpho]` 的設定區段，以及你自己的迴圈（見上方「範圍」）。
6. 協議沒有清算 callback（清算時先拉走還款）的話，實作 `DirectLiquidation`（`liquidation_call`），並搭配 0 手續費的 `FlashSource`（`MorphoBlue` 或 `UniswapV4`；新的來源還需要在 FlashLiquidator.sol 加分支）使用 `flash_venue::FlashLiquidatorVenue`：planner、模擬和 FlashLiquidator.sol 都能直接沿用。`testkit::Toy` 實作了 `DirectLiquidation`；參考 `flash_venue` 和 `planner` 的測試。

## 加入一個 DEX（聚合器）

實作 `dex::Dex`（參考 `router.rs` 裡的 `impl Dex for Kyber`）：`name()` 會成為 `Plan::router`，`quote_within` 必須遵守 gas 預算，`build` 回傳 venue 要執行的呼叫。`testkit::MockDex`（每單位抵押品換 2 單位借款代幣，經由 `0x22..`）和 `planner.rs` 裡的 `generic_tests` 示範了完整的聚合器路徑，包含白名單檢查。記得把 router 加進 `exec.allowed_targets` 和合約的白名單。

## 加入一個 venue（清算合約）

為你的合約實作 `venue::LiquidationVenue<L>`：`encode` 必須回傳你實際會送出的 calldata，以及 `minProfit` 所在的 byte 位置；`simulate` 必須用傳入的 `fee` 在下一個區塊上跑這份 calldata（沿用 `sim::simulate_call`，部署前用 `LiquidatorCode` 注入合約）；合約沒有 packed 路線入口時，回傳 `packed_routes() = false`；swap 不是在 `target()` 執行時，覆寫 `swap_recipient()`。`testkit::MockVenue` 會標記 packed 和 call 兩種路線，從不模擬；`FlashLiquidatorVenue` 和 `AtlasBid` 是兩個完整的範例。部署之前，先照上面的表寫一套 Foundry 安全測試。

## 常用指令

```bash
cd contracts && forge test -vv                      # 合約測試（fork 測試用 BASE_RPC_URL）
cd contracts && forge test --no-match-contract Fork # 只跑安全測試，不需要 RPC
cd bot && cp config.example.toml config.local.toml
cargo run --release -- -c config.local.toml scan --top 5     # 一次性掃描 + HF 驗證
cargo run --release -- -c config.local.toml watch            # 影子模式，只寫 journal
cargo run --release -- -c config.local.toml report           # 從 journal 做輸單分析
cargo run --release -- -c config.local.toml dashboard        # 跑一輪儀表板
cargo run --release -- -c config.local.toml watch --from-block N   # 重播（需要 archive RPC）
cargo run --release -- -c config.local.toml plan --market <name> --user 0x... --price-drop-bps 300
cargo test                                          # 單元測試
cargo test e2e -- --ignored --nocapture             # 在本地 anvil fork 上測試實跑路徑
```
