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
    struct ExactInputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
    interface ISwapRouter02 { function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256); }
}

const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI_WETH_USDC_500: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const WETH_MARKET: B256 = b256!("8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda");
const WETH_BORROWER: Address = address!("33B5a89294e778d14e8Ce4b0828889B6b81b6488");

/// Anvil's automine runs asynchronously: wait (up to ~6 s) for the in-flight tx's outcome.
async fn wait_outcome<P: Provider>(ex: &mut Executor<P>, block: u64) -> (executor::Inflight, Outcome) {
    for _ in 0..30 {
        if let Some(r) = ex.poll(block).await.unwrap().into_iter().next() {
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

    // 4b. Several in flight at once (consecutive nonces); the same position is never sent twice.
    ex = ex.with_max_inflight(2);
    let b = reader.get_block_number().await.unwrap();
    let mut other = mk(b, b + 2, U256::from(1u64));
    other.market = 1; // another position (same borrower, a second slice of its collateral)
    let i1 = ex.send(mk(b, b + 2, U256::from(1u64))).await.unwrap();
    assert!(ex.send(mk(b, b + 2, U256::from(1u64))).await.is_err(), "this position is already in flight");
    let i2 = ex.send(other).await.unwrap();
    assert_eq!(i2.nonce, i1.nonce + 1, "consecutive nonces");
    assert!(!ex.can_send(), "max_inflight reached");
    let mut settled = vec![];
    for _ in 0..30 {
        settled.extend(ex.poll(b + 1).await.unwrap());
        if settled.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(settled.len(), 2, "both settled");
    assert!(settled.iter().all(|(_, o)| matches!(o, Outcome::Included { success: true, .. })), "{settled:?}");
    ex = ex.with_max_inflight(1);

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
    assert_eq!(reader.get_transaction_count(exec_key.address()).await.unwrap(), start_nonce + 6);
    let recorded: Vec<u64> = std::fs::read_to_string(&nonce_file).unwrap().split_whitespace().filter_map(|x| x.parse().ok()).collect();
    assert_eq!(recorded[0], start_nonce + 6, "the shared file holds the next nonce");
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
    let at_current = simr.simulate(&p, cd.clone(), BlockId::number(n), &StateOverride::default(), None, None, None).await;
    assert!(at_current.is_err(), "healthy at the current block: Morpho must refuse");
    let at_next = simr
        .simulate(&p, cd, BlockId::number(n), &StateOverride::default(), Some(sim::SimAt { number: n + 1, time: ts + DELTA }), None, None)
        .await
        .expect("must simulate as liquidatable at the later block time");
    assert!(at_next.profit > U256::ZERO && at_next.gas_used > 100_000);
    println!("e2e lookahead: current-block sim reverted as expected; next-block sim profit {} gas {}", at_next.profit, at_next.gas_used);
}

/// The flash-funded venue end to end: FlashLiquidator (injected, not deployed) flash-borrows USDC
/// from Morpho or from the V4 PoolManager, calls Morpho's `liquidate` without callback data on a
/// real borrower pushed under water, sells the WETH on Uniswap V3 and repays: the Rust encoding
/// and the contract agree, and both sources make the same profit.
#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_flash_venue_simulates_a_flash_funded_liquidation() {
    use alloy::{eips::BlockId, rpc::types::state::StateOverride, sol_types::SolCall};

    use crate::{
        dex::ExitRoute,
        flash_venue::FlashLiquidatorVenue,
        funding::{BASE_POOL_MANAGER, FlashSource, MorphoBlue, UniswapV4},
        morpho::Morpho,
        venue::LiquidationVenue,
    };
    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const UNI_ROUTER: Address = address!("2626664c2603336E57B271c5C0b26F421741e481");

    let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
    let p = ProviderBuilder::new().connect_http(anvil.endpoint_url());
    let exec = anvil.addresses()[1];
    let meta = onchain::load_market_meta(&p, MORPHO, WETH_MARKET, "WETH/USDC").await.unwrap();
    let p0 = IOraclePrice::new(meta.params.oracle, &p).price().call().await.unwrap();
    p.anvil_set_code(meta.params.oracle, sim::constant_price_oracle(p0 * U256::from(9) / U256::from(10))).await.unwrap();
    let n = p.get_block_number().await.unwrap();

    let seize = U256::from(10u64).pow(U256::from(17)); // 0.1 WETH
    let code = || async {
        Some(sim::LiquidatorCode::build(&p, "../contracts/out/FlashLiquidator.sol/FlashLiquidator.json", &[MORPHO, BASE_POOL_MANAGER, exec, exec, exec], exec).await.unwrap())
    };
    let mut profits = vec![];
    let sources: [Box<dyn FlashSource>; 2] = [Box::new(MorphoBlue { address: MORPHO }), Box::new(UniswapV4 { pool_manager: BASE_POOL_MANAGER })];
    for source in sources {
        let name = source.name();
        let morpho = Morpho::for_tests_at(MORPHO);
        let venue = FlashLiquidatorVenue::new(morpho, None, exec, source, 10, code().await, vec![MORPHO], vec![UNI_ROUTER]);
        let swap = ISwapRouter02::exactInputSingleCall {
            params: ExactInputSingleParams { tokenIn: WETH, tokenOut: USDC, fee: 500u32.try_into().unwrap(), recipient: venue.target(), amountIn: seize, amountOutMinimum: U256::ZERO, sqrtPriceLimitX96: Default::default() },
        }
        .abi_encode();
        let route = ExitRoute::Call { target: UNI_ROUTER, calldata: swap.into() };
        // An upper bound on the repayment is enough: Morpho pulls exactly what it computes and the
        // unspent part goes back to the funder with the loan.
        let payload = venue.encode(&meta, WETH_BORROWER, seize, U256::from(1_000_000_000u64), &route, U256::from(1u8), n + 5);
        let r = venue.simulate(&p, payload.calldata.clone(), BlockId::number(n), &StateOverride::default(), None, None, None).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        // Priced like a real send (the sender gets a balance override): same profit, since no pool
        // on this route reads tx.gasprice.
        let fee = crate::venue::SimFee { max_fee_per_gas: 20_000_000, max_priority_fee_per_gas: 1_000_000 };
        let priced = venue.simulate(&p, payload.calldata, BlockId::number(n), &StateOverride::default(), None, Some(fee), None).await.unwrap_or_else(|e| panic!("{name} (priced): {e}"));
        assert_eq!(priced.profit, r.profit, "{name}: a fee-priced simulation of a tax-free route nets the same");
        println!("e2e flash venue ({name}): profit {} USDC (6dp), gas {}", r.profit, r.gas_used);
        assert!(r.profit > U256::ZERO);
        profits.push(r.profit);
    }
    assert_eq!(profits[0], profits[1], "both 0-fee sources make the same profit");
}

/// The Atlas solver venue end to end (simulation only): solver, its MorphoLiquidator and the Atlas
/// stand-in are all injected; a real borrower pushed under water is liquidated through the packed
/// direct route inside a solver operation that buys and pays an ETH bid. The bid comes out of the
/// profit, and a bid the profit cannot cover makes the operation fail.
#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_atlas_solver_op_pays_its_bid_out_of_the_liquidation() {
    use alloy::{eips::BlockId, rpc::types::state::StateOverride};

    use crate::{
        atlas_venue::{AtlasBid, AtlasSolver, SIM_SOLVER},
        dex::ExitRoute,
        venue::LiquidationVenue,
    };
    const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77"); // Atlas on Base (the stub is injected here)
    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const UNI_ROUTER: Address = address!("2626664c2603336E57B271c5C0b26F421741e481");

    let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
    let p = ProviderBuilder::new().connect_http(anvil.endpoint_url());
    let signer = anvil.addresses()[1];
    let meta = onchain::load_market_meta(&p, MORPHO, WETH_MARKET, "WETH/USDC").await.unwrap();
    let p0 = IOraclePrice::new(meta.params.oracle, &p).price().call().await.unwrap();
    p.anvil_set_code(meta.params.oracle, sim::constant_price_oracle(p0 * U256::from(9) / U256::from(10))).await.unwrap();
    let n = p.get_block_number().await.unwrap();

    let solver_code = sim::LiquidatorCode::build(&p, "../contracts/out/AtlasLiquidationSolver.sol/AtlasLiquidationSolver.json", &[ATLAS, WETH, signer, signer, signer], signer).await.unwrap();
    let liq_code = sim::LiquidatorCode::build(&p, "../contracts/out/MorphoLiquidator.sol/MorphoLiquidator.json", &[MORPHO, SIM_SOLVER, SIM_SOLVER, SIM_SOLVER], SIM_SOLVER).await.unwrap();
    let mut liq = Simulator::new(None, SIM_SOLVER, Some(liq_code), vec![]);
    liq.add_approval(USDC, sim::discover_allowance_key(&p, USDC, liq.liquidator, MORPHO).await.unwrap());
    let art: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("../contracts/out/AtlasStub.sol/AtlasStub.json").unwrap()).unwrap();
    let stub: Bytes = art["deployedBytecode"]["object"].as_str().unwrap().parse().unwrap();
    let solver = AtlasSolver::new(ATLAS, None, signer, WETH, UNI_ROUTER, 500, liq, Some(solver_code), stub);

    let seize = U256::from(10u64).pow(U256::from(17)); // 0.1 WETH
    let route = ExitRoute::Packed(hop(UNI_WETH_USDC_500, 1, 10u128.pow(17)).into());
    let mut nets = vec![];
    for bid_wei in [U256::ZERO, U256::from(10u64).pow(U256::from(15)), U256::from(10u64).pow(U256::from(18))] {
        let venue = AtlasBid { solver: &solver, bid_wei };
        let payload = venue.encode(&meta, WETH_BORROWER, seize, U256::ZERO, &route, U256::ZERO, n + 5);
        let r = venue.simulate(&p, payload.calldata, BlockId::number(n), &StateOverride::default(), None, None, None).await;
        println!("e2e atlas solver: bid {bid_wei} wei -> {:?}", r.as_ref().map(|s| (s.profit, s.gas_used)).map_err(|e| e.to_string()));
        nets.push(r.ok().map(|s| s.profit));
    }
    let (free, bid) = (nets[0].expect("no-bid op succeeds"), nets[1].expect("0.001 ETH bid op succeeds"));
    assert!(free > U256::ZERO && bid < free, "the bid is paid out of the profit: {free} -> {bid}");
    assert!(nets[2].is_none(), "a 1 ETH bid on a 0.1 WETH liquidation cannot be paid");
}

