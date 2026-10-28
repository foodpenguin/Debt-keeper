# 安裝與設定

[English](setup.md)

適用於 Linux 和 WSL2（Ubuntu 22.04 / 24.04）。本指南的步驟都不會在主網送出交易；實跑是最後另外一個明確的步驟，只能由你本人啟動。

## 1. 工具

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git curl ca-certificates chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   # Rust（edition 2024：stable >= 1.85）
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup  # forge、cast、anvil
```

`chrony` 讓系統時間保持準確（延遲和出塊時間的量測都依賴它）。TLS 使用 rustls，SQLite 是內建編譯，不需要 OpenSSL 或系統的 SQLite。

## 2. 取得程式碼

bot 以路徑依賴 `onchain-core` crate，所以兩個 repo 要並排 clone：

```bash
git clone <onchain-core-url> onchain-core
git clone --recurse-submodules <repo-url> liquidation-framework
cd liquidation-framework
git submodule update --init --recursive    # 如果 clone 時沒加 --recurse-submodules（forge-std）
cd contracts && forge build && cd ..        # 同時產生 bot 模擬時要用的 artifact
                                            # （MorphoLiquidator、FlashLiquidator、AtlasLiquidationSolver、AtlasStub、GridQuoter）
```

## 3. RPC 端點（`.env`）

RPC URL 含有 API key，所以只放在 repo 根目錄的 `.env`（已被 gitignore）：

```bash
cp .env.example .env    # 再填入你真正的 URL
```

設定檔裡永遠不會出現含 key 的 URL：每個 `[[rpc]]` 用 `http_env` 指向變數名稱（公共端點可以直接用 `http`）。log 和錯誤訊息只會出現端點的 `name`。用支援 archive 狀態的託管 RPC 就夠了，不需要自建節點。先確認每個端點支援哪些方法（`eth_simulateV1`、大範圍的 `eth_getLogs`、WebSocket），不支援的用 `skip_methods` / `max_log_blocks` 標明。

## 4. 設定檔

```bash
cd bot
cp config.example.toml config.local.toml   # config.local.toml 已被 gitignore
```

至少要檢查：`[[rpc]]`、`[morpho]`（地址、indexer API、你要追蹤的 `[[morpho.markets]]`）、`[exec]`（先維持 `mode = "shadow"`）、`[router]`（`mode` 和你的 DEX factory）。市場要謹慎挑選：排除代幣來路不明或假幣市場的白名單是一項安全措施，不只是篩選條件。bot 只接受借款代幣能以 USD 計價的市場（USD 穩定幣 `USDC`、`USDbC`、`USDT`、`DAI`、`USDS`，以及 `WETH`）；有其他借款代幣的市場會讓它拒絕啟動。

其他值得知道的設定：

- `exec.max_inflight`（預設 1）：同時在飛的清算交易數（nonce 連續，每個市場最多一筆）；調高可以在暴跌時同時接下好幾筆清算。
- 出價：`priority_fee_wei`（基本值）、`max_priority_fee_wei`（上限）、`priority_fee_share` / `bid_share_large` / `bid_large_net_usd`（依規模決定出價最多可以用掉多少淨利），以及競爭模型（`bid_quantile`、`bid_outbid_bps`、`bid_min_samples`、`bid_window_secs`）；見 `docs/architecture.zh-TW.md` 的「出價」。
- `exec.kill_switch_file`：相對路徑以設定檔所在目錄為基準，而不是你啟動 bot 時所在的目錄。
- `accrual_lookahead_secs`：不設定時跟著實測的出塊時間（無條件進位，1 到 2 秒）。
- 啟動時會檢查設定：`exec.venue = "flash"` 搭配 `router.mode = "direct"`，或搭配 `[flashblocks] enabled = true` 且 `flashblocks.router = "direct"`，都會被拒絕（這個 venue 只用聚合器出場）。

清算合約（`[exec] venue`）：

- `"callback"`（預設）：MorphoLiquidator，由 Morpho 的清算 callback 籌措還款。
- `"flash"`：FlashLiquidator，先閃電借入還款（給沒有 callback 的協議用）。`flash_funding = "morpho"`（Morpho 閃電貸）或 `"v4"`（Uniswap V4 閃電記帳，需要 `pool_manager`）；`flash_headroom_bps` 是在規劃的還款之上多借的比例；`flash_liquidator` 是已部署的合約（未設定時用注入的 bytecode 模擬）。這個 venue 只會使用聚合器出場（它沒有 packed 路線入口）。

設定 `[atlas.solver] simulate = true` 時，每場被估值的 SVR 拍賣，最大倉位還會被模擬成一個 Atlas solver operation（寫進 journal）。`submit = true` 還會簽名並送出它，只在實跑模式下、而且 solver 已部署、atlETH 已 bond 之後才會：見 `docs/architecture.zh-TW.md` 的「SVR 拍賣出價：操作者步驟」。

## 5. 編譯並以影子模式執行

```bash
cargo build --release
./target/release/bot -c config.local.toml scan --top 5   # 一次性：載入倉位，用 indexer 驗證 HF
./target/release/bot -c config.local.toml watch          # 影子模式：規劃和模擬都寫進 journal，不送任何交易
```

影子模式會寫出 `data/journal.jsonl`。先跑一段時間並閱讀結果（`report`、`dashboard`），再考慮實跑。RPC 用量：區塊路徑每個區塊都會讀被動到的 log 和每個市場一份快照。期限和利息提前量會跟著實測的出塊時間調整，但在 200 ms 出塊下（Base 計畫中的 "Denim" 升級）這些工作的頻率約變成 10 倍，RPC 用量也約變成 10 倍：請檢查你的方案額度。
`bot dashboard`（或每分鐘跑一次的 `scripts/dashboard_loop.sh`）會產生 `data/dashboard.html`；時間預設
是 UTC，可用 `BOT_DASH_TZ_HOURS` 調整（例如 `8` 或 `-5.5`）。

## 6. 測試

```bash
cargo test                                      # 單元測試（不需網路）
cargo test e2e -- --ignored --nocapture         # 本地 anvil fork（需要 anvil；BASE_FORK_URL 可選）：實跑送單路徑、
                                                # 多筆交易同時在飛、卡住交易的取消、閃電出資的 venue、
                                                # Atlas solver op、Atlas EIP-712 digest 和 Base 對照
