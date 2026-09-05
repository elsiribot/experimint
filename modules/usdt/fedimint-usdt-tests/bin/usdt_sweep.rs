//! One-off: sweep the recovered USDT (and residual ETH) out of the old
//! federation's pool `SimpleAccount` using the reconstructed group owner key.
//!
//! Direct-EOA path (no bundler/EntryPoint): the group key is the account
//! owner, so with a little gas it can deploy the counterfactual account via
//! the factory and call `execute(usdt, 0, transfer(dest, amount))` itself.
//!
//!   plan   — read-only: print balances, nonces, gas, and the derived
//!            pool/owner addresses. Signs and broadcasts NOTHING.
//!   run    — fund owner from broadcaster, deploy the account, execute the
//!            USDT transfer to <dest>, then sweep leftover ETH to <dest>.
//!
//! Keys are read from files (never printed). Amounts/addresses/txhashes only.
//!
//! args: usdt-sweep <plan|run> <cfg_dir> <owner_key> <broadcaster_key> <dest_evm> <rpc_url>

use std::str::FromStr;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_usdt_common::{derive_pool_account, evm_address, pool_salt};
use fedimint_usdt_server::config::UsdtConfigConsensus;

const USDT_MODULE_INSTANCE: u16 = 4;

sol! {
    interface ISimpleAccountFactory {
        function createAccount(address owner, uint256 salt) external returns (address);
    }
    interface ISimpleAccount {
        function execute(address dest, uint256 value, bytes calldata func) external;
    }
    #[sol(rpc)]
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address who) external view returns (uint256);
    }
}

fn read_signer(path: &str) -> anyhow::Result<PrivateKeySigner> {
    let hex = std::fs::read_to_string(path)?.trim().trim_start_matches("0x").to_string();
    Ok(PrivateKeySigner::from_slice(&hex::decode(hex)?)?)
}