/// A tx given up on while it still sits in the mempool is replaced (same nonce, 0-value
/// self-transfer, higher fees), so later nonces are not stuck behind it. Automine is off: the tx
/// stays pending until a block is mined by hand.
#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_stuck_tx_is_replaced_by_a_cancel() {
    let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
    let url = anvil.endpoint_url();
    let exec_key: PrivateKeySigner = anvil.keys()[1].clone().into();
    let reader = ProviderBuilder::new().connect_http(url.clone());
    reader.anvil_set_auto_mine(false).await.unwrap();
    let bcast = Broadcaster::new(reqwest::Client::new(), vec![("anvil".into(), url.to_string())]);
    let policy = crate::executor::FailurePolicy { max_in_window: 9, window: std::time::Duration::from_secs(3600), cooldown: std::time::Duration::from_secs(3600) };
    let mut ex = Executor::new(reader.clone(), exec_key.clone(), 8453, bcast, policy);
    let n0 = ex.sync_nonce().await.unwrap();
    let b = reader.get_block_number().await.unwrap();
    let base_fee = reader.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await.unwrap().unwrap().header.base_fee_per_gas.unwrap_or(0) as u128;
    let req = SendRequest {
        payload: Payload { to: anvil.addresses()[2], calldata: Bytes::from(vec![0u8; 64]), min_profit: executor::MinProfitSlot { offset: 0, width: 32 } },
        min_profit: U256::ZERO,
        gas_limit: 100_000,
        base_fee_wei: base_fee.max(1),
        priority_fee_wei: 1_000_000,
        market: 0,
        user: Address::ZERO,
        block: b,
        deadline_block: b + 1,
    };
    let inf = ex.send(req).await.unwrap();
    assert_eq!(inf.nonce, n0);
    // Long past the deadline and still no block: given up on, and replaced in the pool.
    let out = ex.poll(b + 1 + executor::DROP_GRACE_BLOCKS + 1).await.unwrap();
    assert!(matches!(out.as_slice(), [(_, Outcome::Dropped)]), "{out:?}");
    reader.evm_mine(None).await.unwrap();
    let r: serde_json::Value = reader.raw_request("eth_getTransactionReceipt".into(), (inf.hash,)).await.unwrap();
    assert!(r.is_null(), "the stuck liquidation was replaced, never mined");
    assert_eq!(reader.get_transaction_count(exec_key.address()).await.unwrap(), n0 + 1, "its nonce was used by the cancel");
    println!("e2e stuck tx: {} replaced at nonce {n0}", inf.hash);
}
