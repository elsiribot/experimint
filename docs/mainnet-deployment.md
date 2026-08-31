# Deploying the experimint federation on Ethereum mainnet

Seven guardians running `fedimintd-experimint` against Bitcoin mainnet and
Ethereum mainnet, as a second `fedimintd` instance alongside the live
ten-guardian btcpp federation those hosts already run.

Everything below assumes you were not in the session that wrote it. Where a
value has to be chosen by a human it says so; nothing here invents one.

## The topology

`--module` flag order fixes the instance ids, so this list *is* the topology:

| id | kind | params | purpose |
| --- | --- | --- | --- |
| 0 | `walletv2` | none | on-chain BTC peg-in/peg-out |
| 1 | `mintv2` | `{"amount_unit":0}` | BTC ecash |
| 2 | `mintv2` | `{"amount_unit":1}` | USDT ecash |
| 3 | `lnv2` | none | Lightning |
| 4 | `usdt` | full `UsdtGenParams` | USDT-on-EVM peg, issues unit 1 |
| 5 | `amm` | none | constant-product market between units 0 and 1 |
| 6 | `meta` | none | guardian-published metadata |

Seven guardians: fedimint's threshold for `n = 7` is exactly 5, so "5-of-7"
needs no configuration. Two guardians may be offline without stopping
consensus; three may not.

Hosts (from `elsirion-infa`, branch `es/usdt-amm-federation`). **Seven guardians
on six hosts** — btcpp-01 carries two instances:

| guardian | host | instance | API | chain backend |
| --- | --- | --- | --- | --- |
| 1 | `btcpp-01.cypheru.net` | `usdt` | `/ws/` | local pruned `bitcoind` |
| 2 | `btcpp-01.cypheru.net` | `usdt2` | `/ws2/` | same `bitcoind` |
| 3 | `btcpp-03.cypheru.net` | `usdt` | `/ws/` | local pruned `bitcoind` |
| 4 | `btcpp-05.cypheru.net` | `usdt` | `/ws/` | local pruned `bitcoind` |
| 5 | `btcpp-06.cypheru.net` | `usdt` | `/ws/` | local pruned `bitcoind` |
| 6 | `btcpp-10.cypheru.net` | `usdt` | `/ws/` | local pruned `bitcoind` |
| 7 | `btc-2.sirion.io` | `usdt` | `/ws/` | nix-bitcoin full node |

`btcpp-08` was a guardian and is not any more: it is wedged — answers ICMP and
completes a TCP handshake on 22 and 443, but sshd never sends a banner and nginx
never replies. Alive at the kernel, userspace hung, almost certainly out of
memory. There is no power-cycle path (the mynymbox panel is billing-only). DKG
fixes membership at ceremony time, so a host that *might* come back is no use.

Doubling btcpp-01 makes it a correlated failure for two of seven. Threshold is
5, so losing that host alone leaves exactly 5 — consensus survives with zero
margin, and any second failure halts it. See the trade-off block in
`usdt-federation-params.nix`.

`btcpp-11` and the iroh hosts are deliberately *not* guardians: 1 core and 1 GB
of RAM each, and cggmp21 threshold-ECDSA DKG is CPU- and memory-heavy.

Measured on btcpp-01 after deploy, both instances idle pre-DKG: **65 MB and
59 MB** peak. The 3.3 GB figure quoted earlier came from the *old* daemon
crash-looping against a 572 MB database, and does not describe a fresh guardian.
A 4 GiB swapfile is configured on btcpp-01 as a cushion for the DKG spike.

## Prerequisites — all resolved as of 2026-08-31

Kept as a checklist because each one fails in a different and non-obvious way if
it regresses.

1. **`FM_USDT_BROADCASTER_PRIVATE_KEY_FILE` — a funded Ethereum EOA.** Done.
   `secrets/usdt-broadcaster-key.age` holds a real key whose address is
   **`0x7e91F93F1Bfc42EfF669BFCdDe5aC4013255c9C7`**, verified by re-deriving the
   address from the stored key before it was funded. One key is shared across
   all seven — the EntryPoint dedups by `(sender, nonce)`, so it does not matter
   which guardian submits.

   Balance at deployment: **0.050235 ETH**. At ~0.56 gwei that is roughly 220
   deploy-and-sweep operations. `broadcaster_min_balance_wei` was lowered from
   the module's 0.05 ETH default to **0.01 ETH**, because 0.050235 is only
   0.000235 above the default — the first `handleOps` would have dropped below
   it and flagged the module `Degraded` with ~220 operations of gas still in
   hand. The gate sets where "funded" begins; it does not affect gas cost.