cd ../contracts
forge test --no-match-contract Fork -vv         # 不需要 RPC 的測試
BASE_RPC_URL=<你的 url> forge test -j 1 -vv     # fork 測試；-j 1 避免免費方案的 RPC 被限流
```

沒有設定 `BASE_RPC_URL` 時，fork 測試預設使用公共端點。fork 測試固定在某個區塊上跑，結果才不會隨鏈上狀態漂移，所以端點必須提供 archive 狀態（`FORK_BLOCK=<n>` 可改區塊，`FORK_BLOCK=0` 表示最新區塊）。含 key 的 URL 只能透過環境變數傳入，絕不要寫進會 commit 的檔案。

## 7. 私鑰

熱錢包只負責付 gas；利潤會送到合約裡固定的另一個接收者（冷錢包）。私鑰只存在加密的 Foundry keystore：

```bash
cast wallet import bot-hot --interactive   # 會提示輸入私鑰和密碼
```

絕不要把私鑰或 keystore 密碼放進程式碼、設定檔、`.env`、shell 歷史或 log。bot 會從 `exec.keystore_password_env` 指定的環境變數讀取密碼（讀完立刻從 process 環境中刪除），或用提示輸入。熱錢包只放少量的 ETH 付 gas。

## 8. 實跑（只能由 operator 執行）

1. 部署並設定合約：`scripts/deploy_liquidator.sh <HOT> <COLD>`，再由 COLD 接受 owner 身分（`scripts/accept_ownership.sh`）。兩者都會簽主網交易，請自己執行。
2. 第一次實跑前，設定並再次確認：`exec.max_priority_fee_wei`（priority fee 上限）、`exec.daily_gas_budget_usd`、市場白名單、`exec.kill_switch_file`、`exec.allowed_targets`（必須和合約的白名單一致）、`exec.liquidator`、`exec.keystore`。
3. 設定 `exec.mode = "live"`，由你本人啟動 `watch --live`。合約、executor、授權或餘額不對時，pre-flight 檢查會拒絕啟動。`exec.venue = "flash"` 時改檢查 `exec.flash_liquidator`、Morpho 是否在它的 `allowedProtocol` 裡，以及它的 `MORPHO()`／`POOL_MANAGER()` 是否和設定的 `flash_funding` 一致。FlashLiquidator 沒有部署腳本：請自行部署，constructor 是 `(morpho, poolManager, owner, executor, profitReceiver)`，再用 `setAllowedProtocol` 加入協議的清算目標、用 `setAllowedTarget` 加入你的聚合器 router。
4. 緊急停止：建立緊急停止檔案（之後每個決策都會變成 skip），或由 owner 或 executor 呼叫合約的 `pause()`。

如果有多個 bot 用同一個熱錢包簽名，把 `exec.nonce_file` 留空，它們就會共用以錢包為單位的 nonce 鎖（`~/.local/state/onchain-bots/nonce-<錢包>`）。
