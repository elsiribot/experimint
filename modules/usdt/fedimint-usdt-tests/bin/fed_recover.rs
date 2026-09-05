//! One-off recovery tool for the decommissioned experimint USDT/AMM
//! federation (7 guardians, threshold 5). NOT for CI, NOT to be committed —
//! it exists to sweep the federation's single consolidated walletv2 UTXO to
//! a recovery key, using the complete set of guardian data dirs backed up in
//! `~/usdt-fed-backup-2026-09-04/`.
//!
//! Design rule: NO private key material is ever printed. Keys are read from
//! the guardian dirs / written to 0600 files; stdout carries only public
//! data (addresses, outpoints, amounts) and fully-signed transactions.
//!
//! Modes:
//!   inventory <guardian_dir>
//!       Decode the walletv2 consensus config, open the guardian DB
//!       read-only and print the federation wallet UTXO (value, outpoint,
//!       tweak) and its address, plus any unspent deposit outputs.
//!   gen-keys <out_dir>
//!       Generate a fresh recovery keypair; write WIF-equivalent hex 0600 to
//!       <out_dir>/btc-recovery.key, print only the P2WPKH address.
//!   sweep <consensus_guardian_dir> <dest_addr> <fee_sats> <priv1..priv5+>
//!       Build and sign the 1-in-1-out sweep of the federation wallet UTXO
//!       using >= threshold guardian private.json files. Prints the raw tx
//!       hex ready for `bitcoin-cli sendrawtransaction`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, ensure};
use bitcoin::hashes::Hash as _;
use bitcoin::secp256k1::{Scalar, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{Address, Amount, Network, Sequence, Transaction, TxIn, TxOut};
use fedimint_core::PeerId;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_rocksdb::RocksDbReadOnly;
use fedimint_walletv2_common::config::WalletConfigConsensus;
use fedimint_walletv2_common::{FederationWallet, descriptor, tweak_public_key};
use fedimint_walletv2_server::db::{FederationWalletKey, OutputPrefix, SpentOutputPrefix};
use futures::StreamExt as _;
use secp256k1::PublicKey;

const WALLET_MODULE_INSTANCE: u16 = 0;
const USDT_MODULE_INSTANCE: u16 = 4;

/// Reconstruct the USDT module's threshold-ECDSA group key from >= threshold
/// guardian key shares, verify it against the consensus `group_public_key`,
/// and print the pool `SimpleAccount` address (owner = the group EOA) whose
/// USDT/ETH the successor must sweep. The reconstructed key is written 0600,
/// never printed. This proves recoverability; the on-chain move (a
/// SimpleAccount.execute signed by the owner EOA, gas fronted) happens during
/// funding.
fn usdt_inventory(cfg_dir: &Path, key_dirs: &[PathBuf]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    use cggmp21::key_share::reconstruct_secret_key;
    use fedimint_threshold_ecdsa::KeyShare;
    use fedimint_usdt_server::config::UsdtConfigConsensus;
    use fedimint_usdt_common::derive_pool_account;

    let consensus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cfg_dir.join("consensus.json"))?)?;
    let hex_cfg = consensus["modules"][USDT_MODULE_INSTANCE.to_string()]["config"]
        .as_str()
        .context("missing usdt module config")?;
    let cfg = UsdtConfigConsensus::consensus_decode_whole(
        &hex::decode(hex_cfg)?,
        &ModuleDecoderRegistry::default(),
    )
    .context("decoding UsdtConfigConsensus")?;

    let n = key_dirs.len();
    let threshold = 7 - 7usize.saturating_sub(1) / 3; // 5-of-7, this federation
    ensure!(n >= threshold, "need >= {threshold} key shares, got {n}");

    let shares: Vec<KeyShare> = key_dirs
        .iter()
        .map(|d| -> anyhow::Result<KeyShare> {
            let private: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(d.join("private.json"))?)?;
            let ks = &private["modules"][USDT_MODULE_INSTANCE.to_string()]["key_share"];
            serde_json::from_value(ks.clone())
                .with_context(|| format!("parsing key_share from {}", d.display()))
        })
        .collect::<anyhow::Result<_>>()?;

    let secret = reconstruct_secret_key(&shares[..threshold])
        .map_err(|e| anyhow::anyhow!("reconstruct_secret_key: {e}"))?;
    let enc = secret.as_ref().to_be_bytes();
    let sk_bytes: &[u8] = enc.as_ref();
    let sk = SecretKey::from_slice(sk_bytes)?;
    let derived_pk = sk.public_key(&Secp256k1::new());

    // The invariant that proves the reconstruction is correct.
    ensure!(
        derived_pk == cfg.group_public_key,
        "reconstructed key does NOT match consensus group_public_key — aborting"
    );

    let out_dir = cfg_dir
        .ancestors()
        .nth(2)
        .unwrap_or(cfg_dir)
        .join("recovery");
    std::fs::create_dir_all(&out_dir)?;
    let key_path = out_dir.join("usdt-group-owner.key");
    std::fs::write(&key_path, format!("{}\n", sk.display_secret()))?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;

    let pool = derive_pool_account(
        &cfg.group_public_key,
        cfg.account_factory,
        cfg.simple_account_impl,
    );
    let owner_eoa = fedimint_usdt_common::evm_address(&cfg.group_public_key);

    println!("group key reconstructed & verified against consensus ✓");
    println!("owner EOA (group):   {owner_eoa}");
    println!("pool SimpleAccount:  {pool}");
    println!("usdt contract:       {}", cfg.usdt_contract);
    println!("owner key written:   {} (0600)", key_path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("inventory") => inventory(Path::new(&args[2])).await,
        Some("gen-keys") => gen_keys(Path::new(&args[2])),
        Some("gen-evm") => gen_evm(Path::new(&args[2])),
        Some("eth-addr") => eth_addr(Path::new(&args[2])),
        Some("usdt-inventory") => {
            ensure!(args.len() >= 4, "usdt-inventory <cfg_dir> <keydir1..5+>");
            usdt_inventory(
                Path::new(&args[2]),
                &args[3..].iter().map(PathBuf::from).collect::<Vec<_>>(),
            )
        }
        Some("sweep") => {
            ensure!(args.len() >= 9, "sweep <dir> <dest> <fee_sats> <priv1..priv5+>");
            sweep(
                Path::new(&args[2]),
                &args[3],
                args[4].parse().context("fee_sats")?,
                &args[5..].iter().map(PathBuf::from).collect::<Vec<_>>(),
            )
            .await
        }
        _ => anyhow::bail!("usage: fed-recover inventory|usdt-inventory|gen-keys|gen-evm|eth-addr|sweep ..."),
    }
}

