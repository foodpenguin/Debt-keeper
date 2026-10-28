# 貢獻指南

[English](CONTRIBUTING.md)

感謝你的幫忙。有人實跑時，這個專案動用的是真金白銀，所以正確性和安全優先於新功能。

## 開始之前

- 先讀 [docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)：trait 介面（`LendingProtocol`、
  `LiquidationVenue`、`Dex`），以及核心為什麼長這樣。
- 較大的改動（新協議、新 venue、改引擎或合約）請先開 issue 討論做法。
- 鏈的基礎管線（RPC 池、executor、模擬、關卡、出價）放在 `onchain-core`，這是另一個 repo，和這個
  repo 並排 checkout；那裡的改動請在那個 repo 開 pull request。

## 開發環境

見 [docs/setup.zh-TW.md](docs/setup.zh-TW.md)。簡單說：Rust（stable）、Foundry、
`git submodule update --init`，以及 `.env` 裡一個給 fork 測試用的 RPC 端點。

## 每個 pull request 都要通過的檢查

```
cd bot && cargo check --all-targets     # 0 warnings
cd bot && cargo test
cd contracts && forge build && forge test   # fork 測試需要 BASE_RPC_URL；免費方案請加 -j 1
```

改到任何會送交易的程式時，還必須在本地 anvil fork 上通過 `cargo test e2e -- --ignored`。

## 原則

- **透過 trait 擴充**，不要在核心裡加特例。新的協議、DEX 或 venue 要附一個用它驅動引擎和／或
  planner 的測試（參考 `testkit.rs`）。
- **熱路徑要快**：批次讀取、增量更新、受 gas 預算限制的路由和同區塊路徑都是設計的一部分。如果
  一定要移動某個最佳化，請搬過去，不要直接拿掉。
- **合約**：每個新的對外入口或 callback 都要有「誰可以呼叫、什麼時候可以呼叫」的測試（參考
  `MorphoLiquidator.security.t.sol`）。每條路徑都要保留最低利潤檢查和目標白名單。
- 會遷移的 venue 地址**放設定檔，不要寫進程式碼**。
- **註解用英文**，風格跟周圍的程式一致。文件有英文和繁體中文（`*.zh-TW.md`）兩版：兩版都要更新，
  或在 PR 裡註明翻譯待補。
- commit 裡**不能有機密，也不能有私人策略資料**：不放私鑰、含 API key 的 RPC URL、個人錢包地址，
  以及你不想公開的調校參數。
- 測試和工具絕不送主網交易。

## 授權

提交貢獻即表示你同意你的貢獻和本專案其他部分一樣，採用
[GNU AGPL v3.0 或更新版本](LICENSE)授權。
