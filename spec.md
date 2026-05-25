# spec.md — MetaDAO Futarchy → Percolator Market Factory + Staking Vault Rewards

look at ../percolator-prog for the program source and as the depencency

this is a pure solana rust program only.  it should use the same litesvm setup for testing as ../percolator-prog

## Design constraints (MUST)

1. No admin keys, no multisigs, no off-chain publishers.
1. Everything "governance-like" is expected to be triggered from a MetaDAO proposal marked `executed=true`.
1. User funds are never at risk from futarchy itself: no futarchy-triggerable instruction may transfer, freeze, confiscate, or redirect user balances.
1. The DAO may stake and claim COIN rewards like any user, but cannot claim other users' staked collateral.
1. Current implementation assumption: the DAO-controlled client bootstraps the governed authority path for this rewards instance at creation time. `rewards` does not independently prove MetaDAO execution beyond that configured path.

-----

## 1. Programs

|Program                                      |Role                                                                             |
|---------------------------------------------|---------------------------------------------------------------------------------|
|`meta_dao` (existing)                        |Proposal lifecycle, futarchy voting, `executed` bit                              |
|`governance_adapter` (current implementation)|Owns the governance authority PDA and CPIs into `rewards`; bootstrap/signing shim |
|`percolator` (existing + one addition in §2) |Market creation, insurance vault                                                |
|`rewards` (new, non-upgradeable)             |COIN mint-authority PDA, staking vault, stake/unstake, governance-gated minting  |
|SPL Token Program                            |COIN mint, collateral token accounts, staking vault                              |

Current implementation note: `rewards` accepts a governance PDA owned by `governance_adapter`, and that adapter is expected to be initialized only from the intended MetaDAO-controlled creation flow. The adapter is a signing shim, not a policy engine. The trust boundary is therefore established during the init ceremony rather than re-proven inside `rewards`.

-----

## 2. Required additions to Percolator

The existing Percolator instruction set is used as-is except for the addition below. It does not touch any existing instruction, account layout outside `_reserved`, or security invariant.

### 2.1 Store `market_start_slot` at InitMarket time

`SlabHeader._reserved[8..16]` is currently zero-initialized and unused at market creation. At the end of the `InitMarket` handler, write the current slot into that field:

```rust
state::write_market_start_slot(data, clock.slot);   // new write in InitMarket
state::read_market_start_slot(data) -> u64;          // new public reader
```

This single u64 is the only anchor the `rewards` program needs to compute elapsed time. It is written once and never mutated. The `rewards` program reads it via the slab account data; it does not accept a caller-supplied start slot.

-----

## 3. Assets

**Collateral** — SPL token deposited by stakers into a per-market staking vault. Each market has its own collateral mint and isolated vault.

**COIN** — SPL token minted exclusively by the `rewards` program. Shared across all markets managed by the same DAO.

COIN mint requirements:

- `mint_authority = PDA(rewards, [b"coin_mint_authority", coin_mint_key])`
- `freeze_authority = None`
- Decimals are fixed at creation and committed in the proposal hash.
- A single COIN mint is shared across all markets. The `CoinConfig` PDA (§10) gates which authority can register new markets.

`CoinConfig` also records the COIN instance bootstrap phase. `init_coin_config` takes a `bootstrap_delay_slots: u64`; a zero delay marks the instance live immediately, and a nonzero delay requires a governed `activate_live` instruction after `bootstrap_start_slot + bootstrap_delay_slots`.

**Genesis base unit** — SPL token deposited during bootstrap. One deposited base unit gives one genesis vote unit. Genesis deposits are intentionally risk-bearing: the pooled base units can be deployed into the first Percolator market as 50% insurance and 50% backing, and users later withdraw up to their deposited principal from recovered funds.

-----

## 4. Epoch

```
epoch_slots: u64          // per-market, set at init_market_rewards; immutable
```

`epoch_slots` defines the minimum lockup period for stakers and the rate denominator for reward calculation. It is stored in `MarketRewardsCfg` and can differ per market (futarchy votes on it).

-----

## 5. MetaDAO proposal payload

A "Create Percolator Market" proposal commits to `market_config_hash = sha256(payload)` where `payload` is the canonical serialization of:

- Full Percolator `MarketConfig` (passed verbatim to `InitMarket`)
- `N: u64` — COIN emitted per epoch to stakers collectively
- `epoch_slots: u64` — minimum lockup / reward period
- COIN mint address and decimals
- Collateral mint address
- Rounding rule: integer truncation with sub-coin remainder carried forward (§8)

-----

## 6. Staking vault

Each market has a per-market staking vault:

```
stake_vault = PDA(rewards, [b"stake_vault", market_slab_key])
```

The vault is an SPL token account whose authority is the MRC PDA. Users deposit collateral to earn COIN rewards. Each market is isolated: separate collateral, separate vault, separate reward rate.

-----

## 7. Market creation and lifecycle

Bootstrap prerequisite: before the first governed call for a `(rewards_program, coin_mint)` pair, the DAO-controlled client initializes the governance authority path for that pair. The current repo assumes this binding is established during instance creation and then reused for all governed CPIs.

The current implementation uses a rewards-program market-admin PDA:

```
market_admin = PDA(rewards, [b"percolator_market_admin", coin_mint])
```

Any user may call `rewards::init_percolator_market` after the COIN instance exists. The caller supplies the Percolator market account and raw Percolator `InitMarket` payload; `rewards` CPIs into Percolator with `market_admin` as the signer, so the user can fund/create a market but cannot become its admin. This permits the first genesis market to run during the bootstrap delay and permits additional permissionless creation after live activation.

Futarchy controls subsequent Percolator lifecycle/admin actions through `governance_adapter::percolator_admin`, which CPIs into `rewards::percolator_admin`. The rewards program verifies `CoinConfig.authority`, signs as `market_admin`, and forwards only lifecycle/admin Percolator tags:

- `UpdateMarketInitFeePolicy`, allowing permissionless user activation of additional asset slots when the fee is nonzero.
- `UpdateAssetLifecycle`, including admin drain, shutdown, retire, and governed activation paths.
- `ConfigurePermissionlessResolve`, oracle configuration, fee policies, authority updates, `ResolveMarket`, and `CloseSlab`.

Cranks are intentionally not owned by futarchy in this design. Permissionless refresh/liquidation/settlement, forced close pairing after shutdown delay, resolved payout claims/topups, and portfolio close flows are triggered externally through Percolator.

### 7.1 Genesis bootstrap

`rewards::init_genesis_bootstrap` creates:

```
genesis_cfg   = PDA(rewards, [b"genesis_cfg", coin_mint])
genesis_vault = PDA(rewards, [b"genesis_vault", coin_mint])
```

`genesis_vault` is a base-token account owned by `market_admin`. During bootstrap, `genesis_deposit(amount)` transfers base units into the vault and records `vote_units += amount`. The config tracks the intended 50/50 principal split in x2 units so odd base-unit totals are accounted for exactly.

`kickstart_genesis_market(domain, expiry_slot)` can deploy the pooled base units into a PDA-admin Percolator market. `floor(total_deposited / 2)` is topped up as insurance and the remainder is topped up as backing for the selected domain. Deposits close once this kickstart happens.

At the end of the bootstrap market run, `recover_genesis_market(kind, domain, amount)` is the only governed recovery path from Percolator back into the genesis ledger. It signs as `market_admin`, requires the Percolator market authorities to still be that PDA, and forces the destination to be `genesis_vault`. It supports Percolator insurance, backing, and backing-earnings withdrawals, but cannot send recovered funds to an arbitrary DAO account.

After `activate_live`, users can create `GenesisDistribution` allocation items and genesis depositors can vote on them using their recorded base-unit vote weight. Futarchy calls `genesis_mint_reward` against a majority-approved item; the item is marked executed and `minted_supply` is capped by `reward_supply`. Only after the bootstrap market has been kickstarted and `minted_supply == reward_supply` can `finalize_genesis` complete. After finalization, `genesis_withdraw` burns a user's vote weight and returns up to that user's deposited principal, pro-rated by recovered vault balance if the bootstrap market lost capital. Underfunded withdrawals retire only actually paid principal, so later recoveries remain reserved for unpaid genesis principal before DAO surplus. `draw_genesis_surplus` can transfer only `genesis_vault_balance - outstanding_genesis_principal` to futarchy.

### 7.2 Builder approval registry

`approve_builder(code_hash, terms_hash, enabled)` creates or updates:

```
builder_approval = PDA(rewards, [b"builder_approval", coin_mint, builder_program, code_hash])
```

