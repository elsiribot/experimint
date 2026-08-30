//! Server-side database schema. Spec §5.

use fedimint_amm_common::pool_id::PoolId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, PeerId, impl_db_lookup, impl_db_record, secp256k1};
use serde::Serialize;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Pool = 0x01,
    LpPosition = 0x02,
    Balance = 0x03,
    /// Consensus state: one row per (pool, guardian) that has voted.
    FeeVote = 0x04,
    /// Local state only, never replicated: this guardian's own intent, which
    /// `consensus_proposal` turns into [`DbKeyPrefix::FeeVote`] rows.
    DesiredFee = 0x05,
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

/// One guardian's last fee vote for one pool, in per-mille.
///
/// `pool` comes FIRST and `peer` second, so that a byte prefix of
/// `(prefix_byte, pool)` selects exactly one pool's votes — the scan
/// [`FeeVotesByPoolPrefix`] performs and the aggregation depends on. The
/// reverse field order would make the per-pool scan impossible and force a
/// full-table scan filtered in memory. (`MetaSubmissionsKey` in
/// `fedimint-meta-server` is ordered `{ key, peer_id }` for exactly this
/// reason.)
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct FeeVoteKey {
    pub pool: PoolId,
    pub peer: PeerId,
}

/// Table-wide scan, used by `dump_database`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct FeeVotePrefix;

/// Every guardian's vote for one pool — the input to the aggregation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct FeeVotesByPoolPrefix(pub PoolId);

impl_db_record!(
    key = FeeVoteKey,
    value = u16,
    db_prefix = DbKeyPrefix::FeeVote
);
impl_db_lookup!(key = FeeVoteKey, query_prefix = FeeVotePrefix);
impl_db_lookup!(key = FeeVoteKey, query_prefix = FeeVotesByPoolPrefix);

/// This guardian's locally-set intent for one pool's fee, in per-mille.
///
/// Purely local: written only by the guardian-authenticated submit endpoint,
/// never by consensus, and never read by anything but
/// `ServerModule::consensus_proposal`. It is the "what I want" half of the
/// diff against [`FeeVoteKey`]'s "what I have actually had ordered", which is
/// what makes a proposal self-healing across sessions rather than a
/// fire-and-forget submission.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct DesiredFeeKey(pub PoolId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct DesiredFeePrefix;

impl_db_record!(
    key = DesiredFeeKey,
    value = u16,
    db_prefix = DbKeyPrefix::DesiredFee
);
impl_db_lookup!(key = DesiredFeeKey, query_prefix = DesiredFeePrefix);

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

    /// The whole reason `FeeVoteKey` is ordered `{ pool, peer }`: the
    /// per-pool prefix must select exactly that pool's votes. A key ordered
    /// `{ peer, pool }` would encode a byte prefix that groups by peer
    /// instead, and this test would return votes for the wrong pool.
    #[tokio::test]
    async fn fee_votes_scan_per_pool_not_across_pools() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let a = pool_id(0, 1);
        let b = pool_id(0, 2);

        for (pool, peer, fee) in [(a, 0, 5u16), (a, 1, 7), (a, 2, 9), (b, 0, 40)] {
            dbtx.insert_new_entry(
                &FeeVoteKey {
                    pool,
                    peer: PeerId::from(peer),
                },
                &fee,
            )
            .await;
        }

        let mut votes_a: Vec<u16> = dbtx
            .find_by_prefix(&FeeVotesByPoolPrefix(a))
            .await
            .map(|(_, fee)| fee)
            .collect()
            .await;
        votes_a.sort_unstable();
        assert_eq!(votes_a, vec![5, 7, 9]);

        let votes_b: Vec<u16> = dbtx
            .find_by_prefix(&FeeVotesByPoolPrefix(b))
            .await
            .map(|(_, fee)| fee)
            .collect()
            .await;
        assert_eq!(votes_b, vec![40]);

        let all: Vec<_> = dbtx.find_by_prefix(&FeeVotePrefix).await.collect().await;
        assert_eq!(all.len(), 4);

        dbtx.commit_tx().await;
    }

    #[tokio::test]
    async fn desired_fees_round_trip_and_enumerate() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let a = pool_id(0, 1);
        let b = pool_id(0, 2);

        dbtx.insert_new_entry(&DesiredFeeKey(a), &4u16).await;
        dbtx.insert_new_entry(&DesiredFeeKey(b), &6u16).await;

        assert_eq!(dbtx.get_value(&DesiredFeeKey(a)).await, Some(4));
        let all: Vec<_> = dbtx.find_by_prefix(&DesiredFeePrefix).await.collect().await;
        assert_eq!(all.len(), 2);

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