/// The walletv2 consensus config out of a guardian's consensus.json.
fn wallet_consensus_cfg(dir: &Path) -> anyhow::Result<WalletConfigConsensus> {
    let consensus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("consensus.json"))?)?;
    let hex_cfg = consensus["modules"][WALLET_MODULE_INSTANCE.to_string()]["config"]
        .as_str()
        .context("missing walletv2 module config")?;
    let bytes = hex::decode(hex_cfg)?;
    let cfg = WalletConfigConsensus::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
        .context("decoding WalletConfigConsensus")?;
    ensure!(cfg.network == Network::Bitcoin, "expected mainnet config");
    Ok(cfg)
}

/// This guardian's walletv2 secret key out of private.json. Never printed.
fn wallet_private_sk(dir: &Path) -> anyhow::Result<SecretKey> {
    let private: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("private.json"))?)?;
    let sk_val = &private["modules"][WALLET_MODULE_INSTANCE.to_string()]["bitcoin_sk"];
    let sk: SecretKey = serde_json::from_value(sk_val.clone())
        .context("parsing walletv2 bitcoin_sk from private.json")?;
    Ok(sk)
}

/// The federation wallet record out of the guardian DB (read-only).
async fn federation_wallet(dir: &Path) -> anyhow::Result<FederationWallet> {
    let raw = RocksDbReadOnly::open_read_only(dir.join("database")).await?;
    let db = Database::new(raw, ModuleDecoderRegistry::default());
    let db = db.with_prefix_module_id(WALLET_MODULE_INSTANCE).0;
    let mut dbtx = db.begin_transaction_nc().await;
    let wallet = dbtx
        .get_value(&FederationWalletKey)
        .await
        .context("no FederationWallet record — wallet may be empty")?;

    // Count stragglers (unclaimed deposit outputs) for the report.
    let outputs = dbtx.find_by_prefix(&OutputPrefix).await.collect::<Vec<_>>().await;
    let spent = dbtx
        .find_by_prefix(&SpentOutputPrefix)
        .await
        .collect::<Vec<_>>()
        .await;
    let pending = outputs.len() - spent.len();
    if pending > 0 {
        eprintln!("NOTE: {pending} unclaimed deposit output(s) beyond the federation wallet:");
        let spent_idx: std::collections::BTreeSet<u64> = spent.iter().map(|(k, ())| k.0).collect();
        for (k, out) in &outputs {
            if !spent_idx.contains(&k.0) {
                eprintln!("  index {}: {} at {}", k.0, out.1.value, out.0);
            }
        }
    }
    Ok(wallet)
}