This is a local governance registry boundary for external builder/oracle integrations. The approval stores the builder program id, immutable code hash, terms hash, approval slot, and enabled flag. The builder target must be an executable BPF-loader-owned program account. The approval does not give the builder custody over insurance or backing principal.

-----

## 8. Rewards math

### 8.1 Fixed-point scale

```
FP = 2^64
```

### 8.2 Staker rewards — Synthetix-style accumulator

Per market, `MarketRewardsCfg` maintains:

- `reward_per_token_stored: u128` — global accumulator (FP-scaled)
- `last_update_slot: u64`
- `total_staked: u64`

On every stake/unstake/claim, the accumulator is updated:

```
elapsed = current_slot - last_update_slot
if total_staked > 0 and elapsed > 0:
    delta = N * elapsed * FP / (epoch_slots * total_staked)   // u256 intermediate
    reward_per_token_stored += delta
last_update_slot = current_slot
```

Per user, `StakePosition` maintains:

- `amount: u64`
- `deposit_slot: u64`
- `reward_per_token_paid: u128`
- `pending_rewards: u64`

On settle (before any stake/unstake):

```
delta = reward_per_token_stored - reward_per_token_paid
earned = amount * delta / FP
pending_rewards += earned
reward_per_token_paid = reward_per_token_stored
```

**Instruction:** `rewards::stake(amount)` — deposits collateral to vault, creates/updates position, resets lockup.

**Instruction:** `rewards::unstake(amount)` — requires `current_slot >= deposit_slot + epoch_slots`, transfers collateral back, mints pending COIN, closes position on full unstake.

**Instruction:** `rewards::claim_stake_rewards()` — mints pending COIN without unstaking. No lockup check required.

### 8.3 Governance-gated minting (`mint_reward`)

The DAO can vote to mint COIN to any destination (e.g., rewarding best-performing LPs identified off-chain).

**Instruction:** `rewards::activate_live()`

- Signer must be `CoinConfig.authority`.
- Requires `current_slot >= bootstrap_start_slot + bootstrap_delay_slots`.
- Sets the COIN instance phase to live.
- Idempotent after live activation.

**Instruction:** `rewards::mint_reward(amount)`

- Signer must be `CoinConfig.authority` (the preconfigured governance authority path for this COIN).
- Mints `amount` COIN to any provided SPL token destination account.
- Amount must be non-zero.
- Requires the COIN instance to be live.

This replaces on-chain LP fee tracking. LP performance identification is an off-chain process; the DAO votes to reward whichever LPs perform best.

-----

## 9. Staking lockup and withdrawal

Stakers must hold collateral for at least `epoch_slots` after their last deposit before unstaking. Each new stake resets the lockup timer (`deposit_slot = current_slot`).

Claiming COIN rewards (`claim_stake_rewards`) does NOT require lockup to elapse — stakers can harvest COIN at any time.

On full unstake (amount == staked balance), the StakePosition PDA is closed and rent is returned to the user.

-----

## 10. `rewards` program accounts

### CoinConfig

PDA seeds: `[b"coin_cfg", coin_mint_key]`

Created once per COIN token via `init_coin_config`. The authority is the preconfigured governance PDA path established by the DAO-controlled init ceremony for this COIN. It is the only key that can activate live phase, register new markets for this COIN, and call `mint_reward`.

|Field                    |Type  |Description                                      |
|-------------------------|------|--------------------------------------------------|
|`authority`              |Pubkey|Who can call live-phase governance instructions for this COIN |
|`bootstrap_start_slot`   |u64   |Slot when `init_coin_config` created the phase record |
|`bootstrap_delay_slots`  |u64   |Configured bootstrap delay before live activation |
|`live_slot`              |u64   |Slot where live phase was activated; zero while still bootstrapping |
|`phase`                  |u8    |`0 = bootstrap`, `1 = live`                       |

### MarketRewardsCfg

PDA seeds: `[b"mrc", market_slab_key]`

