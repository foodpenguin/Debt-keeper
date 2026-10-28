# 架構

[English](architecture.md)

這個框架用來開發借貸協議的清算 bot。與鏈和協議無關的核心（倉位追蹤、規劃、模擬、送單）是針對三個 trait 寫的；協議、清算合約和 swap 聚合器各自實作對應的 trait 接進來。**Base 上的 Morpho Blue** 是第一個完整的實作，也是開發新實作時的參考。

## 一筆清算的完整流程

```
新區塊（WS newHeads / HTTP 輪詢 / Flashblocks 訊號）
  -> Engine：讀取被事件動到的倉位，加上每個市場一份快照，重算 health factor
  -> HF < 1 的候選倉位，由協議決定清算量（最大可沒收抵押品）
  -> Planner：出場路由（自建 V3 路由和/或聚合器），在 gas 預算內決定數量
  -> LiquidationVenue：編碼出要送的交易，在下一個區塊上模擬（eth_simulateV1）
  -> 風控閘門：最低淨利、priority fee、每日 gas 預算、緊急停止
  -> 影子模式：只寫 journal｜實跑模式：簽名、廣播、追蹤 receipt
```

## Trait

| Trait | 檔案 | 職責 | 第一個實作 |
|---|---|---|---|
| `LendingProtocol` | `bot/src/protocol.rs` | 載入市場和倉位、把事件轉成「被動到的倉位」、讀取每個區塊的快照、評估健康度和清算量 | `morpho::Morpho` |
| `LiquidationVenue<L>` | `bot/src/venue.rs` | 把一筆清算（含出場路由）編碼成可送出的 payload、模擬、探測 gas | `sim::Simulator`（MorphoLiquidator） |
| `Dex` | `bot/src/dex.rs` | 外部聚合器：在 gas 預算內報價、產生 swap 呼叫 | `router::Kyber` |

### `LendingProtocol`

關聯型別：`Market`、`Position`、`Snapshot`、`Health`、`Quote`、`Liquidation`。方法：

- `load_markets`：載入並驗證設定的市場（Morpho 會檢查 `keccak256(params) == id`）。
- `borrower_job`：自給自足的 future，從協議的 indexer 抓借款人清單；會被 spawn 出去，區塊迴圈不必等它。
- `load_positions` + `has_debt`：批次讀取倉位；沒有負債的倉位會被移除。
- `touched`：一段區塊範圍內會改變倉位的事件，以及發生的清算。
- `snapshots(prev)`：每個區塊、每個市場一份快照；`prev` 讓實作可以少一次往返（Morpho 用上一塊的狀態計算 IRM 利率）。
- `evaluate`：純函式，算出 HF、負債、最大可沒收抵押品、清算報價、獎勵。
- 規劃用的 hook：`exit_tokens`、`loan_decimals`、`repay_for_seize`、`collateral_value`。
- `eth_usd`（可選）：從追蹤中的市場推出 ETH/USD，用來計算 gas 成本。

每個方法都維持熱路徑上「批次、增量」的形狀：所有被動到的倉位一次讀完，所有快照一次讀完。

### `Engine<P, L>`（`bot/src/engine.rs`）

持有迴圈狀態，從不指名任何協議：每個市場追蹤中的倉位、市場索引、增量更新（只重讀事件動到的部分）、粉塵過濾、HF 排序，以及 indexer 回報的 HF（用來驗證）。`engine::view` 產生 `PositionView<L>`（市場、使用者、HF、負債、最大可沒收量、報價）。

### `Planner<L, V, A>`（`bot/src/planner.rs`）

把一個可清算的倉位（或同一市場的一整包倉位）變成經過模擬的計畫。

- **出場路由**：`direct` 是自建 V3 路由；`aggregator` 是 `Dex`（別名 `kyber`）；`best` 兩者並行，模擬淨利高的勝出，另一個保留作比較。
- **決定數量**：聚合器路徑對最大可沒收量的幾個比例平行報價，取毛利最高的；路線太耗 gas 時只用更小的比例重試。direct 路徑在同樣的 gas 預算內（`max_gas` 減掉固定的清算開銷）對報價 grid 做分配。
- 對模擬結果做**合理性檢查**（利潤和報價的差距、節點實際模擬的區塊）。
- `quote_bundle` 把一個市場所有可清算倉位當成一次出場來估值（Atlas 估值用；一筆 solver 交易會清算全部）。

