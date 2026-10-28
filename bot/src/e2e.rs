//! End-to-end test of the live path on a LOCAL anvil fork of Base (never mainnet):
//! deploy the liquidator, set it up like the owner would, make a real borrower liquidatable by
//! swapping the oracle code, then let the Executor sign, broadcast and track the transaction.
//!
//! Run: cargo test e2e -- --ignored --nocapture   (needs `anvil` on PATH; BASE_FORK_URL optional)

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    node_bindings::Anvil,
    primitives::{Address, B256, Bytes, U256, address, b256},
    providers::{Provider, ProviderBuilder, ext::AnvilApi},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};

use crate::{
    executor::{self, Broadcaster, Executor, Outcome, Payload, SendRequest},
    morpho::onchain,
    sim::{self, PACKED_MIN_PROFIT, Simulator},
};

sol! {
    #[sol(rpc)]
    interface ILiqAdmin {
        function approveMorpho(address token, bool allowed) external;
        function executor() external view returns (address);
    }
    #[sol(rpc)]
    interface IErc20 { function balanceOf(address) external view returns (uint256); }
    #[sol(rpc)]
    interface IOraclePrice { function price() external view returns (uint256); }
}

const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI_WETH_USDC_500: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const WETH_MARKET: B256 = b256!("8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda");
const WETH_BORROWER: Address = address!("33B5a89294e778d14e8Ce4b0828889B6b81b6488");

