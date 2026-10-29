//! Bidding in Chainlink SVR auctions (Fastlane Atlas): a solver operation built from a valued
//! auction, EIP-712 signed by the bonded solver EOA, and submitted to the auction's bid endpoint.
//!
//! Checked against the deployed contracts on Base (2026-10-29):
//! - the signed struct is `SolverOperation(address from,address to,uint256 value,uint256 gas,
//!   uint256 maxFeePerGas,uint256 deadline,address solver,address control,bytes32 userOpHash,
//!   address bidToken,uint256 bidAmount,bytes data)` (the op without its signature), under the
//!   domain `AtlasVerification` / its `eip712Domain()` version, verifying contract =
//!   `Atlas.VERIFICATION()`. The digest equals `AtlasVerification.getSolverPayload(op)`
//!   (`e2e_solver_op_digest_matches_atlas_verification`);
//! - the signature is the raw secp256k1 signature of that digest, `r || s || v` with v = 27 / 28
//!   (Fastlane's atlas-sdk-go: `utils.SignMessage(hash)`);
//! - submission (docs.chain.link/data-feeds/svr-feeds/searcher-onboarding-atlas): JSON-RPC
//!   `solver_submitSolverOperation` with `[{ auction_id, auction_solution }]`, the op as hex
//!   quantities (atlas-sdk-go `SolverOperationRaw`).
//!
//! Operator prerequisites (never done by the bot): atlETH bonded by the signer EOA
//! (`Atlas.depositAndBond`), a deployed AtlasLiquidationSolver whose `executor` is that EOA, and its
//! MorphoLiquidator. See docs/architecture.md ("Bidding in SVR auctions").

use std::sync::Mutex;

use alloy::{
    primitives::{Address, B256, Bytes, U256},
    providers::Provider,
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{Eip712Domain, SolCall, SolStruct},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

sol! {
    /// The signed part of an Atlas solver operation (EIP-712 primary type).
    #[derive(Debug, PartialEq)]
    struct SolverOperation {
        address from;
        address to;
        uint256 value;
        uint256 gas;
        uint256 maxFeePerGas;
        uint256 deadline;
        address solver;
        address control;
        bytes32 userOpHash;
        address bidToken;
        uint256 bidAmount;
        bytes data;
    }
    interface IAtlasViews {
        function VERIFICATION() external view returns (address);
        function balanceOfBonded(address account) external view returns (uint256);
    }
    interface IEip712 {
        function eip712Domain() external view returns (bytes1 fields, string name, string version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] extensions);
    }
    interface IDappControl {
        function getSolverGasLimit() external view returns (uint32);
    }
}

/// The Chainlink SVR bid endpoint (HTTP JSON-RPC).
pub const SVR_BID_ENDPOINT: &str = "https://svr-bid-endpoint.chain.link/";

/// Atlas's EIP-712 domain, as the deployed AtlasVerification reports it.
pub async fn read_domain<P: Provider>(p: &P, atlas: Address) -> Result<Eip712Domain> {
    let call = |to: Address, data: Vec<u8>| alloy::rpc::types::TransactionRequest::default().to(to).input(Bytes::from(data).into());
    let out = p.call(call(atlas, IAtlasViews::VERIFICATIONCall {}.abi_encode())).await.context("Atlas.VERIFICATION()")?;
    let verification = IAtlasViews::VERIFICATIONCall::abi_decode_returns(&out)?;
    let out = p.call(call(verification, IEip712::eip712DomainCall {}.abi_encode())).await.context("AtlasVerification.eip712Domain()")?;
    let d = IEip712::eip712DomainCall::abi_decode_returns(&out)?;
    anyhow::ensure!(d.name == "AtlasVerification" && d.verifyingContract == verification, "unexpected Atlas EIP-712 domain: {} at {}", d.name, d.verifyingContract);
    Ok(Eip712Domain::new(Some(d.name.into()), Some(d.version.into()), Some(d.chainId), Some(d.verifyingContract), None))
}

/// atlETH bonded by `account` (what Atlas charges the solver's gas against).
pub async fn bonded<P: Provider>(p: &P, atlas: Address, account: Address) -> Result<U256> {
    let data = IAtlasViews::balanceOfBondedCall { account }.abi_encode();
    let out = p.call(alloy::rpc::types::TransactionRequest::default().to(atlas).input(Bytes::from(data).into())).await?;
    Ok(IAtlasViews::balanceOfBondedCall::abi_decode_returns(&out)?)
}