2. **`FM_USDT_RESIDUAL_RECOVERY_RECIPIENT` — a treasury EVM address.** Done, set
   to the broadcaster address above, so recovered EntryPoint gas returns to the
   wallet that fronts gas. Consensus-agreed: every guardian builds the
   byte-identical `EntryPoint.withdrawTo(recipient, amount)`, so all seven must
   carry the same value. The module rejects the zero address on a non-dev chain,
   so config generation stops rather than burning recovered deposits.

3. **`FM_PASSWORD_API` — the guardian API password.** Done, a random 32-char
   value in `secrets/usdt-config-gen-env.age`. It gates every admin RPC
   *including the whole setup ceremony below*, and unlike the UI password it
   never falls back to a file on disk: while unset, admin RPCs return 401
   unconditionally. The built-in UI on `127.0.0.1:8185` is a separate plane.

4. **Secret recipients.** Done. btcpp-05 and btcpp-08 had no host key in
   `secrets.nix` and were recipients of nothing. Both are recorded now and every
   secret has been rekeyed, so `bitcoind-rpc-password.age` reaches them too.

   btcpp-08's key could not be obtained with `ssh-keyscan -t ed25519` — its sshd
   truncates a keyscan handshake and returns only its RSA key, which is why it
   was previously recorded as uncollectable. It was read from the host's own
   `/etc/ssh/ssh_host_ed25519_key.pub` instead.

5. **A bundler-capable EVM RPC.** Done — Infura, with the key appended as the
   final path segment via `FM_USDT_EVM_RPC_API_KEY`, never in the URL.

   This one is easy to get wrong and fails late. The module reads UserOp
   receipts with `eth_getUserOperationReceipt`, an ERC-4337 *bundler* method. A
   plain archive node (the previous default, `ethereum-rpc.publicnode.com`)
   answers `-32601 Method not found`, so `handleOps` lands but the receipt
   lookup never resolves and **withdrawals never confirm** — the submitter
   RBF-reprices forever while the pool balance stays 0. Deposits are unaffected,
   which is what makes it easy to miss. Verify a candidate endpoint with:

   ```bash
   curl -s -X POST "$RPC" -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"eth_getUserOperationReceipt","params":["0x00…01"]}'
   ```
   A bundler-capable provider returns `"result": null` for an unknown hash. A
   plain node returns an error object.

## 1. Build the daemon

```bash
nix build .#fedimintd-experimint
./result/bin/fedimintd-experimint --version
```

The infra flake pulls the same package as an input, so `just build
fedimint-btcpp-01` in `elsirion-infa` builds it too.

## 2. Environment variables

`hosts/modules/usdt-federation.nix` in `elsirion-infa` sets all of these; the
table is here so the deployed unit can be checked against something.

### Set by the `services.fedimintd` NixOS module

| variable | value | note |
| --- | --- | --- |
| `FM_DATA_DIR` | `/var/lib/fedimintd-usdt/` | distinct from the btcpp instance's |
| `FM_BIND_P2P` | `0.0.0.0:8183` | |
| `FM_BIND_UI` | `127.0.0.1:8185` | |
| `FM_P2P_URL` | `fedimint://<fqdn>:8183` | |
| `FM_API_URL` | `wss://<fqdn>/usdt/ws/` | goes into the invite code |
| `FM_BITCOIN_NETWORK` | `bitcoin` | |
| `FM_BITCOIND_URL` | `http://127.0.0.1:8332` | btcpp hosts only |
| `FM_BITCOIND_USERNAME` | `bitcoin` | btcpp hosts only |
| `FM_BITCOIND_URL_PASSWORD_FILE` | `/run/agenix/bitcoind-rpc-password` | btcpp hosts only |
| `FM_ESPLORA_URL` | `https://mempool.space/api` | `fedimint-testing` only |
| `FM_BIND_API_WS`, `FM_BIND_API_IROH` | | **ignored by this binary** — see below |

The NixOS module comes from the upstream `fedimint` flake input, which is a
different API generation from the fork this daemon is built against. Upstream
splits the API bind into `FM_BIND_API_WS`/`FM_BIND_API_IROH`; the fork has a
single `FM_BIND_API` and switches iroh on with `FM_ENABLE_IROH`. Left to
itself the daemon would fall back to its own default of `0.0.0.0:8174` —
public, and the port the live btcpp guardian already holds.

