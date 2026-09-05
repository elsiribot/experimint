//! One-off: fund `from` with gas from `gas_key` if needed, then ERC-20
//! transfer `amount` of `token` to `to`. Keys from files, never printed.
//! args: erc20-send <from_key> <gas_key> <token> <to> <amount> <rpc>
use std::str::FromStr;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::ensure;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address who) external view returns (uint256);
    }
}

fn signer(p: &str) -> anyhow::Result<PrivateKeySigner> {
    Ok(PrivateKeySigner::from_slice(&hex::decode(
        std::fs::read_to_string(p)?.trim().trim_start_matches("0x"),
    )?)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    ensure!(a.len() == 7, "erc20-send <from_key> <gas_key> <token> <to> <amount> <rpc>");
    let from = signer(&a[1])?;
    let gas = signer(&a[2])?;
    let token = Address::from_str(&a[3])?;
    let to = Address::from_str(&a[4])?;
    let amount = U256::from_str(&a[5])?;
    let rpc = &a[6];

    let prov = ProviderBuilder::new().connect(rpc).await?;
    let gp = U256::from(prov.get_gas_price().await? * 2);
    let need = gp * U256::from(120_000u64);
    let bal = prov.get_balance(from.address()).await?;
    if bal < need {
        let gw = EthereumWallet::from(gas.clone());
        let gprov = ProviderBuilder::new().wallet(gw).connect(rpc).await?;
        let tx = TransactionRequest::default().with_to(from.address()).with_value(need - bal);
        let r = gprov.send_transaction(tx).await?.get_receipt().await?;
        println!("gas fund tx: {}", r.transaction_hash);
    }
    let fw = EthereumWallet::from(from.clone());
    let fprov = ProviderBuilder::new().wallet(fw).connect(rpc).await?;
    let data = IERC20::transferCall { to, amount }.abi_encode();
    let tx = TransactionRequest::default().with_to(token).with_input(Bytes::from(data));
    let r = fprov.send_transaction(tx).await?.get_receipt().await?;
    println!("transfer tx: {} status {}", r.transaction_hash, r.status());
    let dest_bal = IERC20::new(token, &prov).balanceOf(to).call().await?;
    println!("dest token balance: {dest_bal}");
    Ok(())
}