/// Anvil's automine runs asynchronously: wait (up to ~6 s) for the in-flight tx's outcome.
async fn wait_outcome<P: Provider>(ex: &mut Executor<P>, block: u64) -> (executor::Inflight, Outcome) {
    for _ in 0..30 {
        if let Some(r) = ex.poll(block).await.unwrap() {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("no receipt from anvil");
}

fn hop(pool: Address, flags: u8, amount: u128) -> Vec<u8> {
    let mut v = pool.to_vec();
    v.push(flags);
    v.extend_from_slice(&amount.to_be_bytes());
    v
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_live_send_on_anvil_fork() {
    let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
    let url = anvil.endpoint_url();
    let owner_key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let exec_key: PrivateKeySigner = anvil.keys()[1].clone().into();
    let receiver = anvil.addresses()[2];
    let owner_p = ProviderBuilder::new().wallet(EthereumWallet::from(owner_key.clone())).connect_http(url.clone());

    // 1. Deploy exactly as the owner would (constructor args: morpho, owner, executor, receiver).
    let artifact: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("../contracts/out/MorphoLiquidator.sol/MorphoLiquidator.json").unwrap()).unwrap();
    let mut init: Vec<u8> = artifact["bytecode"]["object"].as_str().unwrap().parse::<Bytes>().unwrap().to_vec();
    for a in [MORPHO, owner_key.address(), exec_key.address(), receiver] {
        init.extend_from_slice(B256::left_padding_from(a.as_slice()).as_slice());
    }
    let rc = owner_p.send_transaction(TransactionRequest::default().with_deploy_code(Bytes::from(init))).await.unwrap().get_receipt().await.unwrap();
    let liq = rc.contract_address.expect("deployed");
    let admin = ILiqAdmin::new(liq, &owner_p);
    admin.approveMorpho(USDC, true).send().await.unwrap().get_receipt().await.unwrap();
    assert_eq!(admin.executor().call().await.unwrap(), exec_key.address());
    assert!(sim::has_standing_allowance(&owner_p, USDC, liq, MORPHO).await.unwrap());

    // 2. Make a real WETH/USDC borrower liquidatable: oracle code -> constant price 10% lower.
    let meta = onchain::load_market_meta(&owner_p, MORPHO, WETH_MARKET, "WETH/USDC").await.unwrap();
    let p0 = IOraclePrice::new(meta.params.oracle, &owner_p).price().call().await.unwrap();
    owner_p.anvil_set_code(meta.params.oracle, sim::constant_price_oracle(p0 * U256::from(9) / U256::from(10))).await.unwrap();

    // 3. Executor with the hot key, broadcasting to the local node only.
    let reader = ProviderBuilder::new().connect_http(url.clone());
    let bcast = Broadcaster::new(reqwest::Client::new(), vec![("anvil".into(), url.to_string())]);
    let policy = crate::executor::FailurePolicy { max_in_window: 2, window: std::time::Duration::from_secs(3600), cooldown: std::time::Duration::from_secs(3600) };
    let nonce_file = std::env::temp_dir().join(format!("liqbot-e2e-nonce-{}", std::process::id()));
    let mut ex = Executor::new(reader.clone(), exec_key.clone(), 8453, bcast, policy).with_shared_nonce(&nonce_file);
    // Anvil's dev accounts are public keys with real Base history, so the nonce does not start at 0.
    let start_nonce = ex.sync_nonce().await.unwrap();
    let simr = Simulator::new(Some(liq), exec_key.address(), None, vec![]);
    let seize = U256::from(10u64).pow(U256::from(17)); // 0.1 WETH
    let route = hop(UNI_WETH_USDC_500, 1, 10u128.pow(17));

    let base_fee = reader.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await.unwrap().unwrap().header.base_fee_per_gas.unwrap_or(0) as u128;
    println!("e2e: anvil base fee {base_fee} wei");
    let mk = |block: u64, deadline: u64, min_profit: U256| {
        let calldata = simr.calldata_packed(&meta.params, WETH_BORROWER, seize, &route, U256::ZERO, deadline);
        SendRequest {
            payload: Payload { to: liq, calldata, min_profit: PACKED_MIN_PROFIT },
            min_profit,
            gas_limit: 1_500_000,
            base_fee_wei: base_fee.max(1),
            priority_fee_wei: 1_000_000,
            market: 0,
            user: WETH_BORROWER,
            block,
            deadline_block: deadline,
        }
    };

    // 3b. The hot wallet may be shared (manual transfers, other senders): a tx sent around the executor
    // leaves its nonce stale; the next send must resync and go through, not fail.
    let around = ProviderBuilder::new().wallet(EthereumWallet::from(exec_key.clone())).connect_http(url.clone());
    let r = around.send_transaction(TransactionRequest::default().with_to(receiver).with_value(U256::from(1u64))).await.unwrap().get_receipt().await.unwrap();
    assert!(r.status());

    // 4. Happy path: sent, included, profit lands at the cold receiver.
    let b = reader.get_block_number().await.unwrap();
    let before = IErc20::new(USDC, &reader).balanceOf(receiver).call().await.unwrap();
    let inf = ex.send(mk(b, b + 2, U256::from(1u64))).await.unwrap();
    assert_eq!(inf.nonce, start_nonce + 1, "resent with the fresh nonce after \"nonce too low\"");
    assert!(ex.send(mk(b, b + 2, U256::from(1u64))).await.is_err(), "second send while in flight must be refused");
    let (_, out) = wait_outcome(&mut ex, b + 1).await;
    let Outcome::Included { success, gas_used, .. } = out else { panic!("{out:?}") };
    assert!(success, "liquidation tx reverted");
    let profit = IErc20::new(USDC, &reader).balanceOf(receiver).call().await.unwrap() - before;
    assert!(profit > U256::ZERO);
    println!("e2e: tx {} gas {} profit {} USDC (6dp)", inf.hash, gas_used, profit);
    assert_eq!(ex.consecutive_failures, 0);

    // 5. Expired deadline -> mined but reverts -> counted as a failure.
    let b = reader.get_block_number().await.unwrap();
    ex.send(mk(b, b - 1, U256::from(1u64))).await.unwrap();
    let (_, out) = wait_outcome(&mut ex, b + 1).await;
    assert!(matches!(out, Outcome::Included { success: false, .. }), "{out:?}");
    assert_eq!(ex.consecutive_failures, 1);

    // 6. minProfit above what the liquidation yields -> reverts (InsufficientProfit) -> halt at 2.
    let b = reader.get_block_number().await.unwrap();
    ex.send(mk(b, b + 2, U256::from(1_000_000_000_000u64))).await.unwrap();
    let (_, out) = wait_outcome(&mut ex, b + 1).await;
    assert!(matches!(out, Outcome::Included { success: false, .. }), "{out:?}");
    assert!(ex.halted(), "executor must pause after max_in_window failures");
    assert!(ex.send(mk(b, b + 2, U256::from(1u64))).await.is_err(), "halted executor must refuse to send");

    // 7. Nonces stayed consistent across successes, reverts and the tx sent around the executor.
    assert_eq!(reader.get_transaction_count(exec_key.address()).await.unwrap(), start_nonce + 4);
    let recorded: Vec<u64> = std::fs::read_to_string(&nonce_file).unwrap().split_whitespace().filter_map(|x| x.parse().ok()).collect();
    assert_eq!(recorded[0], start_nonce + 4, "the shared file holds the next nonce");
    let _ = std::fs::remove_file(&nonce_file);
    let _ = executor::deadline_blocks(4.0, 2.0);
}

/// Fix for "lookahead candidates are simulated at the current block": a position that is healthy
/// now but goes under purely through interest must simulate as liquidatable when run as the next
/// block (number + 1, later timestamp), and as healthy when run at the current block.
#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_interest_edge_simulates_at_next_block_time() {
    use crate::morpho::math;
    use alloy::{eips::BlockId, rpc::types::state::StateOverride};
    const DELTA: u64 = 30 * 86_400; // long horizon so the interest margin is unambiguous
    let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
    let url = anvil.endpoint_url();
    let owner_key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let exec_key: PrivateKeySigner = anvil.keys()[1].clone().into();
    let p = ProviderBuilder::new().wallet(EthereumWallet::from(owner_key.clone())).connect_http(url.clone());

    let artifact: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("../contracts/out/MorphoLiquidator.sol/MorphoLiquidator.json").unwrap()).unwrap();
    let mut init: Vec<u8> = artifact["bytecode"]["object"].as_str().unwrap().parse::<Bytes>().unwrap().to_vec();
    for a in [MORPHO, owner_key.address(), exec_key.address(), anvil.addresses()[2]] {
        init.extend_from_slice(B256::left_padding_from(a.as_slice()).as_slice());
    }
    let liq = p.send_transaction(TransactionRequest::default().with_deploy_code(Bytes::from(init))).await.unwrap().get_receipt().await.unwrap().contract_address.unwrap();
    ILiqAdmin::new(liq, &p).approveMorpho(USDC, true).send().await.unwrap().get_receipt().await.unwrap();

    let meta = onchain::load_market_meta(&p, MORPHO, WETH_MARKET, "WETH/USDC").await.unwrap();
    let head = p.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await.unwrap().unwrap().header;
    let (n, ts) = (head.number, head.timestamp);
    let (shares, coll) = onchain::load_positions(&p, MORPHO, &[(WETH_MARKET, WETH_BORROWER)], BlockId::number(n)).await.unwrap()[0].unwrap();
    let snap = |t: u64| {
        let p = &p;
        let meta = meta.clone();
        async move { onchain::load_snapshots(p, MORPHO, &[meta], BlockId::number(n), t, None).await.unwrap()[0].clone().unwrap() }
    };
    let (now, later) = (snap(ts).await, snap(ts + DELTA).await);
    // Oracle price at which max_borrow == debt, at a given time.
    let boundary = |t: &onchain::MarketSnapshot| {
        let debt = math::to_assets_up(shares, t.totals.total_borrow_assets, t.totals.total_borrow_shares);
        math::mul_div_up(math::mul_div_up(debt, math::WAD, meta.params.lltv), math::ORACLE_PRICE_SCALE, coll)
    };
    let price = (boundary(&now) + boundary(&later)) / U256::from(2);
    assert!(!math::health(shares, coll, price, meta.params.lltv, now.totals).is_liquidatable(), "must be healthy now");
    assert!(math::health(shares, coll, price, meta.params.lltv, later.totals).is_liquidatable(), "must be liquidatable later");
    p.anvil_set_code(meta.params.oracle, sim::constant_price_oracle(price)).await.unwrap();

    let simr = Simulator::new(Some(liq), exec_key.address(), None, vec![]);
    let seize = U256::from(10u64).pow(U256::from(16)); // 0.01 WETH
    let cd = simr.calldata_packed(&meta.params, WETH_BORROWER, seize, &hop(UNI_WETH_USDC_500, 1, 10u128.pow(16)), U256::ZERO, n + 1_000);
    let at_current = simr.simulate(&p, cd.clone(), BlockId::number(n), &StateOverride::default(), None, None).await;
    assert!(at_current.is_err(), "healthy at the current block: Morpho must refuse");
    let at_next = simr
        .simulate(&p, cd, BlockId::number(n), &StateOverride::default(), Some(sim::SimAt { number: n + 1, time: ts + DELTA }), None)
        .await
        .expect("must simulate as liquidatable at the later block time");
    assert!(at_next.profit > U256::ZERO && at_next.gas_used > 100_000);
    println!("e2e lookahead: current-block sim reverted as expected; next-block sim profit {} gas {}", at_next.profit, at_next.gas_used);
}
