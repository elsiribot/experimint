//! Federation API client for the AMM module's endpoints (spec §12), plus
//! generic pagination helpers over the two recovery endpoints.
//!
//! The pagination helpers are generic over the page-fetching closure rather
//! than tied to [`DynModuleApi`] directly, so pagination *consumption* —
//! "stop once `next_cursor` is `None`", "propagate a page-fetch error" — is
//! unit-testable against an in-memory fake pager, without a running
//! federation (task 11's brief: "write unit tests for everything you
//! reasonably can in-crate ... pagination consumption").

use std::future::Future;

use fedimint_amm_common::endpoints::{
    BALANCE_ENDPOINT, BALANCE_RECOVERY_ENDPOINT, BalanceRecoveryEntry, BalanceRecoveryResponse,
    BalanceRequest, LP_RECOVERY_ENDPOINT, LpRecoveryEntry, LpRecoveryResponse, POOLS_ENDPOINT,
    PoolSummary, QUOTE_ENDPOINT, QuoteRequest, QuoteResponse, RecoveryPageRequest,
};
use fedimint_api_client::api::{FederationApiExt, FederationResult, IModuleFederationApi};
use fedimint_core::module::ApiRequestErased;
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::{Amount, apply, async_trait_maybe_send};

/// Typed access to this module's federation API endpoints, mirroring
/// `fedimint-lnv2-client`'s `LightningFederationApi` (`api.rs`): a thin
/// blanket impl over [`IModuleFederationApi`] so any `DynModuleApi` (or the
/// `DynGlobalClientContext::module_api()` a state machine transition gets)
/// can call these directly.
#[apply(async_trait_maybe_send!)]
pub trait AmmFederationApi {
    async fn amm_pools(&self) -> FederationResult<Vec<PoolSummary>>;

    async fn amm_quote(&self, request: QuoteRequest) -> FederationResult<QuoteResponse>;

    /// Point lookup for a single stored `Balance` (fix pass 3, Important 5) —
    /// use this, not a recovery-page scan, whenever the exact `(pubkey,
    /// unit)` is already known. See [`BALANCE_ENDPOINT`]'s doc comment for
    /// why the scan was the wrong tool for that case.
    async fn amm_balance(&self, request: BalanceRequest) -> FederationResult<Option<Amount>>;

    async fn amm_balance_recovery_page(
        &self,
        request: RecoveryPageRequest,
    ) -> FederationResult<BalanceRecoveryResponse>;

    async fn amm_lp_recovery_page(
        &self,
        request: RecoveryPageRequest,
    ) -> FederationResult<LpRecoveryResponse>;
}

#[apply(async_trait_maybe_send!)]
impl<T: ?Sized> AmmFederationApi for T
where
    T: IModuleFederationApi + MaybeSend + MaybeSync + 'static,
{
    async fn amm_pools(&self) -> FederationResult<Vec<PoolSummary>> {
        self.request_current_consensus(POOLS_ENDPOINT.to_string(), ApiRequestErased::new(()))
            .await
    }

    async fn amm_quote(&self, request: QuoteRequest) -> FederationResult<QuoteResponse> {
        self.request_current_consensus(QUOTE_ENDPOINT.to_string(), ApiRequestErased::new(request))
            .await
    }

    async fn amm_balance(&self, request: BalanceRequest) -> FederationResult<Option<Amount>> {
        self.request_current_consensus(BALANCE_ENDPOINT.to_string(), ApiRequestErased::new(request))
            .await
    }

    async fn amm_balance_recovery_page(
        &self,
        request: RecoveryPageRequest,
    ) -> FederationResult<BalanceRecoveryResponse> {
        self.request_current_consensus(
            BALANCE_RECOVERY_ENDPOINT.to_string(),
            ApiRequestErased::new(request),
        )
        .await
    }

    async fn amm_lp_recovery_page(
        &self,
        request: RecoveryPageRequest,
    ) -> FederationResult<LpRecoveryResponse> {
        self.request_current_consensus(
            LP_RECOVERY_ENDPOINT.to_string(),
            ApiRequestErased::new(request),
        )
        .await
    }
}

