# HYMET distributor program

`programs/hymet-rewards-distributor` (mirrored from [Hymetdotfun/hymet](https://github.com/Hymetdotfun/hymet)) · Anchor 0.31.1 · Mainnet program
[`9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px`](https://solscan.io/account/9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px)
· deployed binary SHA-256 `fda10de4da728442da4a42a63bb1f5d8df614e49f8421c656ee2ca67f3be3e7b` ([verify](VERIFY-BUILD.md))

The distributor receives a launch's Meteora trading fees in HYPE and splits them by fixed rules: **20% to HYMET**, then
the remaining 80% between the creator and the token's holders in the proportion chosen at launch. It has no
general-purpose withdrawal path: funds can only leave as the platform share, a creator claim, or holder payouts.

## Accounts

| Account | Seeds | Contents |
|---|---|---|
| `DistributorConfig` | `["config"]` | Admin, platform fee wallet, snapshot (keeper) authority. Created once by `initialize` |
| `FeePool` (one per token) | `["fee-pool", mint]` | Creator, mint, DBC pool, registered DAMM v2 pool, creator share of the 80% (bps), creator claimable, holder unallocated, holder reserved, next epoch, totals, last snapshot slot |
| Base and HYPE vaults | ATAs of the fee pool | Token custody; the fee pool PDA is the only authority |
| `RewardEpoch` | `["epoch", fee_pool, epoch_number]` | Merkle root of one payout round, funded amount, paid amount, chunk count and a paid-chunk bitmap |

The fee pool PDA is the DBC config's fee claimer and the owner of the partner DAMM v2 LP position (100% of the partner
liquidity, permanently locked).

## Instructions

| Instruction | Signer | What it does |
|---|---|---|
| `initialize` | Program upgrade authority | Creates the config with the platform wallet and snapshot authority. One time only; the upgrade-authority check prevents front-running between deploy and initialize |
| `set_snapshot_authority` | Program upgrade authority | Rotates the keeper key (must differ from the current key, the fee wallet and the upgrade authority) |
| `create_fee_pool` | Creator | Creates the fee pool and vaults for a new mint, binding the creator share (0–10,000 bps of the 80%). Created atomically with the token, so it cannot be front-run |
| `register_damm_pool` | Creator or keeper | Records the migrated DAMM v2 pool. Only the canonical pool (the PDA Meteora's migration derives under config `A8gM…`, which only the DBC pool authority can create) is accepted, once |
| `settle_dbc` | Anyone | Claims the pool's DBC partner fees into the vault and splits them |
| `settle_damm_v2` | Anyone | Claims the fee pool's DAMM v2 position fees into the vault and splits them |
| `fund_holder_epoch` | Keeper | Moves already-collected holder HYPE into a payout round committed by a Merkle root over payout chunks |
| `distribute_holder_chunk` | Anyone | Pays one chunk (up to 6 holders) of a funded round after verifying its Merkle proof |
| `close_holder_epoch` | Keeper | Closes a fully paid round, or an expired one (1,512,000 slots: about 7 days at 400 ms slots, sooner when slots run faster); any unpaid amount returns to the **holder** pool |
| `claim_creator` | Creator | Sends the creator's accumulated share to the creator's HYPE account |

## Fee math

For each settlement the program measures `received`, the increase of the HYPE vault across the Meteora claim, and
splits it in integer arithmetic:

```text
platform = received × 2,000 / 10,000          (rounded down; sent to the platform wallet at once)
creator  = (received − platform) × creator_bps / 10,000   (rounded down; claimable by the creator)
holders  = received − platform − creator       (added to the holder pool)
```

Every base unit is accounted for, and after every transfer out the program checks
`vault ≥ creator_claimable + holder_unallocated + holder_reserved`. A settlement that would return the launched
token instead of HYPE is rejected and rolled back; HYMET launches only collect fees in HYPE.

## Holder payouts

The program enforces the funded amount, the Merkle proof, one payment per chunk, the canonical HYPE account and a
non-zero balance and reward in every entry. The snapshot, payout minimums, carry-over and the 15-minute interval are
keeper rules, not program rules.

1. The keeper settles fees, then snapshots every token account of the mint at one slot. It excludes empty and frozen
   accounts, accounts owned by program-derived addresses (pool vaults), the incinerator and the HYMET platform wallet.
2. It computes each holder's pro-rata share of the holder pool. Amounts below the minimum (0.01 HYPE, or 0.05 HYPE when
   the keeper must create the holder's HYPE account) carry over to the next round. Carry is an off-chain keeper ledger;
   on-chain the amounts remain in the holder pool. It stays attributed to the wallet after it sells, is never shared with
   other holders and leaves the ledger only as a payment to that wallet.
3. It commits a Merkle root over chunks of up to 6 holders with `fund_holder_epoch`. Each leaf binds the mint, epoch,
   chunk index, sorted wallets, token balances and rewards.
4. It pushes every chunk with `distribute_holder_chunk`. Holders never sign or claim. Each chunk can be paid once, only
   to the wallet's canonical HYPE account, and never beyond the funded amount.
5. It closes the round. The keeper runs rounds at most once every 15 real minutes per token, measured from block timestamps. The program sets no minimum interval.

## Security properties

- **Pinned external programs.** CPIs go only to Meteora DBC and DAMM v2 at fixed program IDs, with fixed instruction
  discriminators and fully validated account lists.
- **Locked liquidity.** The fee pool can only sign the two fee-claim CPIs and transfers out of its own HYPE vault. It
  has no instruction that removes liquidity or moves the position NFT.
- **Replay protection.** Paid-chunk bitmap; strictly increasing epoch numbers and snapshot slots; closed epochs cannot
  be reopened.
- **Narrow keeper role.** The snapshot authority can fund rounds from the holder pool, close them and register the
  canonical DAMM v2 pool; it cannot reach HYMET's or creators' funds or the liquidity. The program checks each payment
  against the round's Merkle root, not against token balances, which it cannot read. Holders therefore rely on the keeper
  to compute recipients and amounts correctly, and a compromised key could misdirect the unpaid holder pool. A round
  funded with a root the keeper did not prepare is detected by the alert check, and the upgrade authority can rotate the key.
- **Upgradeability.** The program is upgradeable by `9G9v3Ax4K2prmVQfDzp2HY6YYZ5UFmCo24761pgEmMo7`. Any upgrade is
  public on-chain and changes the binary hash above.

## Verification status

**Verified on Mainnet**, with real HYPE on HYMET's own test tokens:

- Launches through the distributor (50/50 split, at 1% and 2% trading fees) and the native route
- Buying and selling on Meteora DBC with Phantom
- Keeper settlement of DBC fees, including the 20% HYMET share and the creator and holder split
- Creator claims from the distributor
- Funded holder payouts to wallets that already had a HYPE account: one round on each of two tokens
- Recording carried amounts for wallets below their minimum, with original balances proven against the on-chain snapshot
- Exact reconciliation of chain state, database records and wallet payments

**Devnet, local validator or tests only**, not yet run on Mainnet:

- Graduation, DBC-to-DAMM v2 migration and canonical pool registration, at a 1% fee only
- DAMM v2 trading and DAMM v2 fee settlement
- Any distributor split other than 50/50, including 0% creator / 100% holders
- Repeated payout rounds on one token at the 15-minute interval, and a carried amount growing until it is paid
- Keeper-created HYPE accounts at the 0.05 HYPE minimum
- Paying carried amounts to wallets that have sold (simulated only)
- Native-route creator claims and platform claims

Phantom is the only wallet tested in any environment.

Review status: internal security review and reproducible build; no external audit yet.