### `LiquidationVenue<L>`（`bot/src/venue.rs`）

`target()`（交易送往的合約，也是 swap 的付款方和收款方）、`encode(market, user, seize, route, min_profit, deadline)` -> `Payload { to, calldata, minProfit 的位置 }`、`simulate`、`probe_gas`。minProfit 的位置讓 executor 在送出時把最終的 minProfit 寫進去。`ExitRoute` 是 `Packed(bytes)`（自建路由的壓縮路線）或 `Call { target, calldata }`（聚合器的 swap）兩種之一。

`sim::Simulator` 是 MorphoLiquidator 的實作：packed 路線走精簡的 fallback 入口，外部呼叫則變成 ABI `liquidate` 唯一的 swap 步驟。沒有已部署的地址時，會用 state override 把清算合約的 runtime code 注入到佔位地址，所以部署前就能規劃。

### `Dex`（`bot/src/dex.rs`）

`quote_within(token_in, token_out, amount_in, gas_budget)`、`amount_out`、`swap_gas`、`build(quote, sender, recipient, slippage_bps) -> SwapCall { target, calldata }`。planner 從不直接相信回傳的 target：它必須在 `exec.allowed_targets` 裡。`router::Kyber` 使用 gas 階梯（用兩種 gas 價格各報一次價，選出符合預算的最佳路線），並把 Kyber 的 gas 估計乘上安全係數。

## 出場路由：自建 V3 路由（`bot/src/v3router.rs`）

- 透過直達 leg 和 connector leg（抵押品 -> WETH -> 借款代幣），在 `[[router.factories]]` 列出的 Uniswap V3 類池子上把抵押品換成借款代幣（依 fee tier 或 tick spacing 區分的 factory）。factory 地址放在設定檔，不寫死在程式碼裡。
- **報價**：`contracts/src/sim/GridQuoter.sol` 不會部署，而是用 state override 注入，用一次 `eth_call`（Uniswap quoter 的 revert 技巧）報出每條 leg 的一整排數量；每一跳都有 gas 上限，避免異常的池子把整個呼叫的 gas 用光。
- **分配**：在合成的凹曲線上依邊際產出做 greedy（共用的 connector sink 不會重複計算），再和「只用單一 leg」比較，因為每多一條 leg 都有 gas 成本；`allocate_within` 會遵守 gas 預算。leg 的 gas 用 `planning_priority_fee_wei` 計價。
- **執行**：packed 路線直接呼叫池子；池子在自己的 swap callback 裡收款（只有正在交易的那個池子、只收一次、最多收該跳的輸入量）。

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
| 池子回報正的產出 | 每一跳的產出必須是負的 delta（`BadRoute`） | `test_route_positiveOutputRejected` |
| 格式錯誤的 packed calldata | 嚴格解碼，不和 ABI 入口的 selector 衝突 | `test_packed_malformedRejected`、`test_packed_noSelectorCollision`、`test_packedEntry_equalsAbiEntry` |
| 緊急狀況 | executor 可以 `pause`，只有 owner 能恢復 | `test_pauseKillSwitch` |

鏈下的防護：聚合器的 target 必須在 `allowed_targets` 裡；啟動時從 params 重新推出市場 id；不合理的模擬（利潤和報價差太多）會被拒絕；RPC 錯誤訊息會改寫，含 API key 的 URL 不會進 log（`errors_never_contain_urls`）；風控閘門（`bot/src/risk.rs`）負責緊急停止檔案、最低淨利、依淨利比例出價且有硬上限的 priority fee，以及失敗交易的每日 gas 預算。

### Gas

gas 基準測試在 `contracts/test/MorphoLiquidator.gas.fork.t.sol`（`forge test --isolate --match-contract GasBench -vv`）。合約在保留所有檢查的前提下做了 gas 優化：packed 入口（calldata 小很多）、executor 和 paused 放同一個 slot、immutable 的利潤接收者、沒有多餘的 event、不預先讀餘額（平常不留存資金）、對 Morpho 的常設授權、`via_ir` 加 memory-safe 的 assembly。在 base fee 很低的 L2 上，gas 主要影響 priority fee 競價：排序依「每單位 gas 的 priority fee」，所以同樣的預算下，越省 gas 的交易每單位能出更高的價。