/// Visits every entry across every page of `BALANCE_RECOVERY_ENDPOINT`, in
/// order, until pagination is exhausted (`next_cursor: None`).
///
/// Generic over the page fetcher's error type `E` so this is testable against
/// an in-memory fake pager with no dependency on [`fedimint_api_client`]'s
/// `FederationError`; production call sites instantiate `E =
/// fedimint_api_client::api::FederationError`.
pub async fn for_each_balance_recovery_entry<F, Fut, E>(
    mut fetch_page: F,
    mut visit: impl FnMut(&BalanceRecoveryEntry),
) -> Result<(), E>
where
    F: FnMut(RecoveryPageRequest) -> Fut,
    Fut: Future<Output = Result<BalanceRecoveryResponse, E>>,
{
    let mut cursor = None;
    loop {
        let page = fetch_page(RecoveryPageRequest {
            cursor: cursor.clone(),
            limit: None,
        })
        .await?;
        for entry in &page.entries {
            visit(entry);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
}

/// Visits every entry across every page of `LP_RECOVERY_ENDPOINT`. See
/// [`for_each_balance_recovery_entry`]; mirrored here for `LpRecoveryEntry`.
pub async fn for_each_lp_recovery_entry<F, Fut, E>(
    mut fetch_page: F,
    mut visit: impl FnMut(&LpRecoveryEntry),
) -> Result<(), E>
where
    F: FnMut(RecoveryPageRequest) -> Fut,
    Fut: Future<Output = Result<LpRecoveryResponse, E>>,
{
    let mut cursor = None;
    loop {
        let page = fetch_page(RecoveryPageRequest {
            cursor: cursor.clone(),
            limit: None,
        })
        .await?;
        for entry in &page.entries {
            visit(entry);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use fedimint_amm_common::pool_id::PoolId;
    use fedimint_core::Amount;
    use fedimint_core::module::AmountUnit;
    use fedimint_core::secp256k1::{Keypair, SECP256K1};

    use super::*;

    fn test_pubkey(seed: u8) -> fedimint_core::secp256k1::PublicKey {
        Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
            .expect("a repeated non-zero byte is a valid secret key")
            .public_key()
    }

    fn balance_entry(seed: u8, amount: u64) -> BalanceRecoveryEntry {
        BalanceRecoveryEntry {
            tweak: [seed; 16],
            pubkey: test_pubkey(seed),
            unit: AmountUnit::new_custom(0),
            amount: Amount::from_msats(amount),
        }
    }

    /// A fake pager that hands out pre-built pages one at a time and records
    /// how many times it was called, so a test can assert an early-exit
    /// helper really does stop early rather than merely returning the right
    /// answer despite over-fetching.
    struct FakePager {
        pages: RefCell<VecDeque<BalanceRecoveryResponse>>,
        calls: RefCell<u32>,
    }

    impl FakePager {
        fn new(pages: Vec<BalanceRecoveryResponse>) -> Self {
            Self {
                pages: RefCell::new(pages.into()),
                calls: RefCell::new(0),
            }
        }

        async fn fetch(
            &self,
            _req: RecoveryPageRequest,
        ) -> Result<BalanceRecoveryResponse, String> {
            *self.calls.borrow_mut() += 1;
            self.pages
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "fake pager exhausted".to_string())
        }
    }

    fn three_pages() -> Vec<BalanceRecoveryResponse> {
        vec![
            BalanceRecoveryResponse {
                entries: vec![balance_entry(1, 100), balance_entry(2, 200)],
                next_cursor: Some(vec![2]),
            },
            BalanceRecoveryResponse {
                entries: vec![balance_entry(3, 300)],
                next_cursor: Some(vec![3]),
            },
            BalanceRecoveryResponse {
                entries: vec![balance_entry(4, 400)],
                next_cursor: None,
            },
        ]
    }

    #[tokio::test]
    async fn for_each_visits_every_entry_across_every_page() {
        let pager = FakePager::new(three_pages());
        let mut seen = Vec::new();
        for_each_balance_recovery_entry(|req| pager.fetch(req), |entry| seen.push(entry.pubkey))
            .await
            .unwrap();

        assert_eq!(
            seen,
            vec![
                test_pubkey(1),
                test_pubkey(2),
                test_pubkey(3),
                test_pubkey(4),
            ]
        );
        assert_eq!(
            *pager.calls.borrow(),
            3,
            "must consume every page exactly once"
        );
    }

    #[tokio::test]
    async fn for_each_stops_at_a_single_page_with_no_next_cursor() {
        let pager = FakePager::new(vec![BalanceRecoveryResponse {
            entries: vec![balance_entry(9, 1)],
            next_cursor: None,
        }]);
        let mut count = 0;
        for_each_balance_recovery_entry(|req| pager.fetch(req), |_| count += 1)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(*pager.calls.borrow(), 1);
    }

    /// A page-fetch error must abort pagination and propagate (this coverage
    /// used to ride on the now-deleted `find_balance_recovery_entry`'s error
    /// test; the `?`-propagation it exercised is still live here).
    #[tokio::test]
    async fn for_each_propagates_a_page_fetch_error() {
        let pager = FakePager::new(vec![]);
        let result = for_each_balance_recovery_entry(|req| pager.fetch(req), |_| {}).await;
        assert!(result.is_err());
    }

    /// The cursor passed to page N+1 must be exactly the `next_cursor` page N
    /// returned — never mutated or reconstructed — since it is an opaque
    /// server-defined keyset token (spec §12: "Opaque to the client").
    #[tokio::test]
    async fn cursor_is_forwarded_verbatim_between_pages() {
        let seen_cursors = RefCell::new(Vec::new());
        let mut pages: VecDeque<BalanceRecoveryResponse> = three_pages().into();

        for_each_balance_recovery_entry(
            |req| {
                seen_cursors.borrow_mut().push(req.cursor.clone());
                let page = pages.pop_front();
                async move { page.ok_or_else(|| "exhausted".to_string()) }
            },
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(
            seen_cursors.into_inner(),
            vec![None, Some(vec![2]), Some(vec![3])]
        );
    }

    fn lp_entry(seed: u8, pool: PoolId, shares: u64) -> LpRecoveryEntry {
        LpRecoveryEntry {
            tweak: [seed; 16],
            pool,
            pubkey: test_pubkey(seed),
            shares,
        }
    }

    #[tokio::test]
    async fn lp_for_each_visits_every_entry_across_every_page() {
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        let pages = vec![
            LpRecoveryResponse {
                entries: vec![lp_entry(1, pool, 10)],
                next_cursor: Some(vec![1]),
            },
            LpRecoveryResponse {
                entries: vec![lp_entry(2, pool, 20)],
                next_cursor: None,
            },
        ];
        let pages = RefCell::new(VecDeque::from(pages));
        let mut seen = Vec::new();

        for_each_lp_recovery_entry(
            |_req| {
                let page = pages.borrow_mut().pop_front();
                async move { page.ok_or_else(|| "exhausted".to_string()) }
            },
            |entry| seen.push(entry.shares),
        )
        .await
        .unwrap();

        assert_eq!(seen, vec![10, 20]);
    }
}