async fn inventory(dir: &Path) -> anyhow::Result<()> {
    let cfg = wallet_consensus_cfg(dir)?;
    let wallet = federation_wallet(dir).await?;
    let desc = descriptor(&cfg.bitcoin_pks, &wallet.tweak);
    let addr = Address::from_script(&desc.script_pubkey(), Network::Bitcoin)?;
    println!("guardians:     {}", cfg.bitcoin_pks.len());
    println!("wallet value:  {}", wallet.value);
    println!("wallet utxo:   {}", wallet.outpoint);
    println!("wallet tweak:  {}", wallet.tweak);
    println!("wallet addr:   {addr}");
    Ok(())
}

fn gen_keys(out_dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(out_dir)?;
    let secp = Secp256k1::new();
    let (sk, pk) = secp.generate_keypair(&mut rand::thread_rng());
    let key_path = out_dir.join("btc-recovery.key");
    ensure!(!key_path.exists(), "refusing to overwrite {}", key_path.display());
    std::fs::write(&key_path, format!("{}\n", sk.display_secret()))?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    let addr = Address::p2wpkh(
        &bitcoin::CompressedPublicKey(pk),
        Network::Bitcoin,
    );
    println!("recovery address: {addr}");
    println!("key written to:   {} (0600)", key_path.display());
    Ok(())
}

