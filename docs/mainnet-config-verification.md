# Mainnet configuration verification (USDT module)

**Verdict: MATCH.** The module's offline CREATE2 deposit-address derivation
reproduces, byte for byte, the addresses the deployed Ethereum-mainnet
`SimpleAccountFactory` reports from its own `getAddress(address,uint256)`.
Mainnet USDT's balances mapping is confirmed to live at storage slot 2, the
value the module compiles in.

Everything below was produced by
`modules/usdt/fedimint-usdt-tests/bin/verify_mainnet_config.rs`
(`cargo run -p fedimint-usdt-tests --bin verify-mainnet-config`) against
**real Ethereum mainnet** (`eth_chainId` -> `0x1`) over the public RPC
`https://ethereum-rpc.publicnode.com`, at head ~25 870 950. Every call is
read-only (`eth_call`, `eth_getCode`, `eth_getBlockByNumber`, `eth_getProof`);
no transaction was sent and no private key was involved.

The binary is deliberately not a `#[test]`: it needs the public internet and
pins mainnet-specific addresses, so it must not run in CI. It follows the
precedent of `capture-deposit-proof-fixtures`, which likewise hits mainnet.

## Why this needed proving

`fedimint_usdt_common::derive_deposit_account` computes every user's deposit
address *offline*, from a hard-coded copy of `ERC1967Proxy`'s creation
bytecode. Nothing on the deposit path cross-checks that against the chain:
client and guardians agree with each other whether or not they agree with the
factory. If the embedded bytecode -- or the ABI encoding wrapped around it --
differed by one byte from what the deployed factory actually `CREATE2`s, users
would deposit USDT to addresses no `SimpleAccount` can ever be deployed at, and
the funds would be permanently unrecoverable. The failure would be silent right
up to the first sweep attempt.

## Function selectors (recomputed, not taken on faith)

`getAddress` and `createAccount` on `SimpleAccountFactory` take the *same*
argument list, so confusing them would make the whole comparison meaningless.
Both selectors were recomputed from their signatures with `keccak256`:

| signature | selector |
| --- | --- |
| `getAddress(address,uint256)` | `0x8cb84e18` |
| `createAccount(address,uint256)` | `0x5fbfb9cf` |

Note: `0x5fbfb9cf` is **`createAccount`**, not `getAddress`. The verification
uses `0x8cb84e18`, which is also asserted to equal the `sol!`-generated
`ISimpleAccountFactory::getAddressCall::SELECTOR`.

## Verified mainnet contracts

All confirmed to carry code via `eth_getCode`:

| role | address | code size |
| --- | --- | --- |
| `usdt_contract` | `0xdAC17F958D2ee523a2206206994597C13D831ec7` | 11 075 B |
| `entry_point` (ERC-4337 v0.7) | `0x0000000071727De22E5E9d8BAf0edAc6f37da032` | 16 035 B |
| `account_factory` (v0.7) | `0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985` | 2 288 B |
| `simple_account_impl` (v0.7) | `0x68641de71cfEa5A5d0d29712449eE254bB1400C2` | 7 792 B |
| `eth_usd_price_feed` (Chainlink ETH/USD) | `0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419` | 9 571 B |

The factory/implementation/EntryPoint triple was confirmed *on chain* rather
than assumed:

```
0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985.accountImplementation() = 0x68641DE71cfEa5a5d0D29712449Ee254bb1400C2
0x68641DE71cfEa5a5d0D29712449Ee254bb1400C2.entryPoint()            = 0x0000000071727De22E5E9d8BAf0edAc6f37da032
```

So this factory is the correct one for a v0.7 module. Mainnet also carries
`SimpleAccountFactory` deployments wired to EntryPoint **v0.6** -- picking one
of those is the realistic misconfiguration, and is covered by the negative
control below.

## Derivation proof: `derive_deposit_account`

`account_factory = 0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985`,
`simple_account_impl = 0x68641de71cfEa5A5d0d29712449eE254bB1400C2`.
`salt = deposit_salt(claim_pk) = keccak256(DEPOSIT_ADDRESS_DOMAIN || claim_pk.serialize())`,
passed to the factory as `uint256(salt)` (big-endian). Owners are
`evm_address(group_public_key)`.

