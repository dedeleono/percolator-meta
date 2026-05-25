# Percolator Insurance Deposit Program

Solana program that incentivizes insurance capital deposits for [Percolator](https://github.com/aeyakovenko/percolator-prog) markets, governed by MetaDAO futarchy.

## How It Works

1. **DAO bootstraps COIN governance** — MetaDAO governance initializes `CoinConfig` with a configurable bootstrap delay in slots. A zero delay makes the instance live immediately; a nonzero delay requires a later `activate_live` call after the delay elapses. The intended launch setting can be a six-month slot delay.
2. **Genesis depositors fund the first market** — During bootstrap, users deposit base units into the genesis vault. One deposited base unit is one genesis vote unit. These deposits take code and market risk during bootstrap in exchange for the right to vote on the genesis COIN distribution.
3. **The first market is kickstarted** — Governance deploys the pooled genesis base units into a PDA-admin Percolator market as a 50/50 insurance/backing split. The market then runs permissionlessly through the bootstrap window; cranks are external.
4. **Genesis capital is recovered** — Before finalization, futarchy can recover insurance/backing principal and earnings from the PDA-admin bootstrap market only back into `genesis_vault`.
5. **Genesis distributes 100% of COIN** — After `activate_live`, genesis depositors vote on allocation items and futarchy mints the fixed genesis reward supply to the approved recipients. `finalize_genesis` succeeds only after the bootstrap market has been kicked and the full supply cap has been minted.
6. **Genesis depositors exit** — After finalization, genesis voters can withdraw up to their original base-unit deposit, pro-rated by recovered vault balance if the bootstrap market lost capital. Their vote weight is burned on claim; unpaid principal remains reserved for later recovery and can be claimed later.
7. **Surplus goes to futarchy** — Remaining base-token surplus in the genesis vault is drawable by governance only after unclaimed genesis principal is covered.
8. **Post-genesis markets continue normally** — Anyone can initialize additional Percolator markets through `init_percolator_market`; futarchy controls lifecycle/admin actions, fee policies, oracle setup, and builder approvals through the market-admin PDA and explicit registry instructions.

### Bootstrap Phase

`CoinConfig` records the bootstrap start slot, the configured delay, the live slot, and the current phase. Governance actions that add markets, mint discretionary rewards, retune emissions, transfer COIN mint authority, or draw vault profits require the COIN instance to be live. This keeps the launch delay explicit on-chain and lets deployments use a six-month delay or any other voted slot duration.

### Genesis Bootstrap

`init_genesis_bootstrap` creates a `GenesisConfig` PDA and a base-token `genesis_vault` owned by the COIN instance's `percolator_market_admin` PDA. During bootstrap, `genesis_deposit` transfers base units into that vault and records one vote unit per deposited base unit. The ledger tracks the intended 50/50 principal split in x2 units, so odd deposits can still be accounted for exactly even though SPL tokens are indivisible.

`kickstart_genesis_market` deploys the pooled base units into the first Percolator market: `floor(total / 2)` goes to Percolator insurance and the remainder goes to the selected backing domain. Deposits close once kickstarted. `recover_genesis_market` is the constrained recovery path: it signs as the market-admin PDA but can only transfer recovered Percolator insurance/backing funds back into `genesis_vault`, and it is disabled after finalization.

After the bootstrap delay, anyone can create `init_genesis_distribution` items for COIN destinations and amounts. Genesis depositors vote with their recorded base-unit vote weight through `vote_genesis_distribution`, and all 100% of the fixed genesis supply must be minted through executed, majority-approved distribution items. Futarchy can call `genesis_mint_reward` only for an approved item, and `finalize_genesis` requires the genesis market to have been kicked plus `minted_supply == reward_supply`.

Genesis withdrawals are risk-bearing. `genesis_withdraw` burns the user's remaining vote weight and transfers up to their principal from recovered base units. If only half of outstanding principal is present, each claimant gets half of their remaining deposit. The unpaid principal claim remains reserved for later recovery, so a user can withdraw again if more base units return to the vault. `draw_genesis_surplus` can move only `genesis_vault_balance - outstanding_genesis_principal` to futarchy.

### Percolator Lifecycle

The rewards program owns a deterministic `percolator_market_admin` PDA for each COIN mint. Permissionless market creation CPIs into Percolator `InitMarket` with that PDA as admin, so the caller funds/creates the market account but does not receive admin rights.

Futarchy can then invoke the latest Percolator admin lifecycle surface through `percolator_admin`: market-init fee policy, asset activation/drain/shutdown/retire, oracle configuration, resolve, close-slab cleanup, and fee policies. Raw Percolator `UpdateAuthority` is intentionally not forwarded through the generic admin proxy; custody-bearing authority changes must use the explicit meta-program setup paths. Portfolio cranks, force-close pairing, payout topups, and other permissionless maintenance remain external crank work.

`approve_builder` records a governed builder-code approval by `(coin_mint, builder_program, code_hash)` with a separate terms hash and enabled flag. The target must be an executable BPF-loader-owned program account. This gives governance an on-chain registry boundary for external builder/oracle integrations without granting those builders custody over depositor principal.

## Capital Protection

**Post-genesis staking depositor capital is never at risk from governance.** The `draw_insurance` instruction enforces:

```
drawable = vault_balance - total_staked   (profit only)
```

The DAO cannot draw below `total_staked`. Staking depositors always get their full deposit back under normal program accounting. Genesis depositors are different: they deliberately take the first market's code and market risk in exchange for genesis voting power.

COIN rewards are minted (not drawn from the vault), so they are also never at risk.

### How Rewards Work

Depositors earn COIN proportional to their share of the pool and time deposited (Synthetix-style accumulator):

```
reward_rate = n_per_epoch / epoch_slots   (COIN per slot for the entire pool)
your_rate = reward_rate * your_deposit / total_staked
```

| Scenario | Collateral returned | COIN rewards |
|----------|-------------------|--------------|
| Deposit and withdraw same slot | 100% | 0 COIN |
| Deposit 1 epoch, no profit draw | 100% | ~N COIN |
| Deposit 1 epoch, DAO drew profits | 100% (capital protected) | ~N COIN |
| Withdraw before others | 100% | pro-rata to time |
| Stay longer than others | 100% | more COIN (larger share after others leave) |

### Per-Market Isolation

Each market has independent:
- Deposit vault (separate SPL token account)
- Reward rate (`n_per_epoch`, `epoch_slots`)
- Total staked tracking
- Depositor positions (per-user PDAs)

**Isolation guarantees (cross-market):**
- DAO drawing profit from Market A does not touch Market B's vault
- Profit budget is per-market: profit in Market A does not let the DAO draw Market B's depositor capital
- A loss in Market A (defense-in-depth scenario) does not haircut Market B depositors
- The same user staking in two markets has two independent positions; withdrawing from one does not affect the other
- Operations on Market A's MRC do not mutate Market B's MRC state
- Cross-market account substitution (passing Market A's MRC with Market B's vault) is rejected by PDA verification

### Reward Conservation

Total COIN emitted equals `n_per_epoch × elapsed_epochs` (within at most 1 token per active depositor lost to fixed-point truncation). The accumulator math cannot create or destroy rewards beyond that bound.

### Withdrawal Guarantee

**The DAO cannot block withdrawals.** The `unstake` instruction is fully permissionless:
- No governance key is checked during withdrawal
- No governance-modifiable state gates the transfer
- Every account in the path is either user-controlled or program-derived (PDA)
- `claim_stake_rewards` also always succeeds — COIN is minted, not drawn from the vault

### Proportional Withdrawal (Defense-in-Depth)

As defense-in-depth, withdrawals use proportional math: `actual = min(amount, amount * vault_balance / total_staked)`. Under normal operation `vault_balance >= total_staked`, so this equals the full deposit. If the vault were ever underfunded (which `draw_insurance` prevents), all depositors would take the same proportional share.

### Attack Vector Analysis

| Vector | Protection |
|--------|-----------|
| DAO draws depositor capital | `draw_insurance` enforces `amount <= vault_balance - total_staked` — only profits drawable |
| Attacker draws from vault | Requires governance PDA signature — only DAO votes can trigger draws |
| Attacker withdraws another user's deposit | StakePosition PDA derived from `[market_slab, user_pubkey]` — cryptographically bound |
| Attacker uses fake MRC account | MRC verified via PDA derivation — fake accounts don't match expected key |
| Attacker inflates shared COIN | `init_market_rewards` requires governance authority — only DAO can register markets |
| Flash deposit to steal rewards | Same-slot deposit+withdraw earns 0 COIN |
| Withdraw 1 repeatedly for rounding profit | Integer division truncates down — repeated small withdrawals total exact deposit |
| 1-token dilution attack | 1 token in a 1M pool dilutes rewards by < 0.0001% — negligible |
| Direct vault transfer manipulation | Extra tokens become drawable profit — depositors unaffected |
| Freeze user's COIN tokens | COIN mint has no freeze authority — verified at init |
| Claim then unstake double-counting | `pending_rewards` zeroed on claim — no double payout |
| Cross-market vault substitution | MRC/vault/slab keys all PDA-derived from `market_slab` — substituting another market's vault fails the PDA check |
| Drain Market B using Market A's profit | Profit is computed per-market (`vault_B - total_staked_B`); Market A's surplus is irrelevant |
| Loss in one market spreads to another | Each market has its own SPL token account vault and MRC; no shared capital between markets |
| Steal another user's position via SP PDA | StakePosition PDA is derived from `[b"sp", market_slab, user]` — bound to both market and user |

## Tested Invariants

Each invariant is enforced by at least one integration test. Run `RUST_MIN_STACK=8388608 cargo test --manifest-path program/Cargo.toml --test integration` to verify them.

### Capital protection
- DAO cannot draw depositor capital — `test_draw_depositor_capital_rejected`
- DAO can draw exactly the available profit — `test_draw_only_profits`
- Depositors get full deposit back after DAO drew profits — `test_depositors_always_get_full_deposit_back`, `test_depositor_capital_protected_after_profit_draw`
- DAO can drain remaining vault only after all depositors exit — `test_draw_all_remaining_after_depositors_withdraw`
- Non-governance signers cannot call `draw_insurance` — `test_draw_insurance_non_governance_rejected`
- Zero-amount draws rejected — `test_draw_zero_amount_rejected`

### Withdrawal guarantee
- `unstake` succeeds even when the vault is fully empty — `test_withdrawal_always_succeeds_after_full_drain`
- `claim_stake_rewards` succeeds independent of vault state — `test_claim_rewards_always_works`
- No governance action can prevent withdrawal — `test_withdrawal_always_works_no_governance_block`

### Market isolation
- Drawing from Market A leaves Market B's vault exactly unchanged — `test_isolation_draw_from_market_a_does_not_touch_market_b`
- Profit in Market A does not let DAO drain Market B — `test_isolation_dao_cannot_draw_from_market_b_via_market_a_profit`
- Cross-market account substitution rejected — `test_isolation_cross_market_attack_wrong_mrc_with_other_vault`, `test_isolation_unstake_wrong_market_vault_rejected`
- Same user has independent positions in different markets — `test_isolation_alice_two_market_positions_independent`
- Loss/drain in Market A does not haircut Market B — `test_isolation_market_a_drained_does_not_haircut_market_b`
- Profit is computed per-market — `test_isolation_per_market_profit_calculation`
- MRC state changes are per-market — `test_isolation_market_a_loss_does_not_change_market_b_total_staked`
- DAO can drain remaining in one market while others continue — `test_isolation_dao_can_only_drain_after_local_market_depositors_exit`
- Different markets can have different yield rates — `test_two_markets_share_one_coin`

### Reward math
- Conservation: total emitted ≈ `n_per_epoch × elapsed_epochs` — `test_two_users_equal_stake`, `test_two_users_different_amounts`, `test_staker_joins_later`
- Proportional split by stake size — `test_two_users_different_amounts`
- Same-slot stake+withdraw earns 0 COIN — `test_immediate_withdraw_returns_deposit_zero_rewards`, `test_adversarial_flash_deposit_no_extra_rewards`
- Late depositors share rewards from join time only — `test_staker_joins_later`
- N=0 emits no rewards — `test_n_zero_no_rewards_emitted`
- Claim then unstake does not double-count — `test_adversarial_claim_then_unstake_no_double_rewards`, `test_claim_then_unstake_no_double_rewards`

### Adversarial attacks
- Cannot steal another user's position via wrong SP PDA — `test_adversarial_steal_via_wrong_sp_pda`, `test_unstake_wrong_user_sp_rejected`, `test_no_instruction_to_redirect_user_funds`
- Direct vault transfer cannot drain depositors — `test_adversarial_direct_vault_transfer_no_steal`
- 1-token dilution attack is negligible — `test_adversarial_1_token_dilution_negligible`
- Repeated 1-token withdrawals don't extract more than fair share — `test_adversarial_withdraw_1_repeatedly_no_rounding_exploit`
- Same-slot triple-op (stake+claim+unstake) earns 0 — `test_adversarial_same_slot_triple_op`
- Fake MRC accounts rejected — `test_adversarial_fake_mrc_rejected`
- Wrong stake_vault PDA rejected — `test_unstake_wrong_stake_vault_fails`
- Cross-market COIN inflation rejected — `test_unauthorized_market_cannot_inflate_shared_coin`

### Defense-in-depth (proportional withdrawal)
- Equal positions take equal haircut — `test_proportional_withdrawal_defense_in_depth`
- Unequal positions take equal haircut rate — `test_proportional_withdrawal_unequal_positions_defense_in_depth`
- Partial withdrawal math correct — `test_proportional_partial_withdrawal_defense_in_depth`
- Full drain returns 0 collateral but does not revert — `test_proportional_full_drain_defense_in_depth`

### Genesis and builder registry
- Genesis distribution vote, mint, withdrawal, and surplus flow — `test_genesis_bootstrap_votes_distribution_withdrawal_and_surplus`
- Permissionless genesis allocation creation is phase/cap/token bounded — `test_genesis_distribution_creation_is_permissionless_but_bounded`
- Genesis vote records are depositor-only, non-transferable, revotable without double-counting, and strict-majority gated — `test_genesis_vote_records_are_nontransferable_and_strict_majority`
- Governance adapter has a fixed, controller-gated genesis/admin surface — `test_genesis_governance_surface_is_fixed_and_controller_gated`
- Genesis cannot finalize before kickstart and unpaid underfunded claims remain reserved — `test_genesis_finalize_requires_market_kickstart`, `test_underfunded_genesis_withdrawal_keeps_unpaid_principal_claim`
- Risk-vault setup is live-gated and backing fees route to main insurance — `test_risk_vault_setup_is_live_gated_and_fees_route_to_main_insurance`
- Genesis recovery rejects extra ledger accounts except for backing-earnings recovery — `test_genesis_recovery_rejects_unneeded_ledger_accounts`
- Genesis market kickstart 50/50 accounting — `test_genesis_bootstrap_kickstarts_market_50_50`
- Governed builder code and terms registry — `test_builder_code_approval_registry_is_governed_and_versioned`

### Init guards
- Reward init accepts only legacy burned-admin slabs or slabs controlled by the COIN market-admin PDA — `test_init_market_rewards_live_admin_fails`, `test_genesis_bootstrap_kickstarts_market_50_50`
- Cannot init twice — `test_init_market_rewards_double_init_fails`, `test_init_coin_config_double_init_fails`
- COIN mint must have no freeze authority — `test_init_coin_config_freeze_authority_fails`
- COIN mint authority must be the program PDA — `test_init_coin_config_wrong_mint_authority_fails`
- COIN mint must be SPL Token-owned — `test_init_coin_config_non_spl_mint_rejected`
- Direct EOA authority rejected — `test_init_coin_config_direct_eoa_authority_rejected`
- Wrong governance authority rejected — `test_init_market_rewards_wrong_authority_fails`

## Instructions

| Tag | Instruction | Description |
|-----|-------------|-------------|
| 0 | `init_market_rewards` | Create per-market reward config + deposit vault (governance-gated) |
| 1 | `stake` | Deposit collateral to vault, begin earning COIN |
| 2 | `unstake` | Withdraw full deposit + claim pending COIN (no lockup) |
| 3 | `init_coin_config` | One-time COIN mint authority setup (governance-gated) |
| 4 | `claim_stake_rewards` | Harvest pending COIN without withdrawing collateral |
| 5 | `draw_insurance` | Governance-gated: withdraw profits from vault (only excess above total_staked) |
| 6 | `register_insurance_operator` | Legacy burned-admin setup: register the MRC PDA as Percolator insurance operator |
| 7 | `pull_insurance` | Permissionlessly sweep Percolator insurance into the stake vault through the registered operator |
| 8 | `mint_reward` | Governance-gated discretionary COIN mint |
| 9 | `set_market_rewards` | Governance-gated reward emission update |
| 10 | `transfer_mint_authority` | Governance-gated COIN mint-authority transfer or burn |
| 11 | `activate_live` | Governance-gated transition from bootstrap to live after the configured delay |
| 12 | `init_risk_vault` | Governance-gated live-phase insurance/backing risk-vault setup |
| 13 | `register_risk_vault_authority` | Register a risk-vault PDA as the corresponding Percolator authority |
| 14 | `risk_deposit` | External risk depositor funding into insurance or backing |
| 15 | `risk_request_withdraw` | Request a delayed risk-principal withdrawal |
| 16 | `risk_withdraw` | Withdraw matured risk principal after lockup/delay |
| 17 | `sync_risk_vault` | Permissionless sync of Percolator ledger counters into risk accumulators |
| 18 | `risk_claim_rewards` | Claim backing earnings minus DAO fee routed to main insurance |
| 19 | `init_percolator_market` | Permissionless Percolator `InitMarket` with the COIN market-admin PDA as admin |
| 20 | `percolator_admin` | Governance-gated Percolator lifecycle/admin CPI signed by the market-admin PDA |
| 21 | `init_genesis_bootstrap` | Governance-gated genesis vault and fixed reward-supply setup |
| 22 | `genesis_deposit` | Bootstrap-only base-unit deposit; 1 base unit = 1 vote unit |
| 23 | `genesis_withdraw` | Post-finalization withdrawal up to deposited principal, pro-rated by recovered funds |
| 24 | `genesis_mint_reward` | Governance-gated mint against the fixed genesis reward-supply cap |
| 25 | `finalize_genesis` | Mark genesis distribution complete after kickstart and 100% of supply is minted |
| 26 | `draw_genesis_surplus` | Governance-gated draw of base-token surplus above unclaimed genesis principal |
| 27 | `kickstart_genesis_market` | Deploy genesis base units into the first market as 50/50 insurance/backing |
| 28 | `recover_genesis_market` | Recover bootstrap-market insurance/backing funds only back into `genesis_vault` before finalization |
| 29 | `init_genesis_distribution` | Permissionless creation of a genesis COIN allocation item |
| 30 | `vote_genesis_distribution` | Genesis depositor vote on an allocation item using recorded vote units |
| 31 | `approve_builder` | Governance-gated builder-code and terms-hash registry entry |

## Accounts

| Account | PDA Seeds | Description |
|---------|-----------|-------------|
| CoinConfig | `[b"coin_cfg", coin_mint]` | Governance authority and bootstrap/live phase state for this COIN |
| GenesisConfig | `[b"genesis_cfg", coin_mint]` | Genesis deposits, vote units, reward supply cap, and finalized/kicked flags |
| GenesisVault | `[b"genesis_vault", coin_mint]` | Base-token vault owned by the COIN market-admin PDA |
| GenesisPosition | `[b"genesis_position", genesis_cfg, user]` | Per-user genesis deposit, withdrawn principal, and vote units |
| GenesisDistribution | `[b"genesis_distribution", genesis_cfg, proposal_id]` | Vote-approved genesis mint allocation item |
| GenesisDistributionVote | `[b"genesis_distribution_vote", distribution, voter]` | Per-voter ballot record for one genesis allocation item |
| BuilderApproval | `[b"builder_approval", coin_mint, builder_program, code_hash]` | Governed builder-code approval and terms hash |
| MarketRewardsCfg | `[b"mrc", market_slab]` | Per-market reward parameters and accumulator state |
| StakePosition | `[b"sp", market_slab, user]` | Per-user deposit position (accounting units) |
| Deposit Vault | `[b"stake_vault", market_slab]` | SPL token account holding deposited collateral + profits |
| Mint Authority | `[b"coin_mint_authority", coin_mint]` | PDA that signs COIN mints |

## Building

```bash
cargo build-sbf --manifest-path program/Cargo.toml
```

## Testing

Requires the percolator-prog BPF binary to be built first:

```bash
cd ../percolator-prog && cargo build-sbf
cd ../percolator-meta
cargo build-sbf --manifest-path governance/Cargo.toml
cargo build-sbf --manifest-path program/Cargo.toml
RUST_MIN_STACK=8388608 cargo test --manifest-path program/Cargo.toml --test integration
```

The `RUST_MIN_STACK=8MB` is required due to Percolator's >1MB `RiskEngine` stack size.