### Set explicitly to work around that, or because they are not module options

| variable | value | why |
| --- | --- | --- |
| `FM_BIND_API` | `127.0.0.1:8184` | the module's `FM_BIND_API_WS` does nothing here |
| `FM_BIND_METRICS` | `127.0.0.1:8186` | daemon binds it unconditionally and a failed bind is fatal; 8176 is the btcpp instance's |

### Module selection

| variable | value | effect |
| --- | --- | --- |
| `FM_ENABLE_MODULE_WALLETV2` | `1` | pre-ticks it in the setup UI |
| `FM_ENABLE_MODULE_MINTV2` | `1` | " |
| `FM_ENABLE_MODULE_USDT` | `1` | " |

These only decide what the setup UI pre-selects. Every module in the binary is
available either way, and the `--module` CLI path ignores them entirely.

### USDT — per guardian, runtime

| variable | value |
| --- | --- |
| `FM_USDT_EVM_RPC_URL` | `https://ethereum-rpc.publicnode.com` (see the warning above) |
| `FM_USDT_BROADCASTER_PRIVATE_KEY_FILE` | `/run/agenix/usdt-broadcaster-key` |
| `FM_USDT_EVM_RPC_API_KEY_FILE` | not set; add it with the provider key |

### USDT — config generation

Consumed only by the config-gen leader, and only on the setup-UI path (the
`--module` CLI path passes params explicitly instead). Set on every guardian so
that whichever one leads produces the same consensus config.

| variable | value | source |
| --- | --- | --- |
| `FM_USDT_CHAIN_ID` | `1` | Ethereum mainnet |
| `FM_USDT_CONTRACT` | `0xdAC17F958D2ee523a2206206994597C13D831ec7` | Tether USD |
| `FM_USDT_ENTRY_POINT` | `0x0000000071727De22E5E9d8BAf0edAc6f37da032` | ERC-4337 v0.7 |
| `FM_USDT_ACCOUNT_FACTORY` | `0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985` | `accountImplementation()` returns the impl below |
| `FM_USDT_SIMPLE_ACCOUNT_IMPL` | `0x68641de71cfea5a5d0d29712449ee254bb1400c2` | its `entryPoint()` returns the EntryPoint above |
| `FM_USDT_CONFIRMATION_DEPTH` | `12` | the module enforces a minimum of 6 on non-dev chains |
| `FM_USDT_RESIDUAL_RECOVERY_RECIPIENT` | **unset** | from the agenix EnvironmentFile |

`FM_USDT_ETH_USD_PRICE_FEED` is deliberately absent: the module's compiled-in
default is already the canonical mainnet Chainlink ETH/USD aggregator
`0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419`. Same for
`broadcaster_min_balance_wei` (0.05 ETH) and `price_feed_max_staleness_secs`
(14400).

`account_factory` and `simple_account_impl` are normally *derived* from
`entry_point` — `CREATE2(ArachnidDeployer, salt, FACTORY_CREATION_CODE ‖
abi.encode(entry_point))` and `CREATE(factory, 1)` — and the env overrides are
an escape hatch for a pre-deployed stack. They are pinned here because they
were checked against the real chain. If the vendored creation code and the real
factory ever disagree, an explicit override hides the mismatch until the
on-chain `getAddress`-equivalence readiness gate catches it, which is after DKG
has baked the address into consensus config.

The single source of truth for all of these is
`hosts/modules/usdt-federation-params.nix` in `elsirion-infa`. Change them
there, nowhere else.

## 3. Provision the secrets

In `elsirion-infa`, on the `es/usdt-amm-federation` branch:

```bash
nix develop                      # brings in agenix and just

# 1. Close the recipient gap (see blocker 4).
ssh-keyscan -t ed25519 btcpp-05.cypheru.net btcpp-08.cypheru.net
$EDITOR secrets.nix              # add the two keys, uncomment the two entries
just agenix-rekey

# 2. The broadcaster key. BARE hex, 0x-prefixed or not, no trailing newline --
#    the module reads the file's trimmed contents, not KEY=value.
just agenix-edit secrets/usdt-broadcaster-key.age

# 3. The EnvironmentFile: uncomment FM_PASSWORD_API and give it a value,
#    replace the zero address with the treasury address.
just agenix-edit secrets/usdt-config-gen-env.age
```

