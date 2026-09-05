# Federation asset recovery tools

Operator tools for recovering the on-chain assets of an experimint USDT/AMM
federation from a **complete set of guardian data directories** (all `n`
guardians; threshold reconstruction needs `≥ threshold` of them). They exist
for the case where a federation is being decommissioned and its BTC / USDT /
ETH must be swept to fresh keys before the guardians are torn down.

These are `[[bin]]` targets of `fedimint-usdt-tests`, alongside the other
operational one-offs (`usdt-adversary`, `capture-deposit-proof-fixtures`).
They are **not** part of any CI lane.

> **Key hygiene.** No tool ever prints private key material. Reconstructed and
> generated keys are written to `0600` files; stdout carries only addresses,
> outpoints, amounts, and fully-signed transactions. Point `<out_dir>` at a
> directory outside any git tree.

## The tools

| Bin | Purpose |
| --- | --- |
| `fed-recover` | Read guardian data dirs; inventory + reconstruct keys; sweep the BTC federation wallet; small key utilities. |
| `usdt-sweep` | Move the USDT held in the module's ERC-4337 pool `SimpleAccount` to a destination, signing as the reconstructed group owner. |
| `erc20-send` | Generic ERC-20 transfer, fronting gas from a second key if the sender is unfunded. Used to deposit recovered USDT into a successor federation. |
| `btc-send` | Sign a single-UTXO P2WPKH spend (send-all-minus-fee). Used to peg recovered BTC into a successor federation. |

### `fed-recover <mode> ...`

- `inventory <guardian_dir>` — decode walletv2 consensus config, open the
  guardian DB read-only, print the federation-wallet UTXO (value, outpoint,
  tweak) and its address. (Also lists entries in the walletv2 `Output` table,
  which is a **probabilistic client-scan watch log** — NOT federation-owned
  funds; only the single `FederationWallet` record is spendable.)
- `usdt-inventory <cfg_dir> <keydir1..≥threshold>` — reconstruct the module's
  cggmp21 threshold-ECDSA group key from the guardians' key shares, verify the
  derived pubkey equals the consensus `group_public_key`, and print the pool
  `SimpleAccount` address + owner EOA. Writes the owner key `0600`.
- `gen-keys <out_dir>` — fresh BTC P2WPKH recovery key (`btc-recovery.key`).
- `gen-evm <out_dir>` — fresh EVM recovery key (`evm-recovery.key`).
- `eth-addr <key_file>` — print the EVM address for a key file.
- `sweep <cfg_dir> <dest_btc_addr> <fee_sat> <keydir1..≥threshold>` — build and
  sign the federation-wallet BTC sweep (tweaked `n`-of-`n` wsh, the daemon's
  exact `sign_tx`/`finalize_tx` recipe). Prints raw tx hex for
  `bitcoin-cli sendrawtransaction`.

### `usdt-sweep <plan|run> <cfg_dir> <owner_key> <gas_key> <dest_evm> <rpc>`

`plan` is read-only (balances, deployment status, gas). `run` funds the owner
EOA from `gas_key` if needed, deploys the counterfactual pool `SimpleAccount`
via the factory, calls `execute(usdt, 0, transfer(dest, balance))` as the
owner, and returns the owner's leftover ETH to `gas_key` — it never drains the
gas key (which, for experimint, is the broadcaster EOA a successor federation
reuses).

## The flow (as run 2026-09-04)

1. Stop the old guardians; copy every `/var/lib/private/fedimintd-<inst>` off
   the hosts.
2. `fed-recover inventory` + `sweep` → BTC to a `gen-keys` address.
3. `fed-recover usdt-inventory` → reconstruct + verify the group key;
   `usdt-sweep run` → USDT to a `gen-evm` address, gas from the broadcaster.
4. To fund a successor federation: `btc-send` the recovered BTC to a walletv2
   peg-in address, and `erc20-send` the recovered USDT to a usdt deposit
   address, then submit the deposit proof.