### 部署清單（由 operator 執行）

1. `scripts/deploy_liquidator.sh <HOT> <COLD>`：部署時 `owner = HOT`（暫時）、`executor = HOT`、`profitReceiver = COLD`（immutable），對借款代幣呼叫 `approveMorpho`，有用聚合器的話把它的 router 加入白名單，最後發起把 owner 轉給 COLD。
2. COLD 呼叫 `acceptOwnership()`（`scripts/accept_ownership.sh`）。
3. 設定 `exec.liquidator`，並讓 `allowed_targets` 和鏈上的白名單保持一致。

## 鏈下模組（`bot/src/`）

| 模組 | 職責 |
|---|---|
| `config` | TOML 設定和 RPC 解析（URL 只從 `.env` 來） |
| `protocol`、`engine` | 協議的接口和通用的 HF 引擎 |
| `morpho/` | Morpho Blue：鏈上讀取、事件、數學（含 virtual shares 的 shares/assets 換算、LIF、HF、seize <-> repay）、indexer API |
| `planner`、`venue`、`dex` | 規劃和它的兩個接口 |
| `sim` | MorphoLiquidator 的編碼，以及用下一塊的高度和時間跑 `eth_simulateV1` 模擬（利潤、gas、L1 fee） |
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

一次 `eth_simulateV1` 跑完整筆清算，回傳利潤和實際 gas，再加上 L1 fee（`getL1FeeUpperBound`）。模擬是以**下一個**區塊執行（覆寫區塊高度和時間），因為候選倉位是把利息累積到下一塊來評估的。不使用 `eth_estimateGas`：有些節點實作在 estimateGas 裡會忽略 block override。拿不到真實 gas 的計畫一律視為錯誤，絕不用猜的。綁在 "pending" 狀態的計畫會檢查節點實際模擬的是哪個區塊，因為 pending 呼叫可能跑在舊的區塊上。

### Executor（`bot/src/executor.rs`）

- **簽名**：只從加密 keystore 解密；密碼從環境變數讀取（讀完立刻從 process 環境中刪除），或用不回顯的提示輸入。
- **一次只有一筆交易在飛**。minProfit 改成「模擬利潤 × `min_profit_keep`」，gas limit 改成「模擬 gas × `gas_limit_buffer`」；錢包必須付得起最壞情況的 gas。
- **廣播**：同一筆 raw tx 送給所有端點，加上可選的 `extra_broadcast`；第一個接受就回傳，其他的在背景完成。專用的 HTTP client 會保持連線暖機（keep-alive ping），送單時不必再付 DNS/TLS 建立連線的時間。
- **nonce**：在本地追蹤；交易被丟棄、遇到 nonce 錯誤或 "replacement underpriced" 時，從鏈上重新同步並重送一次。用同一個錢包簽名的多個 bot 共用每個錢包一份的鎖檔（`exec.nonce_file`，預設 `~/.local/state/onchain-bots/nonce-<錢包>`），在簽名到廣播之間持有。
- **失敗處理**：`failure_window_secs` 內累積 `max_failures_in_window` 筆失敗或被丟棄的交易，就暫停送單 `failure_cooldown_secs`，之後自動恢復。
- **期限**：`deadline_secs` 依實測的出塊時間換算成區塊數。
- **實跑要同時滿足**：`exec.mode = "live"`、`watch --live`、`exec.liquidator`、`exec.keystore`，以及 pre-flight 檢查通過（合約存在、它的 executor 是 keystore 的地址、沒被暫停、利潤接收者不是熱錢包、已設定 Morpho 授權、錢包餘額 ≥ `min_wallet_eth`）。

### RPC 連線池（`bot/src/rpc.rs`）