Then fund the broadcaster EOA with at least 0.05 ETH (`broadcaster_min_balance_wei`).
If one key is shared across guardians, one account is enough; if each guardian
gets its own, each needs the balance, and at least five must clear it.

Both secrets are delivered to a `DynamicUser` unit, so they are owned by the
static group `fedimintd-usdt-secrets` at mode `0440` and the unit carries that
group in `SupplementaryGroups`. The private key reaches the process as a *path*
(`FM_USDT_BROADCASTER_PRIVATE_KEY_FILE`), not as a value, so it never enters
the process environment.

## 4. Deploy

```bash
just build fedimint-btcpp-01     # and each of the others
just apply fedimint-btcpp-01 root@btcpp-01.cypheru.net
just apply fedimint-btcpp-03 root@btcpp-03.cypheru.net
just apply fedimint-btcpp-05 root@btcpp-05.cypheru.net
just apply fedimint-btcpp-06 root@btcpp-06.cypheru.net
just apply fedimint-btcpp-08 root@btcpp-08.cypheru.net
just apply fedimint-btcpp-10 root@btcpp-10.cypheru.net
just apply fedimint-testing  root@testing.sirion.io
```

`just apply-fedimint` is the wrong recipe here — it is stale and it touches
hosts that are not guardians of this federation.

**`fedimint-testing` will not evaluate on a machine that does not have the
`elsirion-infa` flake's local `path:`/`git+file:` inputs checked out.** On the
box this was written on, `nixosConfigurations.fedimint-testing` fails with
`path '//home/user/projects/elsirion/lnurlw-server' does not exist`, and
`nix flake check` fails the same way on `multipay` — both on `master`, before
any of this work. Nothing here made it worse, but the seventh guardian cannot
be built or deployed until those inputs are present. The other six are
unaffected.

After each host, confirm the live federation did not move:

```bash
systemctl status fedimintd-btcpp   # unchanged, still running, no restart
systemctl status fedimintd-usdt    # new, running
ss -ltnp | grep -E '817[3-6]|818[3-6]'
```

Expect `8173/8174/8175/8176` on the btcpp instance and `8183/8184/8185/8186` on
the new one. `/var/lib/fedimintd-btcpp` must be untouched;
`/var/lib/fedimintd-usdt` is created empty.

## 5. The DKG ceremony

Each guardian's setup API is at `wss://<fqdn><apiPath>`. Six hosts, seven
endpoints — btcpp-01 answers on both `/ws/` and `/ws2/`, and those are two
*different* guardians that must be treated as strangers throughout. Export the
password once:

```bash
export FM_PASSWORD_API='<the value in usdt-config-gen-env>'
E01=wss://btcpp-01.cypheru.net/ws/
E01B=wss://btcpp-01.cypheru.net/ws2/
E03=wss://btcpp-03.cypheru.net/ws/
E05=wss://btcpp-05.cypheru.net/ws/
E06=wss://btcpp-06.cypheru.net/ws/
E10=wss://btcpp-10.cypheru.net/ws/
EB2=wss://btc-2.sirion.io/ws/
```

The authoritative list is rendered onto every guardian at
`/etc/fedimintd-usdt/peers.json` — read it rather than retyping, because the
doubled host is exactly the entry that gets collapsed into one by hand and
leaves the federation a peer short.

`fedimint-cli` here is a stock one built from the same fork revision as the
daemon (`51d011a47769c91aabe2ed6f1f62e91e53c50283`). It can drive setup and the
generic admin API; it cannot drive the `amm`/`usdt` *client* modules, because
no client binary registers them yet.

### 5.1 Build the leader's usdt params file

The `--module` CLI hands its params to the module **verbatim**, and
`UsdtGenParams` has no serde defaults, so a partial `usdt={"chain_id":1}` fails
config generation with `missing field usdt_contract`. The file has to be
complete. The deployed host already carries all of it except the recipient:

Do the substitution on your own machine — the hosts carry neither `jq` nor
`websocat`, and `machine_defaults.nix` is not the place to add them for a
one-off ceremony:

```bash
ssh root@btcpp-01.cypheru.net cat /etc/fedimintd-usdt/config-gen-params.json > params.tmpl
RECIPIENT="$(ssh root@btcpp-01.cypheru.net \
  "sed -n 's/^FM_USDT_RESIDUAL_RECOVERY_RECIPIENT=//p' /run/agenix/usdt-config-gen-env")"

jq --arg r "$RECIPIENT" '.residual_recovery_recipient = $r' params.tmpl > usdt-params.json
jq . usdt-params.json    # sanity check: 10 fields, no zero addresses
```

