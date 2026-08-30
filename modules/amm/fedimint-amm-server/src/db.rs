//! Server-side database schema. Spec §5.

use fedimint_amm_common::pool_id::PoolId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, impl_db_lookup, impl_db_record, secp256k1};
use serde::Serialize;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Pool = 0x01,
    LpPosition = 0x02,
    Balance = 0x03,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct PoolKey(pub PoolId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct PoolPrefix;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct Pool {
    pub reserve_lo: Amount,
    pub reserve_hi: Amount,
    /// Includes the unassigned `MINIMUM_LIQUIDITY`, so this never returns
    /// to zero.
    pub total_shares: u64,
}

impl_db_record!(key = PoolKey, value = Pool, db_prefix = DbKeyPrefix::Pool);
impl_db_lookup!(key = PoolKey, query_prefix = PoolPrefix);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct LpPositionKey {
    pub pool: PoolId,
    pub owner: secp256k1::PublicKey,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct LpPositionPrefix;

// NOTE: a per-pool partial prefix (`LpPositionPoolPrefix(PoolId)`) used to
// live here, claiming "needed by both the API and any future audit path" --
// that was false (the recovery API scans raw byte ranges, and `audit`
// deliberately excludes LP positions to avoid double-counting reserves), so
// it was removed as dead code. Reintroduce it only alongside an actual
// caller.

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct LpPosition {
    pub shares: u64,
    /// Ground tweak, stored so client recovery is a table scan rather than
    /// a session-history replay (spec §8.2).
    pub tweak: [u8; 16],
}

impl_db_record!(
    key = LpPositionKey,
    value = LpPosition,
    db_prefix = DbKeyPrefix::LpPosition
);
impl_db_lookup!(key = LpPositionKey, query_prefix = LpPositionPrefix);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct BalanceKey {
    pub owner: secp256k1::PublicKey,
    pub unit: AmountUnit,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct BalancePrefix;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct BalanceEntry {
    pub amount: Amount,
    /// Ground tweak, stored so client recovery is a table scan rather than
    /// a session-history replay (spec §8.2).
    pub tweak: [u8; 16],
}

impl_db_record!(
    key = BalanceKey,
    value = BalanceEntry,
    db_prefix = DbKeyPrefix::Balance
);
impl_db_lookup!(key = BalanceKey, query_prefix = BalancePrefix);

#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::db::mem_impl::MemDatabase;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::secp256k1::{self, Keypair, SECP256K1};
    use futures::StreamExt;

    use super::*;

    fn db() -> Database {
        Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
    }

    fn pool_id(a: u64, b: u64) -> PoolId {
        PoolId::new(AmountUnit::new_custom(a), AmountUnit::new_custom(b)).expect(
            "a != b in every call site below, so PoolId::new never returns None in this test module",
        )
    }

    /// A public key derived from a repeated byte, so tests are reproducible.
    fn test_pubkey(seed: u8) -> secp256k1::PublicKey {
        Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
            .expect("a repeated non-zero byte is a valid secret key")
            .public_key()
    }

    #[tokio::test]
    async fn pool_records_round_trip() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let id = pool_id(0, 1);
        let pool = Pool {
            reserve_lo: Amount::from_msats(100),
            reserve_hi: Amount::from_msats(200),
            total_shares: 1_000,
        };
        dbtx.insert_new_entry(&PoolKey(id), &pool).await;
        assert_eq!(dbtx.get_value(&PoolKey(id)).await, Some(pool));
        dbtx.commit_tx().await;
    }

    /// All LP positions — across distinct pools and owners — must enumerate
    /// under the table-wide prefix (used by `dump_database`). The per-pool
    /// partial-prefix scan this test used to also exercise was removed with
    /// `LpPositionPoolPrefix` (dead code — see the note at its former
    /// definition site).
    #[tokio::test]
    async fn lp_positions_enumerate_under_table_prefix() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let a = pool_id(0, 1);
        let b = pool_id(0, 2);
        let k1 = test_pubkey(1);
        let k2 = test_pubkey(2);

        for (pool, key) in [(a, k1), (a, k2), (b, k1)] {
            dbtx.insert_new_entry(
                &LpPositionKey { pool, owner: key },
                &LpPosition {
                    shares: 10,
                    tweak: [0u8; 16],
                },
            )
            .await;
        }

        let all: Vec<_> = dbtx.find_by_prefix(&LpPositionPrefix).await.collect().await;
        assert_eq!(all.len(), 3);
        dbtx.commit_tx().await;
    }

    #[tokio::test]
    async fn balances_round_trip_and_delete() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let key = BalanceKey {
            owner: test_pubkey(3),
            unit: AmountUnit::new_custom(1),
        };
        dbtx.insert_new_entry(
            &key,
            &BalanceEntry {
                amount: Amount::from_msats(42),
                tweak: [1u8; 16],
            },
        )
        .await;
        assert!(dbtx.get_value(&key).await.is_some());
        dbtx.remove_entry(&key).await;
        assert!(dbtx.get_value(&key).await.is_none());
        dbtx.commit_tx().await;
    }
}