|Field                       |Type  |Description                                          |
|----------------------------|------|-----------------------------------------------------|
|`market_slab`               |Pubkey|Percolator slab account                              |
|`coin_mint`                 |Pubkey|COIN mint address                                    |
|`collateral_mint`           |Pubkey|Collateral token for this market's staking vault     |
|`n_per_epoch`               |u64   |COIN emitted per epoch to stakers                    |
|`epoch_slots`               |u64   |Minimum lockup / reward period (slots)               |
|`market_start_slot`         |u64   |Read from slab at init; immutable                    |
|`reward_per_token_stored`   |u128  |Synthetix-style accumulator (FP-scaled)              |
|`last_update_slot`          |u64   |Last slot accumulator was updated                    |
|`total_staked`              |u64   |Total collateral currently staked                    |

### StakePosition

PDA seeds: `[b"sp", market_slab_key, user_pubkey]`

|Field                    |Type |Description                                       |
|-------------------------|-----|--------------------------------------------------|
|`amount`                 |u64  |Collateral currently staked                       |
|`deposit_slot`           |u64  |Slot of last deposit (lockup reference)           |
|`reward_per_token_paid`  |u128 |Accumulator snapshot at last settle               |
|`pending_rewards`        |u64  |Unsettled COIN rewards                            |

-----

## 11. Forbidden capabilities (MUST NOT exist)

No instruction in MetaDAO, Percolator, or `rewards` may:

- Transfer tokens from arbitrary user accounts.
- Withdraw collateral from the staking vault except to the user who staked it (via `unstake`).
- Withdraw SOL from the Percolator `insurance_vault` except via Percolator's existing, non-governance risk-engine rules.
- Freeze user token accounts.
- Set or change the COIN mint freeze authority (must remain `None`).
- Modify any reward parameter (`N`, `epoch_slots`, `coin_mint`) after `init_market_rewards` is called.
- Invoke arbitrary Percolator funding or withdrawal instructions through the generic futarchy admin proxy.
- Invoke raw Percolator `UpdateAuthority` through the generic futarchy admin proxy; custody authority changes must use explicit meta-program paths.

-----

## 12. Explicit assumptions

- Reward accounting starts at `init_market_rewards`; Percolator market creation can happen earlier during bootstrap, but staker COIN emissions begin only once the market is registered with the rewards program.
- The `rewards` program is deployed non-upgradeable. The COIN mint `freeze_authority` is `None` at creation and cannot be set afterward.
- Integer truncation in the Synthetix accumulator may cause up to 1 COIN per claim to be deferred. The sub-coin remainder is never lost; it becomes claimable as the accumulator advances.

-----

## 13. Audit checklist

- [ ] `init_percolator_market` sets Percolator admin/authority fields to `market_admin = PDA(rewards, [b"percolator_market_admin", coin_mint])`.
- [ ] `rewards::init_market_rewards` creates `MarketRewardsCfg` with an init guard; a second call on the same slab fails.
- [ ] `market_start_slot` is read from the slab by the `rewards` program; it is not accepted as an instruction argument.
- [ ] Staker reward accumulator update is serialized to MRC before any CPI, preventing double-accumulation.
- [ ] Staker collateral can only be withdrawn by the depositor, after lockup elapses, to their own token account.
- [ ] For all stakers combined: total claimable per slot ≤ `N / epoch_slots`. No single staker can claim more than their `amount / total_staked` fraction.
- [ ] `mint_reward` requires `CoinConfig.authority` as signer; unauthorized callers are rejected.
- [ ] `init_coin_config` records the configured bootstrap delay, and nonzero-delay instances cannot enter live-governance paths until `activate_live` succeeds after the delay.
- [ ] COIN mint `freeze_authority = None`; `rewards` program is non-upgradeable at deploy.
- [ ] Genesis deposits mint one vote unit per base unit, close after kickstart, and withdraw up to principal only after kickstart and full reward-supply distribution are finalized.
- [ ] `recover_genesis_market` can recover bootstrap capital only to `genesis_vault`, and it is disabled after genesis finalization.
- [ ] Genesis reward minting executes only majority-approved `GenesisDistribution` items and cannot exceed `reward_supply`.
- [ ] Builder/oracle code approvals are keyed by `(coin_mint, builder_program, code_hash)`, require an executable BPF-loader-owned program account, and carry a visible terms hash.
- [ ] `CoinConfig.authority` is the only key that can register new markets for a given COIN; unauthorized callers are rejected.
- [ ] Stake vault PDA authority is the MRC PDA; only `unstake` can transfer collateral out.
- [ ] Deployment/init docs specify how the DAO-controlled client bootstraps the governance authority path for this rewards instance.