| # | owner | salt | module-derived | on-chain `getAddress` |
| --- | --- | --- | --- | --- |
| 0 | `0xef045a554cbb0016275e90e3002f4d21c6f263e1` | `0x9b1a6fbbbea5526d0095321ab1cc884c3e5f1e619fe014279873ca8708476551` | `0xDC1959E529BF15B0795c2E61277f0A9a8b23a4e8` | `0xDC1959E529BF15B0795c2E61277f0A9a8b23a4e8` |
| 1 | `0x19e7e376e7c213b7e7e7e46cc70a5dd086daff2a` | `0xcb449e4c3e3fa611aeddf12d4dc2a0f82e2ac7360359b634c1cd8d487fc60b2d` | `0xdAE3aCFB38a2CB8e0604B736379280D96994BCa9` | `0xdAE3aCFB38a2CB8e0604B736379280D96994BCa9` |
| 2 | `0x1c5a77d9fa7ef466951b2f01f724bca3a5820b63` | `0x2126ac4d98fc4a21e767573d94f9f37e1b060d2c98ca59b2f43c04e0f72188f1` | `0xa3446D6C8c0Ff9972Cd31BF52bc6d0421F76A892` | `0xa3446D6C8c0Ff9972Cd31BF52bc6d0421F76A892` |
| 3 | `0x03a1bba60b5aa37094cf16123add674c01589488` | `0x750e578f59763be3b29b619ed93bb0db7f6e1254e0bfff9683831b8181b0e1cc` | `0xAab824D6067C91E0E330d283798992f85a8b3Ac1` | `0xAab824D6067C91E0E330d283798992f85a8b3Ac1` |

Four distinct owners and four distinct salts, all identical. The doc comment on
`derive_deposit_account` claiming self-verification against
`SimpleAccountFactory.getAddress` is therefore correct on mainnet, not only on
the anvil-deployed factory the existing `erc4337_harness.rs` test uses.

## Derivation proof: `derive_pool_account`

The pool account uses a single fixed salt,
`pool_salt() = keccak256(POOL_ACCOUNT_DOMAIN) =
0xf0ac52fd4de16fd009cd91d542aaa1fd8761b1acd4d49505b1a4fe20f3e1251c`.

| # | owner | module-derived | on-chain `getAddress` |
| --- | --- | --- | --- |
| 0 | `0xef045a554cbb0016275e90e3002f4d21c6f263e1` | `0x504df4F511ce5EC6CfB888272B31a0E8A0946d39` | `0x504df4F511ce5EC6CfB888272B31a0E8A0946d39` |
| 1 | `0x19e7e376e7c213b7e7e7e46cc70a5dd086daff2a` | `0xc3861539b41473706a010Aea71DBc3F1f3E20763` | `0xc3861539b41473706a010Aea71DBc3F1f3E20763` |

## Negative control (the match is not vacuous)

A derivation that ignored the factory's proxy bytecode would "match" anything,
so the same derivation was pointed at an EntryPoint-**v0.6**
`SimpleAccountFactory`, which embeds a different `ERC1967Proxy`:

```
0x9406Cc6185a346906296840746125a0E44976454
  -> impl       0x8ABB13360b87Be5EEb1B98647A016adD927a136c
  -> entryPoint 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789   (v0.6, as expected)

module-derived = 0x5241cE8e4ce4acC35C979E28f6d00e74062EbD24
on-chain       = 0x03a74d8035B32c49cdCEfD59727E1954D9d86A92
```

They differ, which is the correct and expected result: the module's embedded
proxy creation code is specific to `@account-abstraction/contracts@0.7.0`. This
also makes concrete what a v0.6 misconfiguration would cost -- every deposit
address handed out would be wrong.

## The config-gen default is a *different* factory

This is the one finding an operator must act on. With only
`FM_USDT_ENTRY_POINT` set, `usdt_gen_params_from_env` does **not** default
`account_factory` to the canonical mainnet factory. It derives the module's
*own* factory (`factory_bytecode::derive_account_factory`: CREATE2 through the
Arachnid deployer over the vendored `FACTORY_CREATION_CODE`) and self-deploys
it:

```
derive_account_factory(0x0000000071727De22E5E9d8BAf0edAc6f37da032) = 0xd095bB8b86Afe336ea11D7382269e1C39037c8fb
derive_simple_account_impl(that)                                   = 0x25510b5911085689e0758109855ad14f14b8aF8b
canonical mainnet factory                                          = 0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985
```

That self-derived factory **is already deployed on mainnet** (2 288 bytes, the
same size as the canonical one -- unsurprising, since the Arachnid CREATE2
address is deterministic and anyone can deploy it), and it checks out:

```
0xd095bB8b86Afe336ea11D7382269e1C39037c8fb.accountImplementation() = 0x25510b5911085689e0758109855ad14f14b8aF8b
0x25510b5911085689e0758109855ad14f14b8aF8b.entryPoint()            = 0x0000000071727De22E5E9d8BAf0edAc6f37da032

module-derived deposit account = 0xd9a5320A4926D2E890Fc11b6e2933E88616310f7
on-chain getAddress            = 0xd9a5320A4926D2E890Fc11b6e2933E88616310f7
```

So *both* factories are safe with respect to derivation. But they are different
addresses and therefore produce different deposit accounts for the same
`(group key, claim key)`, so which one a mainnet federation runs on is a
deliberate choice that must be made before config-gen -- and, once made, is
frozen into consensus config.

## Storage-slot proof: mainnet USDT balances live at slot 2

`USDT_BALANCES_SLOT = 2` was verified end-to-end against the real contract
rather than read off a fixture. The public RPC does serve `eth_getProof` for
the USDT contract.

- block `25 870 923`, hash `0x26da4281b0adba8cb81207f381cf6f6f60bddb98edfbc0a5f6207afa7eb121bc`
- holder `0xF977814e90dA44bFA03b6295A0616a897441aceC` (Binance hot wallet)
- `balances_storage_key(holder) = keccak256(pad32(holder) || pad32(2)) =
  0x0be16d71963429204d70543701f859c43526c316ac005c10114f4694ca405f36`
- `eth_getProof` returned 8 account nodes and 8 storage nodes (an inclusion
  proof)

The proof was fed to the module's own consensus verifier,
`fedimint_usdt_server::proof::verify_deposit_proof`, which walks the trie from
the block hash down and *derives* the balance rather than trusting the RPC's
`value` field:

```
proven balance (MPT walk, slot 2)  = 17000000000000198
balanceOf() eth_call at same block = 17000000000000198
```

Exact equality on a large nonzero value is what makes this conclusive. Had USDT
kept balances at some other slot, the storage key would have been wrong, the
proof would have come back as an *exclusion* proof, and the verifier would have
returned `0` for a funded account -- a silent under-crediting bug, not an
error. The fixture's deliberate use of slot 2 to be "mainnet-USDT-faithful" is
confirmed against the real contract.

## Recommended mainnet config values

`chain_id` must be overridden: it is bound into the ERC-4337 `userOpHash` the
federation signs, so the compiled-in anvil default (31337) would make every
signature invalid on-chain.

```sh
FM_USDT_CHAIN_ID=1
FM_USDT_CONTRACT=0xdAC17F958D2ee523a2206206994597C13D831ec7
FM_USDT_ENTRY_POINT=0x0000000071727De22E5E9d8BAf0edAc6f37da032

# Choose ONE of the two factory options below.
#
# (a) Canonical, already-battle-tested mainnet SimpleAccountFactory. Must be
#     set explicitly -- it is NOT the config-gen default.
FM_USDT_ACCOUNT_FACTORY=0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985
FM_USDT_SIMPLE_ACCOUNT_IMPL=0x68641de71cfEa5A5d0d29712449eE254bB1400C2
#
# (b) Leave both unset to take the module's self-derived/self-deployed factory
#     0xd095bB8b86Afe336ea11D7382269e1C39037c8fb (impl
#     0x25510b5911085689e0758109855ad14f14b8aF8b), which is already deployed on
#     mainnet and verified above.

# Chainlink ETH/USD -- already the compiled-in default, listed for completeness.
FM_USDT_ETH_USD_PRICE_FEED=0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419

# Must be a real address on a non-dev chain: validate_usdt_params rejects the
# zero placeholder, since stranded EntryPoint gas deposits would otherwise be
# withdrawn to 0x0 and burned.
FM_USDT_RESIDUAL_RECOVERY_RECIPIENT=<federation treasury / broadcaster-refill address>

# >= MIN_PROD_CONFIRMATION_DEPTH (6) unless explicitly acknowledged; the
# compiled-in default of 1 is an anvil value and is rejected on chain id 1.
FM_USDT_CONFIRMATION_DEPTH=<>=6>
```

## What did not check out

Nothing failed. Two things worth flagging, neither a defect in the derivation:

1. **The config-gen factory default is not the canonical mainnet factory** (see
   above). Both derive correctly, but an operator who expects
   `0x91E6…8985` and sets nothing will silently get `0xd095…c8fb`.
2. **`0x5fbfb9cf` is `createAccount`, not `getAddress`.** Any future work that
   reaches for the factory by raw selector should use `0x8cb84e18` for the
   counterfactual-address query.

## Scope

This verifies address derivation and the balances storage slot. It does not
verify the `UserOp` signing/gas path, the paymaster, the broadcaster's funding
model, or Chainlink round semantics.
