# Unauthenticated write to the `meta` module's guardian submission

Found incidentally on 2026-08-31 while implementing guardian fee voting for the AMM.
**Not an experimint bug** — it is in upstream fedimint and in Fedi's fork. Recorded here
because it was found here, it affects the federation we are about to deploy, and the AMM
would have inherited it verbatim had we copied the pattern.

**Please decide on disclosure before this goes anywhere public.**

## The bug

`ApiEndpointContext` exposes two different things, and only one of them is verified:

```rust
/// Returns the auth set on the request (regardless of whether it was correct)
pub fn request_auth(&self) -> Option<ApiAuth>

/// Whether the request was authenticated as the guardian who controls this
/// fedimint server
pub fn has_auth(&self) -> bool
```
`fedimint-core/src/module/mod.rs:499-509`

The server dispatch computes the verified flag but passes the *claimed* auth through
unconditionally, and does **not** reject a bad password:

```rust
let has_auth = match (&self.auth_api, &request.auth) {
    (Some(server_auth), Some(req_auth)) => server_auth.verify(req_auth.as_str()),
    _ => false,
};
(self, ApiEndpointContext::new(db, has_auth, request.auth.clone()))
```
`fedimint-server/src/consensus/api.rs:657-665`

So reaching an endpoint proves nothing about authentication. The endpoint must consult
`has_auth()`. The `meta` module's `SUBMIT_ENDPOINT` consults `request_auth()` instead,
and only distinguishes `None` from `Some`:

```rust
match context.request_auth() {
    None => return Err(ApiError::bad_request("Missing password".to_string())),
    Some(auth) => { ... module.handle_submit_request(&mut dbtx.to_ref_nc(), &auth, &request).await?; ... }
}
```
`modules/fedimint-meta-server/src/lib.rs:436-444`

And the handler ignores the value it is handed — the parameter is `_auth: &ApiAuth`
(`:489`), underscore-prefixed and unused. Nothing downstream verifies it either.

**Net effect: any password is accepted, including the empty string.** The endpoint is
effectively unauthenticated.

## Impact

`SUBMIT_ENDPOINT` writes `MetaDesiredKey` — that guardian's *desired* metadata value.
`consensus_proposal` then proposes it, and `meta` commits a value once threshold-many
guardians have submitted byte-identical submissions.

An attacker does not need to defeat the threshold; they can simply satisfy it. Sending
the same submit request, with any password, to 5 of 7 guardians makes all 5 record the
same desired value and propose it, and it commits.

**An unauthenticated network attacker can set federation metadata.** Meta drives
client-visible configuration, so the blast radius is whatever the deployment puts in it.

The second endpoint at `:468` has the same gating; it is a read, so it leaks guardian
submissions rather than accepting writes. Lower severity, same root cause.

## Where it is present

Confirmed by reading each ref, not inferred:

| Line | Present |
| --- | --- |
| upstream fedimint `origin/master` | yes — `:433`, `:468`, `_auth` at `:486`/`:531` |
| Fedi `v0.11.0-fedi9` | yes — identical line numbers |
| experimint's pinned platform (`51d011a4`) | yes |

This is old code and the pattern is identical across all three, so it is unlikely to be
a recent regression.

**It also affects the existing live btcpp federation**, which runs a `meta` instance.

## Fix

One line at each call site: gate on `has_auth()` rather than `request_auth().is_some()`.
`fedimint_core::net::auth::check_auth` already wraps this correctly and is what the AMM's
fee-vote submit endpoint uses.

The deeper fix is to stop offering the footgun: `request_auth()` returning unverified
data with a doc comment as the only guard rail invites exactly this mistake. Either fold
verification into the accessor or make authenticated endpoints declare themselves so the
framework enforces it.

## What we did here

The AMM's `FEE_VOTE_SUBMIT_ENDPOINT` uses `check_auth` (verified `has_auth`), covered by
`submit_endpoint_requires_verified_guardian_auth` in
`modules/amm/fedimint-amm-server/tests/fee_votes.rs`. The original instruction for that
work said to follow `meta`'s pattern; that instruction was wrong, and the deviation was
correct.
