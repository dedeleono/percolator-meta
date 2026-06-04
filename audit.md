# Audit

## Scope

Compared the current implementation and tests against:

- `spec.md`
- `README.md`
- `program/src/lib.rs`
- `governance/src/lib.rs`
- `program/tests/integration.rs`

## Findings

### 1. Medium: the governance trust boundary is operational, not enforced inside `rewards`

Under the current design, the governing authority for an instance is established during a trusted init ceremony performed by the DAO-controlled client. That is now the documented model, but it is still important to state clearly what the code does and does not enforce:

- `governance::process_init_authority` only checks `payer.is_signer` and PDA derivation (`governance/src/lib.rs:107-151`).
- `governance::process_init_coin_config`, `process_init_market_rewards`, and `process_mint_reward` also rely on that same governed PDA path and do not verify MetaDAO proposal state themselves (`governance/src/lib.rs:153-196`, `governance/src/lib.rs:198-267`, `governance/src/lib.rs:269-321`).
- `rewards` accepts the configured governance PDA owned by `governance_adapter`; it does not independently verify a MetaDAO proposal or `executed=true` inside `rewards` (`program/src/lib.rs:331-350`, `program/src/lib.rs:403`, `program/src/lib.rs:486`, `program/src/lib.rs:913`).

Effect:

- This is acceptable only if the init ceremony is treated as part of the system trust model.
- If a deployment violates that ceremony, the program will not detect that mistake on-chain.

This is no longer a pure code/design mismatch after the documentation update, but it remains the main operational assumption in the repo.

### 2. Medium: tests do not model the documented bootstrap trust assumption or full MetaDAO execution path

The integration suite uses LiteSVM and covers the direct `rewards` and Percolator flows well, but it does not yet model the full documented governance ceremony:

- Happy paths initialize the governance adapter authority and then call governed instructions through it (`program/tests/integration.rs:499-517`, `program/tests/integration.rs:625-646`, `program/tests/integration.rs:682-709`, `program/tests/integration.rs:1088-1110`).
- The suite proves that direct EOAs cannot call governed `rewards` instructions (`program/tests/integration.rs:1221-1228`, `program/tests/integration.rs:1392-1427`, `program/tests/integration.rs:2188-2193`).
- The suite does not exercise a real MetaDAO proposal execution path or a client-side bootstrap flow that binds the governed PDA to a specific MetaDAO instance.

Effect:

- The most important trust assumption in the current design is documented, but not exercised end-to-end in tests.
- Reviewers must still rely on deployment procedure and client implementation for that part of the system.

## Confirmed Invariants

- `rewards::init_market_rewards` now rejects live-admin slabs and only accepts markets whose Percolator admin has already been burned (`program/src/lib.rs:496-503`).
- Happy-path test helpers burn admin before reward onboarding (`program/tests/integration.rs:649-654`, `program/tests/integration.rs:712-739`).
- A dedicated regression test rejects live-admin reward onboarding (`program/tests/integration.rs:1355-1360`).
- User payout destinations remain validated for `unstake` and `claim_stake_rewards`, and those protections are covered by tests (`program/src/lib.rs:717-744`, `program/src/lib.rs:849-860`, `program/tests/integration.rs:1651-1675`, `program/tests/integration.rs:1770-1777`).

## Testing Notes

- The integration suite uses LiteSVM.
- The current governed ABI under test is the adapter-based `payer + governance PDA` flow, not a direct MetaDAO CPI.
- Current coverage is strongest on staking, reward math, admin burn, and direct unauthorized caller rejection.
- Current coverage is weakest on the deployment/bootstrap ceremony and on a real MetaDAO `executed=true` path.

## Recommended Next Steps

1. Keep the trusted init ceremony explicit in client and deployment documentation.
   The docs should continue to say that the DAO-controlled client must bootstrap the governing PDA path at instance creation.

2. Add an end-to-end LiteSVM test that mirrors the documented bootstrap flow.
   Even if MetaDAO is mocked, the suite should show the exact ceremony the client is expected to perform.

3. If stronger guarantees are desired later, add an immutable on-chain binding from this program instance to a specific MetaDAO PDA.
   That would move the trust boundary from deployment procedure into contract state.