/// The most gas a solver operation of this DappControl may take.
pub async fn solver_gas_limit<P: Provider>(p: &P, control: Address) -> Result<u64> {
    let data = IDappControl::getSolverGasLimitCall {}.abi_encode();
    let out = p.call(alloy::rpc::types::TransactionRequest::default().to(control).input(Bytes::from(data).into())).await?;
    Ok(IDappControl::getSolverGasLimitCall::abi_decode_returns(&out)? as u64)
}

/// A signed solver operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedSolverOp {
    pub op: SolverOperation,
    /// 65 bytes, `r || s || v`, v = 27 / 28.
    pub signature: Bytes,
}

impl SignedSolverOp {
    /// The op as the bid endpoint takes it (atlas-sdk-go `SolverOperationRaw`).
    pub fn to_json(&self) -> Value {
        let q = |x: U256| format!("{x:#x}");
        let o = &self.op;
        json!({
            "from": o.from, "to": o.to, "value": q(o.value), "gas": q(o.gas), "maxFeePerGas": q(o.maxFeePerGas),
            "deadline": q(o.deadline), "solver": o.solver, "control": o.control, "userOpHash": o.userOpHash,
            "bidToken": o.bidToken, "bidAmount": q(o.bidAmount), "data": o.data, "signature": self.signature,
        })
    }
}

/// The EIP-712 digest Atlas verifies (`AtlasVerification.getSolverPayload`).
pub fn digest(op: &SolverOperation, domain: &Eip712Domain) -> B256 {
    op.eip712_signing_hash(domain)
}

pub fn sign(op: SolverOperation, domain: &Eip712Domain, signer: &PrivateKeySigner) -> Result<SignedSolverOp> {
    anyhow::ensure!(op.from == signer.address(), "solver op from {} is not the signer {}", op.from, signer.address());
    let sig = signer.sign_hash_sync(&digest(&op, domain)).context("signing the solver op")?;
    Ok(SignedSolverOp { op, signature: Bytes::from(sig.as_bytes().to_vec()) })
}

/// The JSON-RPC request submitting `op` to auction `auction_id`.
pub fn submit_request(auction_id: &str, op: &SignedSolverOp) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "solver_submitSolverOperation", "params": [{ "auction_id": auction_id, "auction_solution": op.to_json() }] })
}