It should look like this:

```json
{
  "usdt_contract": "0xdAC17F958D2ee523a2206206994597C13D831ec7",
  "chain_id": 1,
  "confirmation_depth": 12,
  "entry_point": "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
  "account_factory": "0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985",
  "simple_account_impl": "0x68641de71cfea5a5d0d29712449ee254bb1400c2",
  "broadcaster_min_balance_wei": 10000000000000000,
  "eth_usd_price_feed": "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419",
  "price_feed_max_staleness_secs": 14400,
  "residual_recovery_recipient": "0x<treasury>"
}
```

(The alternative is the built-in setup UI over `ssh -L 8185:127.0.0.1:8185`.
There, leaving a module row's params box empty fills it from
`default_config_gen_params` — which *is* built from the `FM_USDT_*` environment
variables — so the UI path needs no params file. Pick one path and use it for
the whole ceremony.)

### 5.2 Local params

Every guardian sets its own local params, and exactly one of them — the leader —
additionally carries the federation name, the size and the module list. Each
call prints that guardian's **setup code** as a JSON string; capture all seven.

The name is what shows up in the guardian roster, so give each one its host.

```bash
declare -A GUARDIANS=(
  [btcpp-01]="$E01" [btcpp-01b]="$E01B" [btcpp-03]="$E03" [btcpp-05]="$E05"
  [btcpp-06]="$E06" [btcpp-10]="$E10" [btc-2]="$EB2"
)
declare -A CODES

# The leader. btcpp-01 here; any one of the seven will do.
CODES[btcpp-01]="$(fedimint-cli admin setup "$E01" set-local-params btcpp-01 \
    --federation-name 'experimint USDT/AMM' \
    --federation-size 7 \
    --module walletv2 \
    --module 'mintv2={"amount_unit":0}' \
    --module 'mintv2={"amount_unit":1}' \
    --module lnv2 \
    --module 'usdt=@./usdt-params.json' \
    --module amm \
    --module meta | jq -r .)"

# The six followers: a name and nothing else. `btcpp-01b` is the second
# instance on the leader's own host and is a full peer like any other.
for name in btcpp-01b btcpp-03 btcpp-05 btcpp-06 btcpp-10 btc-2; do
  CODES[$name]="$(fedimint-cli admin setup "${GUARDIANS[$name]}" \
      set-local-params "$name" | jq -r .)"
done

[ "${#CODES[@]}" -eq 7 ] || { echo "expected 7 setup codes, got ${#CODES[@]}"; exit 1; }
```

Give the two btcpp-01 guardians distinct names. They appear side by side in
every roster, and `btcpp-01` / `btcpp-01b` is the difference between a legible
federation and one where nobody can tell which of two identical rows just
stopped voting.

`set-local-params` called a second time with *identical* arguments returns the
same setup code rather than erroring, so re-running a command verbatim is a safe
way to re-read a code you lost. Called with different arguments it fails with
`Local parameters have already been set` — in particular, re-running the leader
without its `--module` flags is not a way to re-read its code.

### 5.3 Exchange setup codes

Every guardian needs the other six. Peer ids are assigned by sorting the set of
setup codes, not by who led, so all seven must end up with an identical set or
they will disagree about who is peer 0.

```bash
for target in "${!GUARDIANS[@]}"; do
  for src in "${!CODES[@]}"; do
    [ "$target" = "$src" ] && continue      # own code is refused
    fedimint-cli admin setup "${GUARDIANS[$target]}" add-peer "${CODES[$src]}"
  done
done
```

`add-peer` prints the added guardian's name, so expect six lines per target and
42 in total. `admin setup <endpoint> status` will *not* confirm this — it only
reports `AwaitingLocalParams` / `SharingConnectionCodes` / `ConsensusIsRunning`,
with no peer count. The guardian dashboard over
`ssh -L 8185:127.0.0.1:8185 <host>` shows the roster if you want to see it.

### 5.4 Start DKG

On every guardian, in any order. The call returns as soon as that guardian has
handed its config-gen params to the daemon — it does not wait for DKG — so a
serial loop is fine:

```bash
for e in "$E01" "$E01B" "$E03" "$E05" "$E06" "$E10" "$EB2"; do
  fedimint-cli admin setup "$e" start-dkg
done
```