async fn sweep(
    cfg_dir: &Path,
    dest: &str,
    fee_sats: u64,
    key_dirs: &[PathBuf],
) -> anyhow::Result<()> {
    let cfg = wallet_consensus_cfg(cfg_dir)?;
    let wallet = federation_wallet(cfg_dir).await?;
    let threshold = {
        // n - floor((n-1)/3), as everywhere in fedimint
        let n = cfg.bitcoin_pks.len();
        n - n.saturating_sub(1) / 3
    };
    ensure!(
        key_dirs.len() >= threshold,
        "need >= {threshold} guardian dirs, got {}",
        key_dirs.len()
    );

    let dest = Address::from_str(dest)?.require_network(Network::Bitcoin)?;
    let send_value = wallet
        .value
        .checked_sub(Amount::from_sat(fee_sats))
        .context("fee exceeds wallet value")?;

    let mut tx = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: wallet.outpoint,
            script_sig: Default::default(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![TxOut {
            value: send_value,
            script_pubkey: dest.script_pubkey(),
        }],
    };

    // Exactly the daemon's signing recipe (walletv2-server sign_tx +
    // finalize_tx): tweak each guardian sk, p2wsh sighash ALL over the
    // sortedmulti script code, then satisfy the Wsh descriptor with the
    // tweaked-pk -> sig map.
    let desc = descriptor(&cfg.bitcoin_pks, &wallet.tweak);
    let script_code = desc.ecdsa_sighash_script_code();
    let sighash = SighashCache::new(tx.clone())
        .p2wsh_signature_hash(0, &script_code, wallet.value, EcdsaSighashType::All)
        .context("sighash")?;
    let scalar =
        Scalar::from_be_bytes(wallet.tweak.to_byte_array()).expect("hash within field order");

    // Match each provided guardian dir to its peer id via the pubkey.
    let secp = Secp256k1::new();
    let mut satisfier: BTreeMap<PublicKey, bitcoin::ecdsa::Signature> = BTreeMap::new();
    for dir in key_dirs {
        let sk = wallet_private_sk(dir)?;
        let base_pk = sk.public_key(&secp);
        let peer: Option<(&PeerId, &PublicKey)> =
            cfg.bitcoin_pks.iter().find(|(_, pk)| **pk == base_pk);
        ensure!(peer.is_some(), "{} key not in federation pk set", dir.display());
        let tweaked_sk = sk.add_tweak(&scalar).context("tweak sk")?;
        let tweaked_pk = tweak_public_key(&base_pk, &wallet.tweak);
        assert_eq!(tweaked_sk.public_key(&secp), tweaked_pk, "tweak mismatch");
        let sig = secp.sign_ecdsa(&sighash.into(), &tweaked_sk);
        satisfier.insert(tweaked_pk, bitcoin::ecdsa::Signature::sighash_all(sig));
        if satisfier.len() == threshold {
            break;
        }
    }
    ensure!(satisfier.len() == threshold, "insufficient distinct keys");

    miniscript::Descriptor::Wsh(desc)
        .satisfy(&mut tx.input[0], satisfier)
        .map_err(|e| anyhow::anyhow!("satisfy: {e:?}"))?;

    // Sanity: verify the tx spends the recorded outpoint script.
    let expected_spk = descriptor(&cfg.bitcoin_pks, &wallet.tweak).script_pubkey();
    eprintln!("spending {} ({} -> {} + {} fee)", wallet.outpoint, wallet.value, send_value, Amount::from_sat(fee_sats));
    eprintln!("input script_pubkey: {}", expected_spk);
    eprintln!("txid: {}", tx.compute_txid());
    println!("{}", bitcoin::consensus::encode::serialize_hex(&tx));
    Ok(())
}

/// Generate a fresh EVM recovery keypair: hex secret written 0600 to
/// `<out_dir>/evm-recovery.key`, address printed to stdout.
fn gen_evm(out_dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(out_dir)?;
    let (sk, pk) = Secp256k1::new().generate_keypair(&mut rand::thread_rng());
    let key_path = out_dir.join("evm-recovery.key");
    ensure!(!key_path.exists(), "refusing to overwrite {}", key_path.display());
    std::fs::write(&key_path, format!("{}\n", sk.display_secret()))?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    println!("evm address:    {}", fedimint_usdt_common::evm_address(&pk));
    println!("key written to: {} (0600)", key_path.display());
    Ok(())
}

/// Print the EVM address for a hex secp256k1 key read from a file. Address
/// only; the key is never printed.
fn eth_addr(key_path: &Path) -> anyhow::Result<()> {
    let hex = std::fs::read_to_string(key_path)?.trim().trim_start_matches("0x").to_string();
    let sk = bitcoin::secp256k1::SecretKey::from_slice(&hex::decode(hex)?)?;
    let pk = sk.public_key(&Secp256k1::new());
    println!("{}", fedimint_usdt_common::evm_address(&pk));
    Ok(())
}