端點在 `[[rpc]]` 宣告（`http_env` 指向 `.env` 裡的變數名稱；公共端點可以直接用 `http`）。連線池依優先順序競速或備援，遵守每個端點的 `skip_methods` 和 log 範圍限制；遇到「方法不存在」、限流和容量錯誤會換端點（並進入冷卻）；節點落後時回的 "block not found / header not found" 視為*落後*：換下一個端點，但不讓落後的端點進入冷卻，除非它一再落後。log 只會出現端點名稱。

### 同塊路徑（`bot/src/flashblocks.rs`、`bot/src/oracle.rs`）

Base 會串流約 200 ms 一個的子區塊（Flashblocks）。bot 直接從串流裡的原始交易解出 Chainlink 的 transmit（OCR report -> observations 的中位數），在本地算出新的 Morpho 預言機價格，只重算那個市場，並在快照區塊上、用 state override 鎖定新價格來規劃。SVR feed 另外處理：公開價格要等對應的 secondary round 上鏈，或過了該 feed 的 cutoff 才生效。`[flashblocks] send = false` 讓這條路徑只寫 journal。注意：Base 計畫中的 "Denim" 升級會移除 Flashblocks；本地價格模型和單一市場重算在更快的出塊下仍然有用，所以資料層沒有和 Flashblocks API 綁死。

### Atlas / Chainlink SVR（`bot/src/atlas.rs`）

在啟用 SVR 的 feed 上，預言機更新後的清算權會透過 Fastlane Atlas 拍賣。這個模組監聽拍賣，用同一個 planner 對每一場估值（先對整個市場 `quote_bundle`，再逐倉位規劃），把我們可以出的價寫進 journal。它從不送出任何東西。`value-auction` 可以重播過去的拍賣。

### 儀表板和即時頁面

`bot dashboard` 把 journal 和執行 log 匯入 SQLite（`dashboard.db_path`），產生一個靜態 HTML 頁面（不需要伺服器）：KPI、延遲、清算、拍賣估值、鏈上清算的出場路徑分析（從 tx 和 receipt 解碼一次後快取）、RPC 用量。`bot live` 在 `127.0.0.1:live_port` 提供唯讀頁面，顯示最接近被清算的倉位，資料來自 `watch` 每個區塊寫出的快照。

## 加入一個協議

1. 建立 `bot/src/<protocol>/`，實作 `LendingProtocol`（參考 `morpho/mod.rs`）。選好關聯型別；讀取要批次；`evaluate` 要是純函式。
2. 為你的市場型別實作 `MarketInfo`（`key()` 是穩定的 `B256`，`name()`）。
3. 從 `bot/src/testkit.rs` 開始：`Toy` 是一個完整的非 Morpho 協議（position = (負債, 抵押品)，snapshot = 價格），engine 和 planner 的測試都用它跑。照它的形狀為自己的實作寫單元測試。
4. 接上應用層：目前 `main.rs` 用 `morpho::MorphoEngine`；為你的協議加同樣的別名，以及一個像 `[morpho]` 的設定區段。
5. 協議沒有清算 callback 的話，清算必須在你的合約裡用閃電貸取得資金；見「加入一個 venue」。

## 加入一個 DEX（聚合器）

實作 `dex::Dex`（參考 `router.rs` 裡的 `impl Dex for Kyber`）：`name()` 會成為 `Plan::router`，`quote_within` 必須遵守 gas 預算，`build` 回傳 venue 要執行的呼叫。`testkit::MockDex`（每單位抵押品換 2 單位借款代幣，經由 `0x22..`）和 `planner.rs` 裡的 `generic_tests` 示範了完整的聚合器路徑，包含白名單檢查。記得把 router 加進 `exec.allowed_targets` 和合約的白名單。

## 加入一個 venue（清算合約）

為你的合約實作 `venue::LiquidationVenue<L>`：`encode` 必須回傳你實際會送出的 calldata，以及 `minProfit` 所在的 byte 位置；`simulate` 必須在下一個區塊上跑這份 calldata（參考 `Simulator::simulate`）。`testkit::MockVenue` 會標記 packed 和 call 兩種路線，從不模擬。部署之前，先照上面的表寫一套 Foundry 安全測試。

## 常用指令

```bash
cd contracts && forge test -vv                      # 合約測試（fork 測試用 BASE_RPC_URL）
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