fn cfg(cfg_dir: &str) -> anyhow::Result<UsdtConfigConsensus> {
    let consensus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{cfg_dir}/consensus.json"))?)?;
    let hexc = consensus["modules"][USDT_MODULE_INSTANCE.to_string()]["config"]
        .as_str()
        .context("missing usdt module config")?;
    UsdtConfigConsensus::consensus_decode_whole(&hex::decode(hexc)?, &ModuleDecoderRegistry::default())
        .context("decode UsdtConfigConsensus")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    ensure!(a.len() == 7, "usage: usdt-sweep <plan|run> <cfg_dir> <owner_key> <bc_key> <dest> <rpc>");
    let (mode, cfg_dir, owner_key, bc_key, dest, rpc) =
        (&a[1], &a[2], &a[3], &a[4], &a[5], &a[6]);

    let c = cfg(cfg_dir)?;
    let owner = read_signer(owner_key)?;
    let bc = read_signer(bc_key)?;
    let dest = Address::from_str(dest)?;
    let factory = Address::from(c.account_factory.0);
    let usdt = Address::from(c.usdt_contract.0);
    let pool = Address::from(derive_pool_account(&c.group_public_key, c.account_factory, c.simple_account_impl).0);
    let owner_eoa = Address::from(evm_address(&c.group_public_key).0);

    ensure!(owner.address() == owner_eoa, "owner key does not match consensus group EOA");

    let provider = ProviderBuilder::new().connect(rpc).await?;
    let usdt_c = IERC20::new(usdt, &provider);
    let pool_usdt = usdt_c.balanceOf(pool).call().await?;
    let owner_eth = provider.get_balance(owner_eoa).await?;
    let bc_eth = provider.get_balance(bc.address()).await?;
    let pool_code = provider.get_code_at(pool).await?;
    let gas_price = provider.get_gas_price().await?;
    let chain_id = provider.get_chain_id().await?;

    println!("chain_id:        {chain_id}");
    println!("owner EOA:       {owner_eoa}  (ETH {})", owner_eth);
    println!("broadcaster EOA: {}  (ETH {})", bc.address(), bc_eth);
    println!("pool account:    {pool}  (deployed: {})", !pool_code.is_empty());
    println!("pool USDT:       {pool_usdt}");
    println!("dest:            {dest}");
    println!("gas price:       {gas_price} wei");

    if mode == "plan" {
        println!("\n(plan only — nothing signed or sent)");
        return Ok(());
    }
    ensure!(mode == "run", "mode must be plan or run");
    ensure!(pool_usdt > U256::ZERO, "pool holds no USDT");

    // Gas budget: createAccount (~290k) + execute/transfer (~120k) + 3 sends.
    let gp = U256::from(gas_price * 2); // headroom; mainnet gas is ~0.15 gwei
    let owner_need = gp * U256::from(450_000u64);

    // 1. Fund the owner EOA from the broadcaster, if short.
    let bc_prov = {
        let w = EthereumWallet::from(bc.clone());
        ProviderBuilder::new().wallet(w).connect(rpc).await?
    };
    if owner_eth < owner_need {
        let top_up = owner_need - owner_eth;
        println!("\nfunding owner {top_up} wei from broadcaster...");
        let tx = TransactionRequest::default().with_to(owner_eoa).with_value(top_up);
        let r = bc_prov.send_transaction(tx).await?.get_receipt().await?;
        println!("  fund tx: {}", r.transaction_hash);
    }

    let owner_prov = {
        let w = EthereumWallet::from(owner.clone());
        ProviderBuilder::new().wallet(w).connect(rpc).await?
    };

    // 2. Deploy the SimpleAccount via the factory (idempotent — createAccount
    //    returns the existing address if already deployed).
    if pool_code.is_empty() {
        println!("deploying pool SimpleAccount via factory...");
        let data = ISimpleAccountFactory::createAccountCall {
            owner: owner_eoa,
            salt: U256::from_be_bytes(pool_salt()),
        }
        .abi_encode();
        let tx = TransactionRequest::default().with_to(factory).with_input(Bytes::from(data));
        let r = owner_prov.send_transaction(tx).await?.get_receipt().await?;
        println!("  deploy tx: {} (status {})", r.transaction_hash, r.status());
        let code = provider.get_code_at(pool).await?;
        ensure!(!code.is_empty(), "account still not deployed at {pool}");
    }

    // 3. execute(usdt, 0, transfer(dest, pool_usdt)) as the owner.
    println!("sweeping {pool_usdt} USDT -> {dest}...");
    let transfer = IERC20::transferCall { to: dest, amount: pool_usdt }.abi_encode();
    let exec = ISimpleAccount::executeCall {
        dest: usdt,
        value: U256::ZERO,
        func: Bytes::from(transfer),
    }
    .abi_encode();
    let tx = TransactionRequest::default().with_to(pool).with_input(Bytes::from(exec));
    let r = owner_prov.send_transaction(tx).await?.get_receipt().await?;
    println!("  usdt sweep tx: {} (status {})", r.transaction_hash, r.status());
    let after = usdt_c.balanceOf(pool).call().await?;
    ensure!(after == U256::ZERO, "pool still holds {after} USDT after sweep");
    println!("  pool USDT now: {after}");

    // 4. Return the owner EOA's leftover ETH to the BROADCASTER (not to dest):
    //    the broadcaster is reused by the successor federation and must stay
    //    funded. The broadcaster's own balance is deliberately left untouched.
    let owner_bal = provider.get_balance(owner_eoa).await?;
    let cost = gp * U256::from(21_000u64);
    if owner_bal > cost {
        let send = owner_bal - cost;
        let tx = TransactionRequest::default()
            .with_to(bc.address())
            .with_value(send)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(gp.to::<u128>())
            .with_max_priority_fee_per_gas((gp / U256::from(10u64)).to::<u128>());
        let rr = owner_prov.send_transaction(tx).await?.get_receipt().await?;
        println!("  returned owner leftover {send} wei to broadcaster: {}", rr.transaction_hash);
    }
    let bc_final = provider.get_balance(bc.address()).await?;
    println!("  broadcaster ETH left in place for the new fed: {bc_final} wei");

    println!("\ndone.");
    Ok(())
}
