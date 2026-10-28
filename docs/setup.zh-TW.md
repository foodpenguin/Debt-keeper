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

```bash
git clone --recurse-submodules <repo-url> liquidation-framework
cd liquidation-framework
git submodule update --init --recursive    # 如果 clone 時沒加 --recurse-submodules（forge-std）
cd contracts && forge build && cd ..        # 同時產生 bot 模擬時要用的 artifact
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

至少要檢查：`[[rpc]]`、`[morpho]`（地址、indexer API、你要追蹤的 `[[morpho.markets]]`）、`[exec]`（先維持 `mode = "shadow"`）、`[router]`（`mode` 和你的 DEX factory）。市場要謹慎挑選：排除代幣來路不明或假幣市場的白名單是一項安全措施，不只是篩選條件。

## 5. 編譯並以影子模式執行

```bash
cargo build --release
./target/release/bot -c config.local.toml scan --top 5   # 一次性：載入倉位，用 indexer 驗證 HF
./target/release/bot -c config.local.toml watch          # 影子模式：規劃和模擬都寫進 journal，不送任何交易
```

影子模式會寫出 `data/journal.jsonl`。先跑一段時間並閱讀結果（`report`、`dashboard`），再考慮實跑。
`bot dashboard`（或每分鐘跑一次的 `scripts/dashboard_loop.sh`）會產生 `data/dashboard.html`；時間預設
是 UTC，可用 `BOT_DASH_TZ_HOURS` 調整（例如 `8` 或 `-5.5`）。

## 6. 測試

```bash
cargo test                                      # 單元測試（不需網路）
cargo test e2e -- --ignored --nocapture         # 在本地 anvil fork 上測試實跑路徑（需要 anvil；BASE_FORK_URL 可選）
cd ../contracts
forge test --no-match-contract Fork -vv         # 不需要 RPC 的測試
BASE_RPC_URL=<你的 url> forge test -j 1 -vv     # fork 測試；-j 1 避免免費方案的 RPC 被限流
```

沒有設定 `BASE_RPC_URL` 時，fork 測試預設使用公共端點。含 key 的 URL 只能透過環境變數傳入，絕不要寫進會 commit 的檔案。

## 7. 私鑰

熱錢包只負責付 gas；利潤會送到合約裡固定的另一個接收者（冷錢包）。私鑰只存在加密的 Foundry keystore：

```bash
cast wallet import bot-hot --interactive   # 會提示輸入私鑰和密碼
```

絕不要把私鑰或 keystore 密碼放進程式碼、設定檔、`.env`、shell 歷史或 log。bot 會從 `exec.keystore_password_env` 指定的環境變數讀取密碼（讀完立刻從 process 環境中刪除），或用提示輸入。熱錢包只放少量的 ETH 付 gas。

## 8. 實跑（只能由 operator 執行）

1. 部署並設定合約：`scripts/deploy_liquidator.sh <HOT> <COLD>`，再由 COLD 接受 owner 身分（`scripts/accept_ownership.sh`）。兩者都會簽主網交易，請自己執行。
2. 第一次實跑前，設定並再次確認：`exec.max_priority_fee_wei`（priority fee 上限）、`exec.daily_gas_budget_usd`、市場白名單、`exec.kill_switch_file`、`exec.allowed_targets`（必須和合約的白名單一致）、`exec.liquidator`、`exec.keystore`。
3. 設定 `exec.mode = "live"`，由你本人啟動 `watch --live`。合約、executor、授權或餘額不對時，pre-flight 檢查會拒絕啟動。
4. 緊急停止：建立緊急停止檔案（之後每個決策都會變成 skip），或由 owner 或 executor 呼叫合約的 `pause()`。

如果有多個 bot 用同一個熱錢包簽名，把 `exec.nonce_file` 留空，它們就會共用以錢包為單位的 nonce 鎖（`~/.local/state/onchain-bots/nonce-<錢包>`）。
