//! One-off: spend a single P2WPKH UTXO to a destination address (send-all
//! minus fee), signed with a hex private key read from a file. Prints the raw
//! signed tx hex for `bitcoin-cli sendrawtransaction`. Key never printed.
//!
//! args: btc-send <key_file> <prev_txid> <prev_vout> <prev_value_sat> <dest> <fee_sat>

use std::str::FromStr;

use anyhow::{Context, ensure};
use bitcoin::hashes::Hash as _;
use bitcoin::secp256k1::{Message, Secp256k1};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{
    Address, Amount, CompressedPublicKey, Network, OutPoint, PrivateKey, Sequence, Transaction,
    TxIn, TxOut, Witness,
};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    ensure!(a.len() == 7, "btc-send <key_file> <txid> <vout> <value_sat> <dest> <fee_sat>");
    let secp = Secp256k1::new();

    let hexk = std::fs::read_to_string(&a[1])?.trim().trim_start_matches("0x").to_string();
    let sk = bitcoin::secp256k1::SecretKey::from_slice(&hex::decode(hexk)?)?;
    let privkey = PrivateKey::new(sk, Network::Bitcoin);
    let pk = CompressedPublicKey::from_private_key(&secp, &privkey)?;
    let from_addr = Address::p2wpkh(&pk, Network::Bitcoin);

    let prev_value = Amount::from_sat(a[4].parse::<u64>()?);
    let fee = Amount::from_sat(a[6].parse::<u64>()?);
    let send = prev_value.checked_sub(fee).context("fee exceeds value")?;
    let dest = Address::from_str(&a[5])?.require_network(Network::Bitcoin)?;
    let outpoint = OutPoint {
        txid: bitcoin::Txid::from_str(&a[2])?,
        vout: a[3].parse()?,
    };

    let mut tx = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: outpoint,
            script_sig: Default::default(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut { value: send, script_pubkey: dest.script_pubkey() }],
    };

    // rust-bitcoin derives the BIP143 scriptCode internally; pass the p2wpkh
    // script_pubkey (OP_0 <20-byte keyhash>), not the derived p2pkh code.
    let spk = from_addr.script_pubkey();
    let sighash = SighashCache::new(&tx)
        .p2wpkh_signature_hash(0, &spk, prev_value, EcdsaSighashType::All)?;
    let sig = secp.sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), &sk);
    let mut witness = Witness::new();
    let mut sig_ser = sig.serialize_der().to_vec();
    sig_ser.push(EcdsaSighashType::All as u8);
    witness.push(sig_ser);
    witness.push(pk.to_bytes());
    tx.input[0].witness = witness;

    eprintln!("from:  {from_addr}");
    eprintln!("spend: {} -> {} ({} to {}, {} fee)", outpoint, dest, send, dest, fee);
    eprintln!("txid:  {}", tx.compute_txid());
    println!("{}", bitcoin::consensus::encode::serialize_hex(&tx));
    Ok(())
}