/// Submits a signed op; Ok = the endpoint accepted it (whether it wins is decided on-chain).
pub async fn submit(http: &reqwest::Client, endpoint: &str, auction_id: &str, op: &SignedSolverOp) -> Result<Value> {
    let resp: Value = http.post(endpoint).json(&submit_request(auction_id, op)).send().await.context("bid endpoint unreachable")?.json().await.context("bid endpoint answer")?;
    if let Some(e) = resp.get("error") {
        bail!("bid endpoint refused the solver op: {}", e.to_string().chars().take(200).collect::<String>());
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// What a submission needs besides the auction: the signer, the domain, where to send, and the
/// one-op-per-block rule (an EOA's second op in the same block is dropped by Atlas: use another
/// EOA for parallel auctions).
pub struct Submitter {
    pub signer: PrivateKeySigner,
    pub domain: Eip712Domain,
    pub atlas: Address,
    pub control: Address,
    pub endpoint: String,
    pub http: reqwest::Client,
    /// Most gas an op may declare (the DappControl's limit, read at start).
    pub gas_limit: u64,
    /// What submitted ops may have cost the bond today, each at its worst case (all of its gas at
    /// the auction's max fee): Atlas charges it also when an op fails, outside the executor's view.
    pub budget: Mutex<onchain_core::gates::DailyBudget>,
    last_deadline: Mutex<u64>,
}

impl Submitter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(signer: PrivateKeySigner, domain: Eip712Domain, atlas: Address, control: Address, endpoint: String, http: reqwest::Client, gas_limit: u64) -> Self {
        Self { signer, domain, atlas, control, endpoint, http, gas_limit, budget: Mutex::default(), last_deadline: Mutex::new(0) }
    }

    /// Claims the slot for an op landing by `deadline`; false = this EOA already has one there.
    pub fn claim(&self, deadline: u64) -> bool {
        let mut last = self.last_deadline.lock().unwrap();
        if *last == deadline {
            return false;
        }
        *last = deadline;
        true
    }

    /// Gives the slot for `deadline` back (the op claimed for it was not accepted).
    pub fn unclaim(&self, deadline: u64) {
        let mut last = self.last_deadline.lock().unwrap();
        if *last == deadline {
            *last = 0;
        }
    }

    /// The op for an auction: `data` = the solver's `solverOpData`, `gas` = what it may use, the
    /// auction's own max fee and deadline, an ETH bid.
    #[allow(clippy::too_many_arguments)]
    pub fn op(&self, solver: Address, user_op_hash: B256, deadline: u64, max_fee_per_gas: u128, gas: u64, bid_wei: U256, data: Bytes) -> Result<SolverOperation> {
        anyhow::ensure!(gas <= self.gas_limit, "solver op gas {gas} > the DappControl's limit {}", self.gas_limit);
        Ok(SolverOperation {
            from: self.signer.address(),
            to: self.atlas,
            value: U256::ZERO,
            gas: U256::from(gas),
            maxFeePerGas: U256::from(max_fee_per_gas),
            deadline: U256::from(deadline),
            solver,
            control: self.control,
            userOpHash: user_op_hash,
            bidToken: Address::ZERO,
            bidAmount: bid_wei,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    sol! {
        /// The full op (with its signature), as AtlasVerification takes it.
        struct SolverOperationFull {
            address from;
            address to;
            uint256 value;
            uint256 gas;
            uint256 maxFeePerGas;
            uint256 deadline;
            address solver;
            address control;
            bytes32 userOpHash;
            address bidToken;
            uint256 bidAmount;
            bytes data;
            bytes signature;
        }
        interface IPayload {
            function getSolverPayload(SolverOperationFull op) external view returns (bytes32);
        }
    }

    const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77");
    const VERIFICATION: Address = address!("6e1886aEca75160BAa5610B7c1D3a895198C61cf");

    /// The op whose digest was read from AtlasVerification.getSolverPayload on Base.
    fn known_op() -> SolverOperation {
        SolverOperation {
            from: Address::with_last_byte(1),
            to: ATLAS,
            value: U256::ZERO,
            gas: U256::from(500_000u64),
            maxFeePerGas: U256::from(1_000u64),
            deadline: U256::from(100u64),
            solver: Address::with_last_byte(2),
            control: Address::with_last_byte(3),
            userOpHash: B256::repeat_byte(0x11),
            bidToken: Address::ZERO,
            bidAmount: U256::from(7u64),
            data: Bytes::from_static(&[0x12, 0x34]),
        }
    }

    fn base_domain() -> Eip712Domain {
        Eip712Domain::new(Some("AtlasVerification".into()), Some("1.6.4".into()), Some(U256::from(8453u64)), Some(VERIFICATION), None)
    }

    #[test]
    fn the_digest_is_atlas_verifications_solver_payload() {
        // AtlasVerification(0x6e18..).getSolverPayload(known_op) on Base, eth_call 2026-10-29.
        let expected: B256 = "0x3d82094946536f803345146b77b20a1646f1b9d6fef4a200f10f7c04aa1beab2".parse().unwrap();
        assert_eq!(digest(&known_op(), &base_domain()), expected);
    }

    #[test]
    fn ops_are_signed_by_the_solver_eoa_with_v_27_or_28_and_serialised_like_the_sdk() {
        let signer = PrivateKeySigner::random();
        let mut op = known_op();
        assert!(sign(op.clone(), &base_domain(), &signer).is_err(), "only the op's own `from` signs");
        op.from = signer.address();
        let s = sign(op.clone(), &base_domain(), &signer).unwrap();
        assert_eq!(s.signature.len(), 65);
        assert!(matches!(s.signature[64], 27 | 28));
        let sig = alloy::primitives::Signature::try_from(&s.signature[..]).unwrap();
        assert_eq!(sig.recover_address_from_prehash(&digest(&op, &base_domain())).unwrap(), signer.address());
        let r = submit_request("e1c063a1", &s);
        assert_eq!(r["method"], "solver_submitSolverOperation");
        let sol = &r["params"][0]["auction_solution"];
        assert_eq!((r["params"][0]["auction_id"].as_str(), sol["gas"].as_str(), sol["bidAmount"].as_str(), sol["data"].as_str()), (Some("e1c063a1"), Some("0x7a120"), Some("0x7"), Some("0x1234")));
        assert_eq!(sol["value"], "0x0");
    }

    #[test]
    fn one_op_per_block_and_within_the_dapp_gas_limit() {
        let s = Submitter::new(PrivateKeySigner::random(), base_domain(), ATLAS, Address::with_last_byte(3), SVR_BID_ENDPOINT.into(), reqwest::Client::new(), 6_000_000);
        assert!(s.claim(100) && !s.claim(100) && s.claim(101));
        // An op that was not accepted gives its slot back; an older deadline does not touch it.
        s.unclaim(100);
        assert!(!s.claim(101));
        s.unclaim(101);
        assert!(s.claim(101));
        assert!(s.op(Address::with_last_byte(2), B256::ZERO, 100, 1, 6_000_001, U256::ZERO, Bytes::new()).is_err());
        let op = s.op(Address::with_last_byte(2), B256::ZERO, 100, 1, 600_000, U256::from(5u8), Bytes::new()).unwrap();
        assert_eq!((op.from, op.to, op.bidToken, op.deadline), (s.signer.address(), ATLAS, Address::ZERO, U256::from(100u8)));
    }

    /// A one-shot local HTTP endpoint: answers `reply`, hands back the request body.
    async fn mock_endpoint(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 1 << 16];
            let mut got = Vec::new();
            loop {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                    if got.len() >= i + 4 + len {
                        let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}", reply.len());
                        s.write_all(resp.as_bytes()).await.unwrap();
                        return text[i + 4..].to_string();
                    }
                }
            }
        });
        (url, h)
    }

    #[tokio::test]
    async fn submission_posts_the_signed_op_and_surfaces_a_refusal() {
        let signer = PrivateKeySigner::random();
        let mut op = known_op();
        op.from = signer.address();
        let s = sign(op.clone(), &base_domain(), &signer).unwrap();
        let http = reqwest::Client::new();
        let (url, body) = mock_endpoint(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).await;
        assert_eq!(submit(&http, &url, "auction-1", &s).await.unwrap(), Value::Null);
        let sent: Value = serde_json::from_str(&body.await.unwrap()).unwrap();
        assert_eq!(sent, submit_request("auction-1", &s), "exactly the signed op");
        let (url, _) = mock_endpoint(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"auction ended"}}"#).await;
        let err = submit(&http, &url, "auction-1", &s).await.unwrap_err().to_string();
        assert!(err.contains("auction ended"), "{err}");
    }

    /// Read-only, public RPC: the domain read from the deployed contracts, and our digest of a
    /// random op equal to AtlasVerification.getSolverPayload of it.
    #[tokio::test]
    #[ignore = "network: run with --ignored"]
    async fn e2e_solver_op_digest_matches_atlas_verification() {
        let url = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
        let p = alloy::providers::ProviderBuilder::new().connect_http(url.parse().unwrap());
        let domain = read_domain(&p, ATLAS).await.unwrap();
        assert_eq!(domain.verifying_contract, Some(VERIFICATION));
        let mut op = known_op();
        let r = alloy::primitives::keccak256(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos().to_be_bytes());
        (op.userOpHash, op.bidAmount, op.data) = (r, U256::from(123_456_789u64), Bytes::from(alloy::primitives::keccak256(r).to_vec()));
        let full = SolverOperationFull { from: op.from, to: op.to, value: op.value, gas: op.gas, maxFeePerGas: op.maxFeePerGas, deadline: op.deadline, solver: op.solver, control: op.control, userOpHash: op.userOpHash, bidToken: op.bidToken, bidAmount: op.bidAmount, data: op.data.clone(), signature: Bytes::new() };
        let req = alloy::rpc::types::TransactionRequest::default().to(VERIFICATION).input(Bytes::from(IPayload::getSolverPayloadCall { op: full }.abi_encode()).into());
        let on_chain = IPayload::getSolverPayloadCall::abi_decode_returns(&p.call(req).await.unwrap()).unwrap();
        assert_eq!(digest(&op, &domain), on_chain);
        assert!(solver_gas_limit(&p, address!("a5E1a36938769cbd5a26f5e19D8FCB379f597c83")).await.unwrap() > 0);
    }
}