DKG itself only proceeds once all seven have started; a guardian that started
early sits in `Still waiting for peer message` until the last one joins.

Watch it:

```bash
ssh root@btcpp-01.cypheru.net journalctl -u fedimintd-usdt -f
```

Expect, in order: `Running config generation...`, one
`Running config generation for module of kind <kind>...` per instance, then
`Comparing consensus config checksum <hash>...`. Every guardian must print the
**same checksum**; a mismatch means the leader's params did not reach everyone
identically.

The `usdt` instance's DKG is the slow one — cggmp21 threshold-ECDSA keygen
across seven peers. Expect minutes, not seconds, and expect CPU load. If a
guardian dies here, DKG has to be restarted from scratch on all seven, because
a partial key share is useless. `fedimint-cli` has no subcommand for that — the
`reset_peer_setup_codes` endpoint exists on the setup API but is only reachable
from the built-in UI, or by calling the socket by hand as in section 6.

Failures worth recognising:

- `Failed to parse module params` on the `usdt` instance — the params file was
  partial. Section 5.1.
- `residual_recovery_recipient must not be the placeholder zero address` — the
  `jq` substitution did not happen, or the secret still holds the zero address.
- `confirmation_depth (N) is below the minimum safe depth (6)` — someone
  lowered it; do not set `FM_USDT_UNSAFE_LOW_CONFIRMATION_DEPTH` on mainnet.
- `Module <kind> is denominated in unit 1, which no enabled module backs` — the
  `usdt` instance is missing from the `--module` list.

## 6. Verify the federation reached `BootstrapState::Ready`

DKG finishing means consensus is running. It does **not** mean the usdt module
is usable: the module has a second, on-chain readiness gate (`Part C`), and it
blocks deposit-address handout until every condition holds.

The `usdt_status` endpoint reports it. Instance id 4, so the JSON-RPC method
name on the websocket API is `module_4_usdt_status`. `fedimint-cli dev api
--module 4 usdt_status` will *not* work — the module selector resolves against
the *client*, which has no usdt client module — so call the socket directly.
The public vhost path forwards it, so this runs from your own machine:

```bash
nix run nixpkgs#websocat -- -1 "$E01" <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"module_4_usdt_status","params":[{"auth":null,"params":null}]}
JSON
```

The response is a `StatusResponse`:

```json
{
  "state": "Ready",
  "entry_point_ok": true,
  "factory_ok": true,
  "impl_ok": true,
  "funded_guardians": 7,
  "healthy_guardians": 7,
  "threshold": 5
}
```

Read it as follows.

- `state: "AwaitingInfra"` — never been ready. Look at the other fields for why.
- `state: "Ready"` — the full deposit → claim → sweep → withdraw lifecycle is
  operational.
- `state: "Degraded"` — it was ready and something regressed, most often a
  broadcaster's ETH running low. Advisory; distinguished from `AwaitingInfra`
  only by a persisted latch.
- `entry_point_ok` / `factory_ok` / `impl_ok` are federation facts, each true
  once at least `threshold` guardians have voted it. They cover the on-chain
  EntryPoint, the self-deployed account factory, and the equivalence between
  the factory's `getAddress` and the module's own derivation. If `factory_ok`
  stays false, the vendored factory creation code and the on-chain factory
  disagree — the fail-safe that stops unspendable deposit addresses being
  handed out.
- `funded_guardians` is the count reporting a broadcaster at or above
  `broadcaster_min_balance_wei`. **This is the one that stays at 0 until the
  broadcaster key is filled in and funded.** It must reach `threshold`.
- `healthy_guardians` is the count with a working EVM RPC. Below `threshold`,
  suspect the RPC endpoint or a provider rate limit.

Because the state is read out of consensus DB, any guardian answers
identically; asking a second one is a useful check that consensus is actually
agreeing.

Three more endpoints worth knowing during bring-up, same call shape (the last
is a core endpoint, so no `module_N_` prefix):

- `module_4_pool_state` — the pool account, its USDT balance, accrued fees.
- `module_4_latest_anchored_block` — how far the federation's agreed view of
  the chain has advanced.
- `invite_code` — what clients join with. Also shown by the guardian dashboard
  over `ssh -L 8185:127.0.0.1:8185`.

Join with it and confirm the client sees seven module instances in the order at
the top of this document. A stock `fedimint-cli` will list the `amm` and `usdt`
instances as `UnsupportedByClient`; that is expected, since no client binary
registers those two module implementations yet.
