//! Insurance deposit program: users deposit collateral into per-market vaults
//! and earn COIN (DAO token) as yield. No lockup — withdraw anytime.
//! Non-upgradeable. No admin keys. CoinConfig authority gates bootstrap/live
//! phase transitions and market registration.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[allow(unused_imports)]
use alloc::format; // Required by entrypoint! macro in SBF builds

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    declare_id, entrypoint,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::{clock::Clock, Sysvar},
};

declare_id!("Rewards111111111111111111111111111111111111");

use governance_adapter::{
    authority_address as governance_authority_address, id as governance_program_id,
};

mod percolator_abi {
    use solana_program::{declare_id, program_error::ProgramError};

    declare_id!("Perco1ator111111111111111111111111111111111");

    const MAGIC: u64 = 0x5045_5243_5631_3600; // "PERCV16\0"
    const VERSION: u16 = 16;
    const KIND_MARKET: u8 = 1;
    const KIND_BACKING_DOMAIN_LEDGER: u8 = 3;
    const KIND_INSURANCE_LEDGER: u8 = 4;
    const HEADER_LEN: usize = 16;
    const WRAPPER_CONFIG_LEN: usize = 624;
    pub const BACKING_DOMAIN_LEDGER_ACCOUNT_LEN: usize = HEADER_LEN + 224;
    pub const INSURANCE_LEDGER_ACCOUNT_LEN: usize = HEADER_LEN + 160;
    const CFG_ADMIN_OFF: usize = HEADER_LEN;
    const CFG_COLLATERAL_MINT_OFF: usize = HEADER_LEN + 32;
    const CFG_SECONDARY_COLLATERAL_MINT_OFF: usize = HEADER_LEN + 64;
    const CFG_INSURANCE_AUTHORITY_OFF: usize = HEADER_LEN + 192;
    const CFG_INSURANCE_OPERATOR_OFF: usize = HEADER_LEN + 224;
    const CFG_BACKING_BUCKET_AUTHORITY_OFF: usize = HEADER_LEN + 256;

    pub struct MarketConfig {
        pub admin: [u8; 32],
        pub collateral_mint: [u8; 32],
        pub secondary_collateral_mint: [u8; 32],
        pub insurance_authority: [u8; 32],
        pub insurance_operator: [u8; 32],
        pub backing_bucket_authority: [u8; 32],
    }

    pub struct InsuranceLedger {
        pub market_group: [u8; 32],
        pub authority: [u8; 32],
        pub cumulative_loss_atoms: u128,
    }

    pub struct BackingDomainLedger {
        pub market_group: [u8; 32],
        pub authority: [u8; 32],
        pub total_earnings_atoms: u128,
        pub cumulative_loss_atoms: u128,
        pub cumulative_recovery_atoms: u128,
        pub domain: u16,
    }

    fn read_u16(data: &[u8], off: usize) -> Result<u16, ProgramError> {
        let bytes = data
            .get(off..off + 2)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u128(data: &[u8], off: usize) -> Result<u128, ProgramError> {
        let bytes = data
            .get(off..off + 16)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(u128::from_le_bytes(bytes))
    }

    fn read_u64(data: &[u8], off: usize) -> Result<u64, ProgramError> {
        let bytes = data
            .get(off..off + 8)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_pubkey_bytes(data: &[u8], off: usize) -> Result<[u8; 32], ProgramError> {
        let mut out = [0u8; 32];
        out.copy_from_slice(
            data.get(off..off + 32)
                .ok_or(ProgramError::InvalidAccountData)?,
        );
        Ok(out)
    }

    fn check_header(data: &[u8], kind: u8, min_len: usize) -> Result<(), ProgramError> {
        if data.len() < min_len {
            return Err(ProgramError::InvalidAccountData);
        }
        if read_u64(data, 0)? != MAGIC || read_u16(data, 8)? != VERSION || data[10] != kind {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    pub fn read_market_config(data: &[u8]) -> Result<MarketConfig, ProgramError> {
        check_header(data, KIND_MARKET, HEADER_LEN + WRAPPER_CONFIG_LEN)?;

        let config = MarketConfig {
            admin: read_pubkey_bytes(data, CFG_ADMIN_OFF)?,
            collateral_mint: read_pubkey_bytes(data, CFG_COLLATERAL_MINT_OFF)?,
            secondary_collateral_mint: read_pubkey_bytes(data, CFG_SECONDARY_COLLATERAL_MINT_OFF)?,
            insurance_authority: read_pubkey_bytes(data, CFG_INSURANCE_AUTHORITY_OFF)?,
            insurance_operator: read_pubkey_bytes(data, CFG_INSURANCE_OPERATOR_OFF)?,
            backing_bucket_authority: read_pubkey_bytes(data, CFG_BACKING_BUCKET_AUTHORITY_OFF)?,
        };
        if config.collateral_mint == [0u8; 32]
            || (config.secondary_collateral_mint != [0u8; 32]
                && config.secondary_collateral_mint == config.collateral_mint)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(config)
    }

    pub fn read_insurance_ledger(data: &[u8]) -> Result<InsuranceLedger, ProgramError> {
        check_header(data, KIND_INSURANCE_LEDGER, INSURANCE_LEDGER_ACCOUNT_LEN)?;
        let ledger = InsuranceLedger {
            market_group: read_pubkey_bytes(data, HEADER_LEN)?,
            authority: read_pubkey_bytes(data, HEADER_LEN + 32)?,
            cumulative_loss_atoms: read_u128(data, HEADER_LEN + 128)?,
        };
        if ledger.market_group == [0u8; 32] || ledger.authority == [0u8; 32] {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(ledger)
    }

    pub fn read_backing_domain_ledger(data: &[u8]) -> Result<BackingDomainLedger, ProgramError> {
        check_header(
            data,
            KIND_BACKING_DOMAIN_LEDGER,
            BACKING_DOMAIN_LEDGER_ACCOUNT_LEN,
        )?;
        let ledger = BackingDomainLedger {
            market_group: read_pubkey_bytes(data, HEADER_LEN)?,
            authority: read_pubkey_bytes(data, HEADER_LEN + 32)?,
            total_earnings_atoms: read_u128(data, HEADER_LEN + 112)?,
            cumulative_loss_atoms: read_u128(data, HEADER_LEN + 160)?,
            cumulative_recovery_atoms: read_u128(data, HEADER_LEN + 176)?,
            domain: read_u16(data, HEADER_LEN + 208)?,
        };
        if ledger.market_group == [0u8; 32] || ledger.authority == [0u8; 32] {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(ledger)
    }
}

/// Fixed-point scale for reward math.
pub const FP: u128 = 1u128 << 64;

/// Instruction tags
const IX_INIT_MARKET_REWARDS: u8 = 0;
const IX_STAKE: u8 = 1;
const IX_UNSTAKE: u8 = 2;
const IX_INIT_COIN_CONFIG: u8 = 3;
const IX_CLAIM_STAKE_REWARDS: u8 = 4;
const IX_DRAW_INSURANCE: u8 = 5;
/// Register the MRC PDA as the percolator market's insurance_operator.
/// Legacy helper for burned-admin markets. PDA-admin genesis markets should use
/// the governed lifecycle/admin path instead.
/// Uses invoke_signed with MRC seeds so the new authority (the PDA)
/// is treated as a signer by percolator's UpdateAuthority handler.
const IX_REGISTER_INSURANCE_OPERATOR: u8 = 6;
/// Pull tokens from the percolator market's insurance fund into our
/// stake_vault via WithdrawInsuranceLimited. MRC PDA must be the
/// registered insurance_operator. Permissionless keeper — anyone can
/// call it; destination is always the stake_vault, so only deposit-
/// facing instructions and draw_insurance can redistribute the pulled
/// funds.
const IX_PULL_INSURANCE: u8 = 7;
const IX_MINT_REWARD: u8 = 8;
const IX_SET_MARKET_REWARDS: u8 = 9;
const IX_TRANSFER_MINT_AUTHORITY: u8 = 10;
const IX_ACTIVATE_LIVE: u8 = 11;
const IX_INIT_RISK_VAULT: u8 = 12;
const IX_REGISTER_RISK_VAULT_AUTHORITY: u8 = 13;
const IX_RISK_DEPOSIT: u8 = 14;
const IX_RISK_REQUEST_WITHDRAW: u8 = 15;
const IX_RISK_WITHDRAW: u8 = 16;
const IX_SYNC_RISK_VAULT: u8 = 17;
const IX_RISK_CLAIM_REWARDS: u8 = 18;
const IX_INIT_PERCOLATOR_MARKET: u8 = 19;
const IX_PERCOLATOR_ADMIN: u8 = 20;
const IX_INIT_GENESIS_BOOTSTRAP: u8 = 21;
const IX_GENESIS_DEPOSIT: u8 = 22;
const IX_GENESIS_WITHDRAW: u8 = 23;
const IX_GENESIS_MINT_REWARD: u8 = 24;
const IX_FINALIZE_GENESIS: u8 = 25;
const IX_DRAW_GENESIS_SURPLUS: u8 = 26;
const IX_KICKSTART_GENESIS_MARKET: u8 = 27;
const IX_RECOVER_GENESIS_MARKET: u8 = 28;
const IX_INIT_GENESIS_DISTRIBUTION: u8 = 29;
const IX_VOTE_GENESIS_DISTRIBUTION: u8 = 30;
const IX_APPROVE_BUILDER: u8 = 31;

/// Percolator instruction tags we CPI into
const PERC_IX_INIT_MARKET: u8 = 0;
const PERC_IX_CLOSE_SLAB: u8 = 13;
const PERC_IX_RESOLVE_MARKET: u8 = 19;
const PERC_IX_UPDATE_AUTHORITY: u8 = 32;
const PERC_IX_UPDATE_INSURANCE_POLICY: u8 = 33;
const PERC_IX_CONFIGURE_HYBRID_ORACLE: u8 = 34;
const PERC_IX_CONFIGURE_EWMA_MARK: u8 = 35;
const PERC_IX_UPDATE_LIQUIDATION_FEE_POLICY: u8 = 37;
const PERC_IX_CONFIGURE_PERMISSIONLESS_RESOLVE: u8 = 38;
const PERC_IX_UPDATE_ASSET_LIFECYCLE: u8 = 40;
const PERC_IX_WITHDRAW_INSURANCE: u8 = 41;
const PERC_IX_UPDATE_MAINTENANCE_FEE_POLICY: u8 = 49;
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
const PERC_IX_TOP_UP_BACKING_BUCKET: u8 = 24;
const PERC_IX_WITHDRAW_INSURANCE_LIMITED: u8 = 23;
const PERC_IX_WITHDRAW_BACKING_BUCKET: u8 = 50;
const PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS: u8 = 52;
const PERC_IX_SYNC_BACKING_DOMAIN_LEDGER: u8 = 53;
const PERC_IX_SYNC_INSURANCE_LEDGER: u8 = 54;
const PERC_IX_UPDATE_BACKING_FEE_POLICY: u8 = 51;
const PERC_IX_UPDATE_TRADE_FEE_POLICY: u8 = 55;
const PERC_IX_UPDATE_FEE_REDIRECT_POLICY: u8 = 58;
const PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY: u8 = 59;
const PERC_IX_UPDATE_BASE_UNIT_MINTS: u8 = 60;
const PERC_IX_CONFIGURE_AUTH_MARK: u8 = 62;
const PERC_IX_WITHDRAW_INSURANCE_DOMAIN: u8 = 57;
const PERC_AUTHORITY_INSURANCE: u8 = 2;
const PERC_AUTHORITY_BACKING_BUCKET: u8 = 3;
const PERC_AUTHORITY_INSURANCE_OPERATOR: u8 = 4;

const RISK_KIND_INSURANCE: u8 = 0;
const RISK_KIND_BACKING: u8 = 1;

const GENESIS_RECOVER_INSURANCE_LIMITED: u8 = 0;
const GENESIS_RECOVER_BACKING: u8 = 1;
const GENESIS_RECOVER_BACKING_EARNINGS: u8 = 2;
const GENESIS_RECOVER_INSURANCE_TERMINAL: u8 = 3;
const GENESIS_RECOVER_INSURANCE_DOMAIN: u8 = 4;

// ============================================================================
// Account sizes
// ============================================================================

/// MarketRewardsCfg: 8 + 32 + 32 + 32 + 8 + 8 + 8 + 16 + 8 + 8 = 160
const MRC_SIZE: usize = 8 + 32 + 32 + 32 + 8 + 8 + 8 + 16 + 8 + 8;
/// StakePosition: 8 + 8 + 8 + 16 + 8 = 48
const SP_SIZE: usize = 8 + 8 + 8 + 16 + 8;
/// CoinConfig: 8 + 32 + 8 + 8 + 8 + 1 + 7 = 72
const COIN_CFG_SIZE: usize = 8 + 32 + 8 + 8 + 8 + 1 + 7;
/// RiskVaultCfg: fixed-layout state for insurance/backing depositor accounting.
const RISK_VAULT_SIZE: usize = 352;
/// RiskPosition: fixed-layout state for a depositor in one RiskVaultCfg.
const RISK_POSITION_SIZE: usize = 136;
/// GenesisConfig: base-token bootstrap deposits, vote units, and fixed supply.
const GENESIS_CFG_SIZE: usize = 176;
/// GenesisPosition: per-user base-unit deposit and voting weight.
const GENESIS_POSITION_SIZE: usize = 72;
/// GenesisDistribution: vote-approved mint allocation item.
const GENESIS_DISTRIBUTION_SIZE: usize = 112;
/// GenesisDistributionVote: one voter's weight on one allocation item.
const GENESIS_DISTRIBUTION_VOTE_SIZE: usize = 88;
/// BuilderApproval: governed registry entry for approved builder code.
const BUILDER_APPROVAL_SIZE: usize = 152;

// Discriminators
const MRC_DISC: [u8; 8] = *b"MRC_V003";
const SP_DISC: [u8; 8] = *b"SP__INIT";
const COIN_CFG_DISC: [u8; 8] = *b"CCFGV002";
const RISK_VAULT_DISC: [u8; 8] = *b"RVLT0001";
const RISK_POSITION_DISC: [u8; 8] = *b"RPOS0001";
const GENESIS_CFG_DISC: [u8; 8] = *b"GENCFG01";
const GENESIS_POSITION_DISC: [u8; 8] = *b"GENPOS01";
const GENESIS_DISTRIBUTION_DISC: [u8; 8] = *b"GENDIST1";
const GENESIS_DISTRIBUTION_VOTE_DISC: [u8; 8] = *b"GENDVOTE";
const BUILDER_APPROVAL_DISC: [u8; 8] = *b"BLDAPP01";

const PHASE_BOOTSTRAP: u8 = 0;
const PHASE_LIVE: u8 = 1;

// ============================================================================
// PDA seeds
// ============================================================================

fn mrc_seeds(market_slab: &Pubkey) -> [&[u8]; 2] {
    [b"mrc", market_slab.as_ref()]
}

fn sp_seeds<'a>(market_slab: &'a Pubkey, user: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"sp", market_slab.as_ref(), user.as_ref()]
}

fn mint_authority_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_mint_authority", coin_mint.as_ref()]
}

fn coin_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_cfg", coin_mint.as_ref()]
}

fn market_admin_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"percolator_market_admin", coin_mint.as_ref()]
}

fn genesis_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"genesis_cfg", coin_mint.as_ref()]
}

fn genesis_vault_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"genesis_vault", coin_mint.as_ref()]
}

fn genesis_position_seeds<'a>(genesis_cfg: &'a Pubkey, user: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"genesis_position", genesis_cfg.as_ref(), user.as_ref()]
}

fn genesis_distribution_seeds<'a>(
    genesis_cfg: &'a Pubkey,
    proposal_id: &'a [u8; 8],
) -> [&'a [u8]; 3] {
    [b"genesis_distribution", genesis_cfg.as_ref(), proposal_id]
}

fn genesis_distribution_vote_seeds<'a>(proposal: &'a Pubkey, voter: &'a Pubkey) -> [&'a [u8]; 3] {
    [
        b"genesis_distribution_vote",
        proposal.as_ref(),
        voter.as_ref(),
    ]
}

fn builder_approval_seeds<'a>(
    coin_mint: &'a Pubkey,
    builder_program: &'a Pubkey,
    code_hash: &'a [u8; 32],
) -> [&'a [u8]; 4] {
    [
        b"builder_approval",
        coin_mint.as_ref(),
        builder_program.as_ref(),
        code_hash,
    ]
}

fn stake_vault_seeds(market_slab: &Pubkey) -> [&[u8]; 2] {
    [b"stake_vault", market_slab.as_ref()]
}

fn risk_vault_seeds<'a>(market_slab: &'a Pubkey, suffix: &'a [u8; 2]) -> [&'a [u8]; 3] {
    [b"risk_vault", market_slab.as_ref(), suffix]
}

fn risk_token_vault_seeds<'a>(market_slab: &'a Pubkey, suffix: &'a [u8; 2]) -> [&'a [u8]; 3] {
    [b"risk_token_vault", market_slab.as_ref(), suffix]
}

fn risk_ledger_seeds<'a>(market_slab: &'a Pubkey, suffix: &'a [u8; 2]) -> [&'a [u8]; 3] {
    [b"risk_ledger", market_slab.as_ref(), suffix]
}

fn risk_position_seeds<'a>(risk_vault: &'a Pubkey, user: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"risk_position", risk_vault.as_ref(), user.as_ref()]
}

// ============================================================================
// Instruction deserialization
// ============================================================================

fn read_u8(data: &mut &[u8]) -> Result<u8, ProgramError> {
    if data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let val = data[0];
    *data = &data[1..];
    Ok(val)
}

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let val = u64::from_le_bytes(data[..8].try_into().unwrap());
    *data = &data[8..];
    Ok(val)
}

fn read_u16(data: &mut &[u8]) -> Result<u16, ProgramError> {
    if data.len() < 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let val = u16::from_le_bytes(data[..2].try_into().unwrap());
    *data = &data[2..];
    Ok(val)
}

fn read_optional_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.is_empty() {
        return Ok(0);
    }
    let value = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(value)
}

fn read_bytes32(data: &mut &[u8]) -> Result<[u8; 32], ProgramError> {
    if data.len() < 32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let value = data[..32].try_into().unwrap();
    *data = &data[32..];
    Ok(value)
}

// ============================================================================
// CoinConfig — shared across all markets using the same COIN mint
// ============================================================================

struct CoinConfig {
    authority: Pubkey,
    bootstrap_start_slot: u64,
    bootstrap_delay_slots: u64,
    live_slot: u64,
    phase: u8,
}

impl CoinConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < COIN_CFG_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != COIN_CFG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = Pubkey::new_from_array(data[8..40].try_into().unwrap());
        let bootstrap_start_slot = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let bootstrap_delay_slots = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let live_slot = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let phase = data[64];
        match phase {
            PHASE_BOOTSTRAP | PHASE_LIVE => {}
            _ => return Err(ProgramError::InvalidAccountData),
        }
        Ok(Self {
            authority,
            bootstrap_start_slot,
            bootstrap_delay_slots,
            live_slot,
            phase,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&COIN_CFG_DISC);
        data[8..40].copy_from_slice(self.authority.as_ref());
        data[40..48].copy_from_slice(&self.bootstrap_start_slot.to_le_bytes());
        data[48..56].copy_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
        data[56..64].copy_from_slice(&self.live_slot.to_le_bytes());
        data[64] = self.phase;
        data[65..COIN_CFG_SIZE].fill(0);
    }

    fn is_live(&self) -> bool {
        self.phase == PHASE_LIVE
    }
}

// ============================================================================
// MarketRewardsCfg — per-market staking and reward configuration
// ============================================================================

struct MarketRewardsCfg {
    market_slab: Pubkey,           // [8..40]
    coin_mint: Pubkey,             // [40..72]
    collateral_mint: Pubkey,       // [72..104]
    n_per_epoch: u64,              // [104..112] COIN emitted per epoch to stakers
    epoch_slots: u64,              // [112..120] minimum lockup / reward period
    market_start_slot: u64,        // [120..128] from slab
    reward_per_token_stored: u128, // [128..144] accumulator (FP)
    last_update_slot: u64,         // [144..152]
    total_staked: u64,             // [152..160]
}

impl MarketRewardsCfg {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < MRC_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != MRC_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut off = 8;
        let market_slab = Pubkey::new_from_array(data[off..off + 32].try_into().unwrap());
        off += 32;
        let coin_mint = Pubkey::new_from_array(data[off..off + 32].try_into().unwrap());
        off += 32;
        let collateral_mint = Pubkey::new_from_array(data[off..off + 32].try_into().unwrap());
        off += 32;
        let n_per_epoch = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        off += 8;
        let epoch_slots = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        off += 8;
        let market_start_slot = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        off += 8;
        let reward_per_token_stored = u128::from_le_bytes(data[off..off + 16].try_into().unwrap());
        off += 16;
        let last_update_slot = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        off += 8;
        let total_staked = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        Ok(Self {
            market_slab,
            coin_mint,
            collateral_mint,
            n_per_epoch,
            epoch_slots,
            market_start_slot,
            reward_per_token_stored,
            last_update_slot,
            total_staked,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&MRC_DISC);
        let mut off = 8;
        data[off..off + 32].copy_from_slice(self.market_slab.as_ref());
        off += 32;
        data[off..off + 32].copy_from_slice(self.coin_mint.as_ref());
        off += 32;
        data[off..off + 32].copy_from_slice(self.collateral_mint.as_ref());
        off += 32;
        data[off..off + 8].copy_from_slice(&self.n_per_epoch.to_le_bytes());
        off += 8;
        data[off..off + 8].copy_from_slice(&self.epoch_slots.to_le_bytes());
        off += 8;
        data[off..off + 8].copy_from_slice(&self.market_start_slot.to_le_bytes());
        off += 8;
        data[off..off + 16].copy_from_slice(&self.reward_per_token_stored.to_le_bytes());
        off += 16;
        data[off..off + 8].copy_from_slice(&self.last_update_slot.to_le_bytes());
        off += 8;
        data[off..off + 8].copy_from_slice(&self.total_staked.to_le_bytes());
    }
}

// ============================================================================
// StakePosition — per (market, user) staking state
// ============================================================================

struct StakePosition {
    amount: u64,                 // [8..16]
    deposit_slot: u64,           // [16..24]
    reward_per_token_paid: u128, // [24..40]
    pending_rewards: u64,        // [40..48]
}

impl StakePosition {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < SP_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != SP_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let amount = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let deposit_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let reward_per_token_paid = u128::from_le_bytes(data[24..40].try_into().unwrap());
        let pending_rewards = u64::from_le_bytes(data[40..48].try_into().unwrap());
        Ok(Self {
            amount,
            deposit_slot,
            reward_per_token_paid,
            pending_rewards,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&SP_DISC);
        data[8..16].copy_from_slice(&self.amount.to_le_bytes());
        data[16..24].copy_from_slice(&self.deposit_slot.to_le_bytes());
        data[24..40].copy_from_slice(&self.reward_per_token_paid.to_le_bytes());
        data[40..48].copy_from_slice(&self.pending_rewards.to_le_bytes());
    }
}

// ============================================================================
// RiskVaultCfg / RiskPosition — external risk depositor subledger state
// ============================================================================

struct RiskVaultCfg {
    kind: u8,
    domain: u8,
    market_slab: Pubkey,
    coin_mint: Pubkey,
    collateral_mint: Pubkey,
    token_vault: Pubkey,
    engine_ledger: Pubkey,
    lockup_slots: u64,
    withdraw_delay_slots: u64,
    total_deposited: u64,
    total_withdrawn: u64,
    total_shares: u64,
    reward_per_share_stored: u128,
    loss_per_share_stored: u128,
    recovery_per_share_stored: u128,
    last_reward_counter: u128,
    last_loss_counter: u128,
    last_recovery_counter: u128,
    dao_fee_bps: u16,
    fee_destination: Pubkey,
}

impl RiskVaultCfg {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < RISK_VAULT_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != RISK_VAULT_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let kind = data[8];
        let domain = data[9];
        validate_risk_kind(kind)?;
        let market_slab = Pubkey::new_from_array(data[16..48].try_into().unwrap());
        let coin_mint = Pubkey::new_from_array(data[48..80].try_into().unwrap());
        let collateral_mint = Pubkey::new_from_array(data[80..112].try_into().unwrap());
        let token_vault = Pubkey::new_from_array(data[112..144].try_into().unwrap());
        let engine_ledger = Pubkey::new_from_array(data[144..176].try_into().unwrap());
        let lockup_slots = u64::from_le_bytes(data[176..184].try_into().unwrap());
        let withdraw_delay_slots = u64::from_le_bytes(data[184..192].try_into().unwrap());
        let total_deposited = u64::from_le_bytes(data[192..200].try_into().unwrap());
        let total_withdrawn = u64::from_le_bytes(data[200..208].try_into().unwrap());
        let total_shares = u64::from_le_bytes(data[208..216].try_into().unwrap());
        let reward_per_share_stored = u128::from_le_bytes(data[216..232].try_into().unwrap());
        let loss_per_share_stored = u128::from_le_bytes(data[232..248].try_into().unwrap());
        let recovery_per_share_stored = u128::from_le_bytes(data[248..264].try_into().unwrap());
        let last_reward_counter = u128::from_le_bytes(data[264..280].try_into().unwrap());
        let last_loss_counter = u128::from_le_bytes(data[280..296].try_into().unwrap());
        let last_recovery_counter = u128::from_le_bytes(data[296..312].try_into().unwrap());
        let dao_fee_bps = u16::from_le_bytes(data[312..314].try_into().unwrap());
        let fee_destination = Pubkey::new_from_array(data[320..352].try_into().unwrap());
        if dao_fee_bps > 10_000 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            kind,
            domain,
            market_slab,
            coin_mint,
            collateral_mint,
            token_vault,
            engine_ledger,
            lockup_slots,
            withdraw_delay_slots,
            total_deposited,
            total_withdrawn,
            total_shares,
            reward_per_share_stored,
            loss_per_share_stored,
            recovery_per_share_stored,
            last_reward_counter,
            last_loss_counter,
            last_recovery_counter,
            dao_fee_bps,
            fee_destination,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&RISK_VAULT_DISC);
        data[8] = self.kind;
        data[9] = self.domain;
        data[10..16].fill(0);
        data[16..48].copy_from_slice(self.market_slab.as_ref());
        data[48..80].copy_from_slice(self.coin_mint.as_ref());
        data[80..112].copy_from_slice(self.collateral_mint.as_ref());
        data[112..144].copy_from_slice(self.token_vault.as_ref());
        data[144..176].copy_from_slice(self.engine_ledger.as_ref());
        data[176..184].copy_from_slice(&self.lockup_slots.to_le_bytes());
        data[184..192].copy_from_slice(&self.withdraw_delay_slots.to_le_bytes());
        data[192..200].copy_from_slice(&self.total_deposited.to_le_bytes());
        data[200..208].copy_from_slice(&self.total_withdrawn.to_le_bytes());
        data[208..216].copy_from_slice(&self.total_shares.to_le_bytes());
        data[216..232].copy_from_slice(&self.reward_per_share_stored.to_le_bytes());
        data[232..248].copy_from_slice(&self.loss_per_share_stored.to_le_bytes());
        data[248..264].copy_from_slice(&self.recovery_per_share_stored.to_le_bytes());
        data[264..280].copy_from_slice(&self.last_reward_counter.to_le_bytes());
        data[280..296].copy_from_slice(&self.last_loss_counter.to_le_bytes());
        data[296..312].copy_from_slice(&self.last_recovery_counter.to_le_bytes());
        data[312..314].copy_from_slice(&self.dao_fee_bps.to_le_bytes());
        data[314..320].fill(0);
        data[320..352].copy_from_slice(self.fee_destination.as_ref());
    }
}

struct RiskPosition {
    owner: Pubkey,
    shares: u64,
    deposit_slot: u64,
    pending_withdraw_shares: u64,
    withdraw_request_slot: u64,
    reward_per_share_paid: u128,
    loss_per_share_paid: u128,
    recovery_per_share_paid: u128,
    pending_rewards: u64,
    pending_losses: u64,
}

impl RiskPosition {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < RISK_POSITION_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != RISK_POSITION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            owner: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            shares: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            deposit_slot: u64::from_le_bytes(data[48..56].try_into().unwrap()),
            pending_withdraw_shares: u64::from_le_bytes(data[56..64].try_into().unwrap()),
            withdraw_request_slot: u64::from_le_bytes(data[64..72].try_into().unwrap()),
            reward_per_share_paid: u128::from_le_bytes(data[72..88].try_into().unwrap()),
            loss_per_share_paid: u128::from_le_bytes(data[88..104].try_into().unwrap()),
            recovery_per_share_paid: u128::from_le_bytes(data[104..120].try_into().unwrap()),
            pending_rewards: u64::from_le_bytes(data[120..128].try_into().unwrap()),
            pending_losses: u64::from_le_bytes(data[128..136].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&RISK_POSITION_DISC);
        data[8..40].copy_from_slice(self.owner.as_ref());
        data[40..48].copy_from_slice(&self.shares.to_le_bytes());
        data[48..56].copy_from_slice(&self.deposit_slot.to_le_bytes());
        data[56..64].copy_from_slice(&self.pending_withdraw_shares.to_le_bytes());
        data[64..72].copy_from_slice(&self.withdraw_request_slot.to_le_bytes());
        data[72..88].copy_from_slice(&self.reward_per_share_paid.to_le_bytes());
        data[88..104].copy_from_slice(&self.loss_per_share_paid.to_le_bytes());
        data[104..120].copy_from_slice(&self.recovery_per_share_paid.to_le_bytes());
        data[120..128].copy_from_slice(&self.pending_rewards.to_le_bytes());
        data[128..136].copy_from_slice(&self.pending_losses.to_le_bytes());
    }
}

// ============================================================================
// GenesisConfig / GenesisPosition — bootstrap vote and principal ledger
// ============================================================================

struct GenesisConfig {
    coin_mint: Pubkey,
    base_mint: Pubkey,
    token_vault: Pubkey,
    total_deposited: u64,
    total_withdrawn: u64,
    reward_supply: u64,
    minted_supply: u64,
    insurance_principal_x2: u128,
    backing_principal_x2: u128,
    finalized: u8,
    kicked: u8,
}

impl GenesisConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_CFG_SIZE || data[..8] != GENESIS_CFG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let finalized = data[168];
        let kicked = data[169];
        if finalized > 1 || kicked > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            base_mint: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            token_vault: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            total_deposited: u64::from_le_bytes(data[104..112].try_into().unwrap()),
            total_withdrawn: u64::from_le_bytes(data[112..120].try_into().unwrap()),
            reward_supply: u64::from_le_bytes(data[120..128].try_into().unwrap()),
            minted_supply: u64::from_le_bytes(data[128..136].try_into().unwrap()),
            insurance_principal_x2: u128::from_le_bytes(data[136..152].try_into().unwrap()),
            backing_principal_x2: u128::from_le_bytes(data[152..168].try_into().unwrap()),
            finalized,
            kicked,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_CFG_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.base_mint.as_ref());
        data[72..104].copy_from_slice(self.token_vault.as_ref());
        data[104..112].copy_from_slice(&self.total_deposited.to_le_bytes());
        data[112..120].copy_from_slice(&self.total_withdrawn.to_le_bytes());
        data[120..128].copy_from_slice(&self.reward_supply.to_le_bytes());
        data[128..136].copy_from_slice(&self.minted_supply.to_le_bytes());
        data[136..152].copy_from_slice(&self.insurance_principal_x2.to_le_bytes());
        data[152..168].copy_from_slice(&self.backing_principal_x2.to_le_bytes());
        data[168] = self.finalized;
        data[169] = self.kicked;
        data[170..GENESIS_CFG_SIZE].fill(0);
    }

    fn is_finalized(&self) -> bool {
        self.finalized == 1
    }

    fn is_kicked(&self) -> bool {
        self.kicked == 1
    }

    fn outstanding_principal(&self) -> u64 {
        self.total_deposited.saturating_sub(self.total_withdrawn)
    }
}

struct GenesisPosition {
    owner: Pubkey,
    amount: u64,
    withdrawn: u64,
    vote_units: u64,
    allocated_vote_units: u64,
}

impl GenesisPosition {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_POSITION_SIZE || data[..8] != GENESIS_POSITION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            owner: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            amount: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            withdrawn: u64::from_le_bytes(data[48..56].try_into().unwrap()),
            vote_units: u64::from_le_bytes(data[56..64].try_into().unwrap()),
            allocated_vote_units: u64::from_le_bytes(data[64..72].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_POSITION_DISC);
        data[8..40].copy_from_slice(self.owner.as_ref());
        data[40..48].copy_from_slice(&self.amount.to_le_bytes());
        data[48..56].copy_from_slice(&self.withdrawn.to_le_bytes());
        data[56..64].copy_from_slice(&self.vote_units.to_le_bytes());
        data[64..72].copy_from_slice(&self.allocated_vote_units.to_le_bytes());
    }
}

struct GenesisDistribution {
    genesis_cfg: Pubkey,
    destination: Pubkey,
    proposal_id: u64,
    amount: u64,
    yes_votes: u64,
    no_votes: u64,
    executed: u8,
}

impl GenesisDistribution {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_DISTRIBUTION_SIZE || data[..8] != GENESIS_DISTRIBUTION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let executed = data[104];
        if executed > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            genesis_cfg: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            destination: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            proposal_id: u64::from_le_bytes(data[72..80].try_into().unwrap()),
            amount: u64::from_le_bytes(data[80..88].try_into().unwrap()),
            yes_votes: u64::from_le_bytes(data[88..96].try_into().unwrap()),
            no_votes: u64::from_le_bytes(data[96..104].try_into().unwrap()),
            executed,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_DISTRIBUTION_DISC);
        data[8..40].copy_from_slice(self.genesis_cfg.as_ref());
        data[40..72].copy_from_slice(self.destination.as_ref());
        data[72..80].copy_from_slice(&self.proposal_id.to_le_bytes());
        data[80..88].copy_from_slice(&self.amount.to_le_bytes());
        data[88..96].copy_from_slice(&self.yes_votes.to_le_bytes());
        data[96..104].copy_from_slice(&self.no_votes.to_le_bytes());
        data[104] = self.executed;
        data[105..GENESIS_DISTRIBUTION_SIZE].fill(0);
    }

    fn is_executed(&self) -> bool {
        self.executed == 1
    }
}

struct GenesisDistributionVote {
    proposal: Pubkey,
    voter: Pubkey,
    weight: u64,
    support: u8,
}

impl GenesisDistributionVote {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_DISTRIBUTION_VOTE_SIZE
            || data[..8] != GENESIS_DISTRIBUTION_VOTE_DISC
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let support = data[80];
        if support > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            proposal: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            voter: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            weight: u64::from_le_bytes(data[72..80].try_into().unwrap()),
            support,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_DISTRIBUTION_VOTE_DISC);
        data[8..40].copy_from_slice(self.proposal.as_ref());
        data[40..72].copy_from_slice(self.voter.as_ref());
        data[72..80].copy_from_slice(&self.weight.to_le_bytes());
        data[80] = self.support;
        data[81..GENESIS_DISTRIBUTION_VOTE_SIZE].fill(0);
    }
}

struct BuilderApproval {
    coin_mint: Pubkey,
    builder_program: Pubkey,
    code_hash: [u8; 32],
    terms_hash: [u8; 32],
    approved_slot: u64,
    enabled: u8,
}

impl BuilderApproval {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < BUILDER_APPROVAL_SIZE || data[..8] != BUILDER_APPROVAL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let enabled = data[144];
        if enabled > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            builder_program: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            code_hash: data[72..104].try_into().unwrap(),
            terms_hash: data[104..136].try_into().unwrap(),
            approved_slot: u64::from_le_bytes(data[136..144].try_into().unwrap()),
            enabled,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&BUILDER_APPROVAL_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.builder_program.as_ref());
        data[72..104].copy_from_slice(&self.code_hash);
        data[104..136].copy_from_slice(&self.terms_hash);
        data[136..144].copy_from_slice(&self.approved_slot.to_le_bytes());
        data[144] = self.enabled;
        data[145..BUILDER_APPROVAL_SIZE].fill(0);
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn create_pda_account<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
) -> ProgramResult {
    create_pda_account_with_owner(
        payer,
        target,
        system_program,
        program_id,
        seeds,
        size,
        program_id,
    )
}

fn create_pda_account_with_owner<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    pda_program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
    owner: &Pubkey,
) -> ProgramResult {
    let (expected, bump) = Pubkey::find_program_address(seeds, pda_program_id);
    if *target.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(size);
    let mut seeds_with_bump: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::from(seeds);
    let bump_bytes = [bump];
    seeds_with_bump.push(&bump_bytes);
    invoke_signed(
        &system_instruction::create_account(payer.key, target.key, lamports, size as u64, owner),
        &[payer.clone(), target.clone(), system_program.clone()],
        &[&seeds_with_bump],
    )
}

fn validate_risk_kind(kind: u8) -> ProgramResult {
    match kind {
        RISK_KIND_INSURANCE | RISK_KIND_BACKING => Ok(()),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn risk_suffix(kind: u8, domain: u8) -> Result<[u8; 2], ProgramError> {
    validate_risk_kind(kind)?;
    if kind == RISK_KIND_INSURANCE && domain != 0 {
        msg!("main insurance risk vault must use domain 0");
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok([kind, domain])
}

fn verify_risk_vault_pda(
    risk_vault_account: &AccountInfo,
    cfg: &RiskVaultCfg,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    if risk_vault_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let seeds = risk_vault_seeds(&cfg.market_slab, &suffix);
    let (expected, bump) = Pubkey::find_program_address(&seeds, program_id);
    if *risk_vault_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

fn verify_risk_cfg_accounts<'a>(
    program_id: &Pubkey,
    cfg: &RiskVaultCfg,
    risk_vault_account: &AccountInfo<'a>,
    market_slab: &AccountInfo<'a>,
    token_vault: &AccountInfo<'a>,
    engine_ledger: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
) -> Result<u8, ProgramError> {
    let bump = verify_risk_vault_pda(risk_vault_account, cfg, program_id)?;
    if *market_slab.key != cfg.market_slab || *token_vault.key != cfg.token_vault {
        return Err(ProgramError::InvalidAccountData);
    }
    if *engine_ledger.key != cfg.engine_ledger {
        return Err(ProgramError::InvalidAccountData);
    }
    verify_token_program(token_program)?;
    validate_token_account(token_vault, &cfg.collateral_mint, risk_vault_account.key)?;
    if market_slab.owner != &percolator_abi::id() {
        return Err(ProgramError::IllegalOwner);
    }
    if engine_ledger.owner != &percolator_abi::id() {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(bump)
}

fn risk_position_for_user<'a>(
    program_id: &Pubkey,
    user: &AccountInfo<'a>,
    risk_vault_account: &AccountInfo<'a>,
    position_account: &AccountInfo<'a>,
    system_program: Option<&AccountInfo<'a>>,
) -> Result<RiskPosition, ProgramError> {
    let seeds = risk_position_seeds(risk_vault_account.key, user.key);
    let (expected_position, _) = Pubkey::find_program_address(&seeds, program_id);
    if *position_account.key != expected_position {
        return Err(ProgramError::InvalidSeeds);
    }
    if position_account.data_len() == 0 || position_account.lamports() == 0 {
        let system_program = system_program.ok_or(ProgramError::InvalidAccountData)?;
        create_pda_account(
            user,
            position_account,
            system_program,
            program_id,
            &seeds,
            RISK_POSITION_SIZE,
        )?;
        return Ok(RiskPosition {
            owner: *user.key,
            shares: 0,
            deposit_slot: 0,
            pending_withdraw_shares: 0,
            withdraw_request_slot: 0,
            reward_per_share_paid: 0,
            loss_per_share_paid: 0,
            recovery_per_share_paid: 0,
            pending_rewards: 0,
            pending_losses: 0,
        });
    }
    if position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let data = position_account.try_borrow_data()?;
    let pos = RiskPosition::deserialize(&data)?;
    if pos.owner != *user.key {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(pos)
}

fn checked_add_per_share(accumulator: &mut u128, amount: u128, total_shares: u64) -> ProgramResult {
    if amount == 0 || total_shares == 0 {
        return Ok(());
    }
    let (lo, hi) = mul_u128_wide(amount, FP);
    let delta = div_u256_by_u128(lo, hi, total_shares as u128);
    *accumulator = accumulator
        .checked_add(delta)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    Ok(())
}

fn fp_mul_to_u64(amount: u64, per_share_delta: u128) -> u64 {
    let (lo, hi) = mul_u128_wide(amount as u128, per_share_delta);
    let value = (lo >> 64) | (hi << 64);
    core::cmp::min(value, u64::MAX as u128) as u64
}

fn settle_risk_position(pos: &mut RiskPosition, cfg: &RiskVaultCfg) {
    if pos.shares != 0 {
        let reward_delta = cfg
            .reward_per_share_stored
            .saturating_sub(pos.reward_per_share_paid);
        let loss_delta = cfg
            .loss_per_share_stored
            .saturating_sub(pos.loss_per_share_paid);
        let recovery_delta = cfg
            .recovery_per_share_stored
            .saturating_sub(pos.recovery_per_share_paid);
        let earned = fp_mul_to_u64(pos.shares, reward_delta);
        let lost = fp_mul_to_u64(pos.shares, loss_delta);
        let recovered = fp_mul_to_u64(pos.shares, recovery_delta);
        pos.pending_rewards = pos.pending_rewards.saturating_add(earned);
        pos.pending_losses = core::cmp::min(pos.shares, pos.pending_losses.saturating_add(lost));
        pos.pending_losses = pos.pending_losses.saturating_sub(recovered);
    }
    pos.reward_per_share_paid = cfg.reward_per_share_stored;
    pos.loss_per_share_paid = cfg.loss_per_share_stored;
    pos.recovery_per_share_paid = cfg.recovery_per_share_stored;
}

fn available_risk_shares(pos: &RiskPosition) -> u64 {
    pos.shares
        .saturating_sub(pos.pending_losses)
        .saturating_sub(pos.pending_withdraw_shares)
}

fn verify_token_program(token_program: &AccountInfo) -> ProgramResult {
    if *token_program.key != spl_token::ID {
        msg!("Expected SPL Token program");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

fn load_token_account(account: &AccountInfo) -> Result<spl_token::state::Account, ProgramError> {
    if account.owner != &spl_token::ID {
        msg!("Token account must be owned by SPL Token");
        return Err(ProgramError::IllegalOwner);
    }
    let data = account.try_borrow_data()?;
    spl_token::state::Account::unpack(&data).map_err(|_| ProgramError::InvalidAccountData)
}

fn validate_token_account(
    account: &AccountInfo,
    expected_mint: &Pubkey,
    expected_owner: &Pubkey,
) -> ProgramResult {
    let token = load_token_account(account)?;
    if token.mint != *expected_mint || token.owner != *expected_owner {
        msg!("Token account mint/owner mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn verify_percolator_program(percolator_program: &AccountInfo) -> ProgramResult {
    if *percolator_program.key != percolator_abi::id() {
        msg!("Unexpected Percolator program id");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

fn verify_market_admin_pda(
    market_admin: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    let seeds = market_admin_seeds(coin_mint);
    let (expected, bump) = Pubkey::find_program_address(&seeds, program_id);
    if *market_admin.key != expected {
        msg!("Percolator market admin PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

fn ensure_market_admin_account<'a>(
    payer: &AccountInfo<'a>,
    market_admin: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let seeds = market_admin_seeds(coin_mint);
    let (expected, _) = Pubkey::find_program_address(&seeds, program_id);
    if *market_admin.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if market_admin.lamports() == 0 {
        create_pda_account_with_owner(
            payer,
            market_admin,
            system_program,
            program_id,
            &seeds,
            0,
            &solana_program::system_program::ID,
        )?;
    } else if market_admin.owner != &solana_program::system_program::ID
        || market_admin.data_len() != 0
    {
        msg!("Percolator market admin PDA must be a system account");
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

fn percolator_admin_tag_allowed(tag: u8) -> bool {
    matches!(
        tag,
        PERC_IX_CLOSE_SLAB
            | PERC_IX_RESOLVE_MARKET
            | PERC_IX_UPDATE_INSURANCE_POLICY
            | PERC_IX_CONFIGURE_HYBRID_ORACLE
            | PERC_IX_CONFIGURE_EWMA_MARK
            | PERC_IX_UPDATE_LIQUIDATION_FEE_POLICY
            | PERC_IX_CONFIGURE_PERMISSIONLESS_RESOLVE
            | PERC_IX_UPDATE_ASSET_LIFECYCLE
            | PERC_IX_UPDATE_MAINTENANCE_FEE_POLICY
            | PERC_IX_UPDATE_BACKING_FEE_POLICY
            | PERC_IX_UPDATE_TRADE_FEE_POLICY
            | PERC_IX_UPDATE_FEE_REDIRECT_POLICY
            | PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY
            | PERC_IX_UPDATE_BASE_UNIT_MINTS
            | PERC_IX_CONFIGURE_AUTH_MARK
    )
}

fn account_meta_from_info(
    account: &AccountInfo,
    is_signer: bool,
) -> solana_program::instruction::AccountMeta {
    if account.is_writable {
        solana_program::instruction::AccountMeta::new(*account.key, is_signer)
    } else {
        solana_program::instruction::AccountMeta::new_readonly(*account.key, is_signer)
    }
}

fn load_percolator_market_config(
    market_slab: &AccountInfo,
    expected_collateral_mint: &Pubkey,
) -> Result<percolator_abi::MarketConfig, ProgramError> {
    if market_slab.owner != &percolator_abi::id() {
        msg!("Market slab must be owned by Percolator");
        return Err(ProgramError::IllegalOwner);
    }
    let slab_data = market_slab.try_borrow_data()?;
    let config = percolator_abi::read_market_config(&slab_data)?;
    if config.collateral_mint != expected_collateral_mint.to_bytes() {
        msg!("Percolator slab collateral mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(config)
}

fn validate_percolator_vault_accounts(
    market_slab: &AccountInfo,
    percolator_vault: &AccountInfo,
    percolator_vault_pda: &AccountInfo,
    collateral_mint: &Pubkey,
) -> ProgramResult {
    let (expected_vault_authority, _) =
        Pubkey::find_program_address(&[b"vault", market_slab.key.as_ref()], &percolator_abi::id());
    if *percolator_vault_pda.key != expected_vault_authority {
        msg!("Percolator vault authority PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(percolator_vault, collateral_mint, &expected_vault_authority)
}

fn verify_genesis_config_pda(
    genesis_cfg: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<GenesisConfig, ProgramError> {
    let (expected, _) = Pubkey::find_program_address(&genesis_cfg_seeds(coin_mint), program_id);
    if *genesis_cfg.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_cfg.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let data = genesis_cfg.try_borrow_data()?;
    let cfg = GenesisConfig::deserialize(&data)?;
    if cfg.coin_mint != *coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(cfg)
}

fn verify_genesis_vault(
    genesis_vault: &AccountInfo,
    cfg: &GenesisConfig,
    market_admin: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let (expected_vault, _) =
        Pubkey::find_program_address(&genesis_vault_seeds(&cfg.coin_mint), program_id);
    if *genesis_vault.key != expected_vault || cfg.token_vault != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(genesis_vault, &cfg.base_mint, market_admin)
}

fn genesis_recoverable_principal(
    remaining_principal: u64,
    vault_balance: u64,
    outstanding_principal: u64,
) -> Result<u64, ProgramError> {
    if remaining_principal == 0 {
        return Ok(0);
    }
    if outstanding_principal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    if vault_balance >= outstanding_principal {
        return Ok(remaining_principal);
    }
    Ok(((remaining_principal as u128)
        .checked_mul(vault_balance as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / outstanding_principal as u128) as u64)
}

fn genesis_recovery_ix_data(
    kind: u8,
    domain: u8,
    amount: u64,
) -> Result<alloc::vec::Vec<u8>, ProgramError> {
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = amount as u128;
    let mut ix_data = alloc::vec::Vec::with_capacity(18);
    match kind {
        GENESIS_RECOVER_INSURANCE_LIMITED => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE_LIMITED);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_BACKING => {
            ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_BACKING_EARNINGS => {
            ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_INSURANCE_TERMINAL => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_INSURANCE_DOMAIN => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE_DOMAIN);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    }
    Ok(ix_data)
}

/// Mint COIN tokens via PDA authority.
fn mint_coin<'a>(
    token_program: &AccountInfo<'a>,
    coin_mint: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    mint_authority: &AccountInfo<'a>,
    amount: u64,
    signer_seeds: &[&[u8]],
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let ix = spl_token::instruction::mint_to(
        token_program.key,
        coin_mint.key,
        destination.key,
        mint_authority.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &ix,
        &[
            coin_mint.clone(),
            destination.clone(),
            mint_authority.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )
}

/// Update the reward accumulator in MRC.
fn update_accumulator(cfg: &mut MarketRewardsCfg, current_slot: u64) {
    if cfg.total_staked == 0 || current_slot <= cfg.last_update_slot || cfg.epoch_slots == 0 {
        cfg.last_update_slot = current_slot;
        return;
    }
    let elapsed = current_slot - cfg.last_update_slot;
    // delta = n_per_epoch * elapsed * FP / (epoch_slots * total_staked)
    // Use u256 intermediate to avoid overflow
    let n_elapsed = (cfg.n_per_epoch as u128).saturating_mul(elapsed as u128);
    let (num_lo, num_hi) = mul_u128_wide(n_elapsed, FP);
    let denom = (cfg.epoch_slots as u128).saturating_mul(cfg.total_staked as u128);
    if denom > 0 {
        let delta = div_u256_by_u128(num_lo, num_hi, denom);
        cfg.reward_per_token_stored = cfg.reward_per_token_stored.saturating_add(delta);
    }
    cfg.last_update_slot = current_slot;
}

/// Compute earned COIN for a position, add to pending.
fn settle_pending(pos: &mut StakePosition, reward_per_token: u128) {
    if pos.amount == 0 {
        return;
    }
    let delta = reward_per_token.saturating_sub(pos.reward_per_token_paid);
    let (lo, hi) = mul_u128_wide(pos.amount as u128, delta);
    // Divide by FP (>> 64)
    let earned_u128 = (lo >> 64) | (hi << 64);
    let earned = core::cmp::min(earned_u128, u64::MAX as u128) as u64;
    pos.pending_rewards = pos.pending_rewards.saturating_add(earned);
    pos.reward_per_token_paid = reward_per_token;
}

/// Verify CoinConfig PDA and return authority.
fn load_coin_config(
    coin_cfg_account: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<CoinConfig, ProgramError> {
    let (expected_cfg, _) = Pubkey::find_program_address(&coin_cfg_seeds(coin_mint), program_id);
    if *coin_cfg_account.key != expected_cfg {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_cfg_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let cfg_data = coin_cfg_account.try_borrow_data()?;
    CoinConfig::deserialize(&cfg_data)
}

fn require_live(coin_cfg: &CoinConfig) -> ProgramResult {
    if !coin_cfg.is_live() {
        msg!("COIN bootstrap phase is not live");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn validate_governance_authority(
    authority: &AccountInfo,
    coin_mint: &Pubkey,
    rewards_program: &Pubkey,
) -> ProgramResult {
    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if authority.owner != &governance_program_id() {
        msg!("Authority must be the governance adapter PDA");
        return Err(ProgramError::IllegalOwner);
    }

    let (expected, _) = governance_authority_address(rewards_program, coin_mint);
    if *authority.key != expected {
        msg!("Governance authority PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }

    Ok(())
}

// ============================================================================
// Entrypoint
// ============================================================================

entrypoint!(process_instruction);

pub fn process_instruction<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    instruction_data: &[u8],
) -> ProgramResult {
    let mut data = instruction_data;
    let tag = read_u8(&mut data)?;

    match tag {
        IX_INIT_MARKET_REWARDS => process_init_market_rewards(program_id, accounts, &mut data),
        IX_STAKE => process_stake(program_id, accounts, &mut data),
        IX_UNSTAKE => process_unstake(program_id, accounts, &mut data),
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts, &mut data),
        IX_CLAIM_STAKE_REWARDS => process_claim_stake_rewards(program_id, accounts),
        IX_DRAW_INSURANCE => process_draw_insurance(program_id, accounts, &mut data),
        IX_REGISTER_INSURANCE_OPERATOR => process_register_insurance_operator(program_id, accounts),
        IX_PULL_INSURANCE => process_pull_insurance(program_id, accounts, &mut data),
        IX_MINT_REWARD => process_mint_reward(program_id, accounts, &mut data),
        IX_SET_MARKET_REWARDS => process_set_market_rewards(program_id, accounts, &mut data),
        IX_TRANSFER_MINT_AUTHORITY => process_transfer_mint_authority(program_id, accounts),
        IX_ACTIVATE_LIVE => process_activate_live(program_id, accounts, &mut data),
        IX_INIT_RISK_VAULT => process_init_risk_vault(program_id, accounts, &mut data),
        IX_REGISTER_RISK_VAULT_AUTHORITY => {
            process_register_risk_vault_authority(program_id, accounts, &mut data)
        }
        IX_RISK_DEPOSIT => process_risk_deposit(program_id, accounts, &mut data),
        IX_RISK_REQUEST_WITHDRAW => process_risk_request_withdraw(program_id, accounts, &mut data),
        IX_RISK_WITHDRAW => process_risk_withdraw(program_id, accounts, &mut data),
        IX_SYNC_RISK_VAULT => process_sync_risk_vault(program_id, accounts),
        IX_RISK_CLAIM_REWARDS => process_risk_claim_rewards(program_id, accounts, &mut data),
        IX_INIT_PERCOLATOR_MARKET => process_init_percolator_market(program_id, accounts, &data),
        IX_PERCOLATOR_ADMIN => process_percolator_admin(program_id, accounts, &data),
        IX_INIT_GENESIS_BOOTSTRAP => {
            process_init_genesis_bootstrap(program_id, accounts, &mut data)
        }
        IX_GENESIS_DEPOSIT => process_genesis_deposit(program_id, accounts, &mut data),
        IX_GENESIS_WITHDRAW => process_genesis_withdraw(program_id, accounts, &mut data),
        IX_GENESIS_MINT_REWARD => process_genesis_mint_reward(program_id, accounts, &mut data),
        IX_FINALIZE_GENESIS => process_finalize_genesis(program_id, accounts, &mut data),
        IX_DRAW_GENESIS_SURPLUS => process_draw_genesis_surplus(program_id, accounts, &mut data),
        IX_KICKSTART_GENESIS_MARKET => {
            process_kickstart_genesis_market(program_id, accounts, &mut data)
        }
        IX_RECOVER_GENESIS_MARKET => {
            process_recover_genesis_market(program_id, accounts, &mut data)
        }
        IX_INIT_GENESIS_DISTRIBUTION => {
            process_init_genesis_distribution(program_id, accounts, &mut data)
        }
        IX_VOTE_GENESIS_DISTRIBUTION => {
            process_vote_genesis_distribution(program_id, accounts, &mut data)
        }
        IX_APPROVE_BUILDER => process_approve_builder(program_id, accounts, &mut data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ============================================================================
// init_coin_config
// ============================================================================
// Accounts:
//   [0] payer (signer, writable)
//   [1] authority (signer, read-only governance PDA)
//   [2] coin_mint (read-only)
//   [3] coin_config PDA (writable, to create)
//   [4] system_program
//
// Data: bootstrap_delay_slots (u64, optional for legacy zero-delay callers)

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let bootstrap_delay_slots = read_optional_u64(data)?;
    let bootstrap_start_slot = Clock::get()?.slot;
    if bootstrap_start_slot
        .checked_add(bootstrap_delay_slots)
        .is_none()
    {
        msg!("bootstrap delay overflows slot range");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Validate coin_mint is a real SPL Token mint
    if coin_mint.owner != &spl_token::ID {
        msg!("COIN mint must be owned by SPL Token program");
        return Err(ProgramError::IllegalOwner);
    }
    let mint_data = coin_mint.try_borrow_data()?;
    let mint_info = spl_token::state::Mint::unpack(&mint_data)?;
    if mint_info.freeze_authority.is_some() {
        msg!("COIN mint must have freeze_authority = None");
        return Err(ProgramError::InvalidAccountData);
    }
    let (expected_mint_auth, _) =
        Pubkey::find_program_address(&mint_authority_seeds(coin_mint.key), program_id);
    match mint_info.mint_authority {
        solana_program::program_option::COption::Some(auth) if auth == expected_mint_auth => {}
        _ => {
            msg!("COIN mint_authority must be the rewards PDA");
            return Err(ProgramError::InvalidAccountData);
        }
    }
    drop(mint_data);

    // Create CoinConfig PDA (init guard)
    let seeds = coin_cfg_seeds(coin_mint.key);
    create_pda_account(
        payer,
        coin_cfg_account,
        system_program,
        program_id,
        &seeds,
        COIN_CFG_SIZE,
    )?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let (phase, live_slot) = if bootstrap_delay_slots == 0 {
        (PHASE_LIVE, bootstrap_start_slot)
    } else {
        (PHASE_BOOTSTRAP, 0)
    };
    let cfg = CoinConfig {
        authority: *authority.key,
        bootstrap_start_slot,
        bootstrap_delay_slots,
        live_slot,
        phase,
    };
    cfg.serialize(&mut cfg_data);

    Ok(())
}

// ============================================================================
// activate_live
// ============================================================================
// Accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, read-only governance PDA — must match CoinConfig.authority)
//   [2] coin_mint (read-only)
//   [3] coin_config PDA (writable)
//   [4] clock

fn process_activate_live<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let mut coin_cfg = CoinConfig::deserialize(&cfg_data)?;
    let (expected_cfg, _) =
        Pubkey::find_program_address(&coin_cfg_seeds(coin_mint.key), program_id);
    if *coin_cfg_account.key != expected_cfg {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_cfg_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if coin_cfg.is_live() {
        return Ok(());
    }

    let clock = Clock::from_account_info(clock_info)?;
    let live_after_slot = coin_cfg
        .bootstrap_start_slot
        .checked_add(coin_cfg.bootstrap_delay_slots)
        .ok_or(ProgramError::InvalidAccountData)?;
    if clock.slot < live_after_slot {
        msg!("bootstrap delay has not elapsed");
        return Err(ProgramError::InvalidInstructionData);
    }

    coin_cfg.phase = PHASE_LIVE;
    coin_cfg.live_slot = clock.slot;
    coin_cfg.serialize(&mut cfg_data);
    Ok(())
}

// ============================================================================
// init_market_rewards
// ============================================================================
// Accounts:
//   [0] payer (signer, writable)
//   [1] authority (signer, read-only governance PDA — must match CoinConfig.authority)
//   [2] market_slab (read-only)
//   [3] mrc PDA (writable, to create)
//   [4] coin_mint (read-only)
//   [5] coin_config PDA (read-only)
//   [6] collateral_mint (read-only)
//   [7] stake_vault PDA (writable, to create — SPL token account)
//   [8] token_program
//   [9] rent sysvar
//   [10] system_program
//
// Data: N (u64), epoch_slots (u64)
// Requires CoinConfig phase = live.

fn process_init_market_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;

    let n_per_epoch = read_u64(data)?;
    let epoch_slots = read_u64(data)?;

    if epoch_slots == 0 {
        msg!("epoch_slots must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    validate_governance_authority(authority, coin_mint.key, program_id)?;

    // Verify CoinConfig PDA and authority
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;

    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    // Verify market is a real Percolator market for this collateral and that
    // its admin is either legacy-burned or controlled by this COIN instance.
    let config = load_percolator_market_config(market_slab, collateral_mint.key)?;
    let (expected_market_admin, _) =
        Pubkey::find_program_address(&market_admin_seeds(coin_mint.key), program_id);
    if config.admin != [0u8; 32] && config.admin != expected_market_admin.to_bytes() {
        msg!("Percolator market admin must be burned or controlled by the COIN market-admin PDA");
        return Err(ProgramError::InvalidAccountData);
    }
    // Use current clock slot as the market start for reward tracking
    let clock_for_init = Clock::get()?;
    let market_start_slot = clock_for_init.slot;

    // Create MarketRewardsCfg PDA (init guard)
    let seeds = mrc_seeds(market_slab.key);
    create_pda_account(
        payer,
        mrc_account,
        system_program,
        program_id,
        &seeds,
        MRC_SIZE,
    )?;

    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let cfg = MarketRewardsCfg {
        market_slab: *market_slab.key,
        coin_mint: *coin_mint.key,
        collateral_mint: *collateral_mint.key,
        n_per_epoch,
        epoch_slots,
        market_start_slot,
        reward_per_token_stored: 0,
        last_update_slot: market_start_slot,
        total_staked: 0,
    };
    cfg.serialize(&mut mrc_data);
    drop(mrc_data);

    // Create stake vault — SPL token account PDA
    let vault_seeds = stake_vault_seeds(market_slab.key);
    let (expected_vault, vault_bump) = Pubkey::find_program_address(&vault_seeds, program_id);
    if *stake_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }

    let vault_signer_seeds: [&[u8]; 3] = [b"stake_vault", market_slab.key.as_ref(), &[vault_bump]];
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            stake_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[payer.clone(), stake_vault.clone(), system_program.clone()],
        &[&vault_signer_seeds],
    )?;

    // Initialize as token account — vault authority is the MRC PDA
    let (mrc_key, _) = Pubkey::find_program_address(&mrc_seeds(market_slab.key), program_id);
    let init_ix = spl_token::instruction::initialize_account2(
        &spl_token::ID,
        stake_vault.key,
        collateral_mint.key,
        &mrc_key,
    )?;
    invoke(
        &init_ix,
        &[
            stake_vault.clone(),
            collateral_mint.clone(),
            rent_sysvar.clone(),
            token_program.clone(),
        ],
    )?;

    Ok(())
}

// ============================================================================
// stake
// ============================================================================
// Accounts:
//   [0] user (signer)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] user_collateral_ata (writable)
//   [4] stake_vault (writable)
//   [5] stake_position PDA (writable)
//   [6] token_program
//   [7] system_program
//   [8] clock
//
// Data: amount (u64)

fn process_stake<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }

    // Verify stake vault
    let (expected_vault, _) =
        Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    verify_token_program(token_program)?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load or create StakePosition
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut pos = if sp_account.data_len() == 0 || sp_account.lamports() == 0 {
        // First stake (or re-stake after full withdrawal closed the account)
        drop(mrc_data); // release borrow for CPI
        create_pda_account(
            user,
            sp_account,
            system_program,
            program_id,
            &sp_seeds_arr,
            SP_SIZE,
        )?;
        mrc_data = mrc_account.try_borrow_mut_data()?;
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        sp_data[..8].copy_from_slice(&SP_DISC);
        sp_data[8..SP_SIZE].fill(0);
        drop(sp_data);
        StakePosition {
            amount: 0,
            deposit_slot: 0,
            reward_per_token_paid: 0,
            pending_rewards: 0,
        }
    } else {
        if sp_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let sp_data = sp_account.try_borrow_data()?;
        let p = StakePosition::deserialize(&sp_data)?;
        drop(sp_data);
        p
    };

    // Settle pending rewards before changing position
    settle_pending(&mut pos, cfg.reward_per_token_stored);

    // Update MRC total_staked and serialize before CPI (preserves accumulator update)
    cfg.total_staked = cfg
        .total_staked
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cfg.serialize(&mut mrc_data);

    // Transfer collateral from user to vault
    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        user_ata.key,
        stake_vault.key,
        user.key,
        &[],
        amount,
    )?;
    drop(mrc_data); // release borrow for CPI
    invoke(
        &xfer_ix,
        &[
            user_ata.clone(),
            stake_vault.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    // Update position
    pos.amount = pos
        .amount
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.deposit_slot = clock.slot;
    pos.reward_per_token_paid = cfg.reward_per_token_stored;

    // Write position
    let mut sp_data = sp_account.try_borrow_mut_data()?;
    pos.serialize(&mut sp_data);

    Ok(())
}

// ============================================================================
// withdraw — return collateral + claim pending COIN rewards (no lockup)
// ============================================================================
// WITHDRAWAL GUARANTEE: this instruction is fully permissionless. No
// governance action can prevent a depositor from calling unstake.
// draw_insurance can only draw PROFITS (excess above total_staked), so
// depositor capital is always fully backed. The proportional withdrawal
// math is defense-in-depth only. Every account and PDA in this path is
// either user-controlled or program-derived — no governance approval is
// needed, no governance key is checked, and no governance-modifiable
// state gates the transfer.
// Accounts:
//   [0] user (signer, writable — receives rent on close)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] user_collateral_ata (writable)
//   [4] stake_vault (writable)
//   [5] stake_position PDA (writable)
//   [6] coin_mint (writable)
//   [7] user_coin_ata (writable)
//   [8] mint_authority PDA (read-only)
//   [9] token_program
//   [10] clock
//
// Data: amount (u64)

fn process_unstake<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let user_coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }

    // Verify stake vault PDA
    let (expected_vault, _) =
        Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    verify_token_program(token_program)?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;
    validate_token_account(user_coin_ata, &cfg.coin_mint, user.key)?;

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load and verify StakePosition PDA belongs to this user
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp {
        return Err(ProgramError::InvalidSeeds);
    }
    if sp_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let sp_data_r = sp_account.try_borrow_data()?;
    let mut pos = StakePosition::deserialize(&sp_data_r)?;
    drop(sp_data_r);

    if amount > pos.amount {
        msg!("Unstake amount exceeds staked balance");
        return Err(ProgramError::InsufficientFunds);
    }

    // Settle pending rewards
    settle_pending(&mut pos, cfg.reward_per_token_stored);

    // Proportional withdrawal: if insurance draw depleted the vault,
    // everyone takes the same haircut.
    // actual_withdrawal = (amount * vault_balance) / total_staked
    let vault_token = load_token_account(stake_vault)?;
    let vault_balance = vault_token.amount;
    let actual_withdrawal = if vault_balance >= cfg.total_staked {
        // Vault fully backed — no haircut
        amount
    } else if cfg.total_staked == 0 {
        0
    } else {
        // Vault underfunded — proportional haircut
        let w = (amount as u128)
            .checked_mul(vault_balance as u128)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / (cfg.total_staked as u128);
        // Cap at amount (prevent rounding-up exploitation)
        core::cmp::min(w, amount as u128) as u64
    };

    // Update MRC total_staked and serialize before CPI (so re-reads see updated state)
    cfg.total_staked = cfg.total_staked.saturating_sub(amount);
    cfg.serialize(&mut mrc_data);

    // Transfer proportional collateral from vault to user (signed by MRC PDA)
    let mrc_seeds_arr = mrc_seeds(&cfg.market_slab);
    let (_, mrc_bump) = Pubkey::find_program_address(&mrc_seeds_arr, program_id);
    let mrc_signer: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &[mrc_bump]];

    if actual_withdrawal > 0 {
        let xfer_ix = spl_token::instruction::transfer(
            token_program.key,
            stake_vault.key,
            user_ata.key,
            mrc_account.key,
            &[],
            actual_withdrawal,
        )?;
        drop(mrc_data); // release for CPI
        invoke_signed(
            &xfer_ix,
            &[
                stake_vault.clone(),
                user_ata.clone(),
                mrc_account.clone(),
                token_program.clone(),
            ],
            &[&mrc_signer],
        )?;
    } else {
        drop(mrc_data);
    }

    // Mint pending COIN rewards
    let pending = pos.pending_rewards;
    if pending > 0 {
        if *coin_mint.key != cfg.coin_mint {
            return Err(ProgramError::InvalidAccountData);
        }
        let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
        let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
        if *mint_authority.key != expected_ma {
            return Err(ProgramError::InvalidSeeds);
        }
        let bump_bytes = [ma_bump];
        let signer_seeds: [&[u8]; 3] =
            [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
        mint_coin(
            token_program,
            coin_mint,
            user_coin_ata,
            mint_authority,
            pending,
            &signer_seeds,
        )?;
    }

    // Update position
    pos.amount -= amount;
    pos.pending_rewards = 0;

    if pos.amount == 0 {
        // Close position — return rent to user
        let dest_lamports = user.lamports();
        **user.try_borrow_mut_lamports()? = dest_lamports
            .checked_add(sp_account.lamports())
            .ok_or(ProgramError::ArithmeticOverflow)?;
        **sp_account.try_borrow_mut_lamports()? = 0;
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        sp_data.fill(0);
    } else {
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        pos.serialize(&mut sp_data);
    }

    Ok(())
}

// ============================================================================
// claim_stake_rewards — claim COIN without unstaking
// ============================================================================
// Accounts:
//   [0] user (signer)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] stake_position PDA (writable)
//   [4] coin_mint (writable)
//   [5] user_coin_ata (writable)
//   [6] mint_authority PDA (read-only)
//   [7] token_program
//   [8] clock

fn process_claim_stake_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let user_coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }

    // Verify StakePosition PDA
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp {
        return Err(ProgramError::InvalidSeeds);
    }
    if sp_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    verify_token_program(token_program)?;
    validate_token_account(user_coin_ata, &cfg.coin_mint, user.key)?;

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);
    cfg.serialize(&mut mrc_data);
    drop(mrc_data);

    // Load position, settle, mint
    let sp_data_r = sp_account.try_borrow_data()?;
    let mut pos = StakePosition::deserialize(&sp_data_r)?;
    drop(sp_data_r);

    settle_pending(&mut pos, cfg.reward_per_token_stored);

    let pending = pos.pending_rewards;
    if pending > 0 {
        if *coin_mint.key != cfg.coin_mint {
            return Err(ProgramError::InvalidAccountData);
        }
        let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
        let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
        if *mint_authority.key != expected_ma {
            return Err(ProgramError::InvalidSeeds);
        }
        let bump_bytes = [ma_bump];
        let signer_seeds: [&[u8]; 3] =
            [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
        mint_coin(
            token_program,
            coin_mint,
            user_coin_ata,
            mint_authority,
            pending,
            &signer_seeds,
        )?;
        pos.pending_rewards = 0;
    }

    let mut sp_data = sp_account.try_borrow_mut_data()?;
    pos.serialize(&mut sp_data);

    Ok(())
}

// ============================================================================
// draw_insurance — governance-gated profit withdrawal from vault
// ============================================================================
// The DAO draws PROFITS from the deposit vault — the excess above depositor
// capital (total_staked). Depositor capital is always protected: the DAO
// cannot draw below total_staked. When all depositors have withdrawn
// (total_staked == 0), the DAO can draw whatever remains.
//
// Accounts:
//   [0] payer (signer)
//   [1] authority (signer, governance PDA — must match CoinConfig.authority)
//   [2] mrc PDA (read-only, vault authority for signing)
//   [3] market_slab (read-only)
//   [4] stake_vault (writable)
//   [5] destination (writable — where collateral goes)
//   [6] coin_mint (read-only — for governance authority verification)
//   [7] coin_config PDA (read-only)
//   [8] token_program
//
// Data: amount (u64)

fn process_draw_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;

    // Verify CoinConfig and authority match
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    // Verify MRC PDA
    let mrc_data = mrc_account.try_borrow_data()?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }
    if *coin_mint.key != cfg.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    drop(mrc_data);

    // Verify stake vault PDA
    let (expected_vault, _) =
        Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;

    // Verify destination is correct mint
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != cfg.collateral_mint {
        msg!("Destination mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // DAO can only draw PROFITS: excess above depositor capital (total_staked).
    // Depositor capital is always protected — the DAO cannot haircut depositors.
    // When total_staked == 0 (all depositors withdrew), DAO can draw everything.
    let vault_token = load_token_account(stake_vault)?;
    let available_profit = vault_token.amount.saturating_sub(cfg.total_staked);
    if amount > available_profit {
        msg!("Draw exceeds available profit (vault_balance - total_staked)");
        return Err(ProgramError::InsufficientFunds);
    }

    // Transfer from vault to destination (signed by MRC PDA)
    let mrc_seeds_arr = mrc_seeds(&cfg.market_slab);
    let (_, mrc_bump) = Pubkey::find_program_address(&mrc_seeds_arr, program_id);
    let mrc_signer: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &[mrc_bump]];

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        stake_vault.key,
        destination.key,
        mrc_account.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &xfer_ix,
        &[
            stake_vault.clone(),
            destination.clone(),
            mrc_account.clone(),
            token_program.clone(),
        ],
        &[&mrc_signer],
    )
}

// ============================================================================
// governed reward mint lifecycle
// ============================================================================

fn process_mint_reward<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    let destination_token = load_token_account(destination)?;
    if destination_token.mint != *coin_mint.key {
        msg!("Reward destination mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    mint_coin(
        token_program,
        coin_mint,
        destination,
        mint_authority,
        amount,
        &signer_seeds,
    )
}

fn process_set_market_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let n_per_epoch = read_u64(data)?;
    let epoch_slots = read_u64(data)?;
    if epoch_slots == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }
    if *coin_mint.key != cfg.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }

    let clock = Clock::from_account_info(clock_info)?;
    update_accumulator(&mut cfg, clock.slot);
    cfg.n_per_epoch = n_per_epoch;
    cfg.epoch_slots = epoch_slots;
    cfg.serialize(&mut mrc_data);
    Ok(())
}

fn process_transfer_mint_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let new_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }

    let new_authority_opt = if *new_authority.key == Pubkey::default() {
        None
    } else {
        Some(new_authority.key)
    };
    let ix = spl_token::instruction::set_authority(
        token_program.key,
        coin_mint.key,
        new_authority_opt,
        spl_token::instruction::AuthorityType::MintTokens,
        mint_authority.key,
        &[],
    )?;
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            coin_mint.clone(),
            mint_authority.clone(),
            token_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// ============================================================================
// register_insurance_operator — set MRC PDA as percolator insurance_operator
// ============================================================================
// Called by the current percolator admin in the legacy burned-admin flow to transfer the
// insurance_operator authority to our MRC PDA. After this, the MRC PDA is
// the only account that can call WithdrawInsuranceLimited on that market,
// which our program uses via pull_insurance to capture profits into the
// stake_vault.
//
// Accounts:
//   [0] admin (signer — current percolator admin)
//   [1] mrc_pda (not a signer here; we sign for it via invoke_signed)
//   [2] market_slab (writable — percolator mutates the header)
//   [3] percolator_program
//
// Data: (none)

fn process_register_insurance_operator<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let mrc_pda_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;
    if market_slab.owner != &percolator_abi::id() {
        msg!("Market slab must be owned by Percolator");
        return Err(ProgramError::IllegalOwner);
    }
    {
        let slab_data = market_slab.try_borrow_data()?;
        percolator_abi::read_market_config(&slab_data)?;
    }

    // Derive expected MRC PDA from market_slab and verify the passed account matches.
    let seeds_arr = mrc_seeds(market_slab.key);
    let (expected_mrc, mrc_bump) = Pubkey::find_program_address(&seeds_arr, program_id);
    if *mrc_pda_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }

    // Build percolator UpdateAuthority { kind: INSURANCE_OPERATOR, new: MRC_PDA }.
    // Wire format: tag(1) + kind(1) + pubkey(32) = 34 bytes.
    let mut ix_data = alloc::vec::Vec::with_capacity(34);
    ix_data.push(PERC_IX_UPDATE_AUTHORITY);
    ix_data.push(PERC_AUTHORITY_INSURANCE_OPERATOR);
    ix_data.extend_from_slice(expected_mrc.as_ref());

    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*admin.key, true),
            solana_program::instruction::AccountMeta::new_readonly(expected_mrc, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
        ],
        data: ix_data,
    };

    let bump_bytes = [mrc_bump];
    let signer_seeds: [&[u8]; 3] = [b"mrc", market_slab.key.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            admin.clone(),
            mrc_pda_account.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// ============================================================================
// pull_insurance — capture profit from percolator insurance into stake_vault
// ============================================================================
// Permissionless keeper instruction. CPIs percolator's WithdrawInsuranceLimited
// with MRC PDA as the insurance_operator (signed via invoke_signed), and
// destination = our stake_vault. The bps cap and cooldown on percolator's
// side gate the rate at which fees can be swept. Once funds land in the
// stake_vault, draw_insurance (DAO-gated, profit-only) is how the DAO
// realizes the profit; user unstake continues to draw from the same vault.
//
// Accounts:
//   [0] payer (signer — anyone, pays CPI fees)
//   [1] mrc PDA (writable is NOT needed; we read only, but percolator wants operator signer)
//   [2] market_slab (writable)
//   [3] operator_ata = stake_vault (writable)
//   [4] percolator_vault (writable — source)
//   [5] token_program
//   [6] percolator_vault_pda (signing vault authority on percolator side)
//   [7] clock
//   [8] percolator_program
//
// Data: amount (u64)

fn process_pull_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let mrc_pda_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let _clock_info = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;

    // Verify MRC PDA & derive seeds
    let mrc_data = mrc_account_data_ref(mrc_pda_account, program_id, market_slab.key)?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    drop(mrc_data);
    load_percolator_market_config(market_slab, &cfg.collateral_mint)?;

    let (expected_mrc, mrc_bump) =
        Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_pda_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }

    // Verify stake_vault PDA (destination for the CPI)
    let (expected_vault, _) =
        Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_pda_account.key)?;
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.collateral_mint,
    )?;

    // Build WithdrawInsuranceLimited(amount) — tag 23.
    // Current Percolator v16 expects vault authority before token program, and
    // treats any account after token_program as an optional insurance ledger.
    // insurance ledger, so keep the public meta instruction ABI stable but do
    // not forward the compatibility clock account into the CPI.
    let mut ix_data = alloc::vec::Vec::with_capacity(17);
    ix_data.push(PERC_IX_WITHDRAW_INSURANCE_LIMITED);
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());

    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(expected_mrc, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*stake_vault.key, false),
            solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
            solana_program::instruction::AccountMeta::new_readonly(
                *percolator_vault_pda.key,
                false
            ),
            solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: ix_data,
    };

    let bump_bytes = [mrc_bump];
    let signer_seeds: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            mrc_pda_account.clone(),
            market_slab.clone(),
            stake_vault.clone(),
            percolator_vault.clone(),
            percolator_vault_pda.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// ============================================================================
// Percolator market lifecycle wiring
// ============================================================================
// init_percolator_market accounts:
//   [0] payer/user (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] market_admin PDA (writable; created if missing)
//   [4] market_slab (writable; owned by Percolator, created by caller)
//   [5] collateral_mint
//   [6] percolator_program
//   [7] system_program
//
// Data: raw Percolator InitMarket instruction data, including tag 0.

fn process_init_percolator_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    percolator_ix_data: &[u8],
) -> ProgramResult {
    if percolator_ix_data.first().copied() != Some(PERC_IX_INIT_MARKET) {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_percolator_program(percolator_program)?;
    let _coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    ensure_market_admin_account(
        payer,
        market_admin,
        system_program,
        coin_mint.key,
        program_id,
    )?;
    if market_slab.owner != &percolator_abi::id() {
        return Err(ProgramError::IllegalOwner);
    }
    if collateral_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint_data = collateral_mint.try_borrow_data()?;
    spl_token::state::Mint::unpack(&mint_data)?;
    drop(mint_data);

    let (_, admin_bump) =
        Pubkey::find_program_address(&market_admin_seeds(coin_mint.key), program_id);
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            account_meta_from_info(market_admin, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*collateral_mint.key, false),
        ],
        data: percolator_ix_data.to_vec(),
    };
    invoke_signed(
        &ix,
        &[
            market_admin.clone(),
            market_slab.clone(),
            collateral_mint.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// percolator_admin accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] market_admin PDA (first Percolator account; signed via invoke_signed)
//   [5] percolator_program
//   [6..] remaining Percolator accounts after the admin/authority account
//
// Data: raw allowed Percolator admin/lifecycle instruction data.

fn process_percolator_admin<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    percolator_ix_data: &[u8],
) -> ProgramResult {
    let tag = percolator_ix_data
        .first()
        .copied()
        .ok_or(ProgramError::InvalidInstructionData)?;
    if !percolator_admin_tag_allowed(tag) {
        msg!("Percolator instruction is not an allowed futarchy admin lifecycle action");
        return Err(ProgramError::InvalidInstructionData);
    }

    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;

    let tail: alloc::vec::Vec<AccountInfo<'a>> = iter.cloned().collect();
    let mut metas = alloc::vec::Vec::with_capacity(1 + tail.len());
    metas.push(account_meta_from_info(market_admin, true));
    for account in tail.iter() {
        metas.push(account_meta_from_info(account, account.is_signer));
    }
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: metas,
        data: percolator_ix_data.to_vec(),
    };

    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let mut cpi_accounts = alloc::vec::Vec::with_capacity(2 + tail.len());
    cpi_accounts.push(market_admin.clone());
    cpi_accounts.extend(tail);
    cpi_accounts.push(percolator_program.clone());
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

// ============================================================================
// genesis bootstrap — base deposits, vote units, reward cap, and kickoff
// ============================================================================
// init_genesis_bootstrap accounts:
//   [0] payer (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] base_mint
//   [5] genesis_config PDA (writable, to create)
//   [6] genesis_vault PDA (writable, to create; SPL token account)
//   [7] market_admin PDA (writable; created if missing)
//   [8] token_program
//   [9] rent sysvar
//   [10] system_program
//
// Data: reward_supply (u64)

fn process_init_genesis_bootstrap<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let base_mint = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let reward_supply = read_u64(data)?;
    if reward_supply == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if coin_cfg.is_live() {
        msg!("genesis bootstrap must be initialized during bootstrap phase");
        return Err(ProgramError::InvalidInstructionData);
    }
    if base_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let base_mint_data = base_mint.try_borrow_data()?;
    spl_token::state::Mint::unpack(&base_mint_data)?;
    drop(base_mint_data);

    ensure_market_admin_account(
        payer,
        market_admin,
        system_program,
        coin_mint.key,
        program_id,
    )?;
    let genesis_cfg_seeds_arr = genesis_cfg_seeds(coin_mint.key);
    create_pda_account(
        payer,
        genesis_cfg,
        system_program,
        program_id,
        &genesis_cfg_seeds_arr,
        GENESIS_CFG_SIZE,
    )?;

    let genesis_vault_seeds_arr = genesis_vault_seeds(coin_mint.key);
    let (expected_vault, vault_bump) =
        Pubkey::find_program_address(&genesis_vault_seeds_arr, program_id);
    if *genesis_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    let vault_bump_bytes = [vault_bump];
    let vault_signer: [&[u8]; 3] = [b"genesis_vault", coin_mint.key.as_ref(), &vault_bump_bytes];
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            genesis_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[payer.clone(), genesis_vault.clone(), system_program.clone()],
        &[&vault_signer],
    )?;
    let init_ix = spl_token::instruction::initialize_account2(
        token_program.key,
        genesis_vault.key,
        base_mint.key,
        market_admin.key,
    )?;
    invoke(
        &init_ix,
        &[
            genesis_vault.clone(),
            base_mint.clone(),
            rent_sysvar.clone(),
            token_program.clone(),
        ],
    )?;

    let cfg = GenesisConfig {
        coin_mint: *coin_mint.key,
        base_mint: *base_mint.key,
        token_vault: *genesis_vault.key,
        total_deposited: 0,
        total_withdrawn: 0,
        reward_supply,
        minted_supply: 0,
        insurance_principal_x2: 0,
        backing_principal_x2: 0,
        finalized: 0,
        kicked: 0,
    };
    let mut cfg_data = genesis_cfg.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// genesis_deposit accounts:
//   [0] user (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA (writable)
//   [4] genesis_position PDA (writable, created if missing)
//   [5] user_base_ata (writable)
//   [6] genesis_vault (writable)
//   [7] token_program
//   [8] system_program
//
// Data: amount (u64). One base unit deposited equals one vote unit.

fn process_genesis_deposit<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let user_base_ata = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if coin_cfg.is_live() {
        msg!("genesis deposits are closed after bootstrap");
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut genesis_cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    let mut genesis_cfg = GenesisConfig::deserialize(&genesis_cfg_data)?;
    if genesis_cfg.coin_mint != *coin_mint.key
        || *genesis_cfg_account.key
            != Pubkey::find_program_address(&genesis_cfg_seeds(coin_mint.key), program_id).0
    {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_cfg.is_finalized() || genesis_cfg.is_kicked() {
        msg!("genesis deposits are closed");
        return Err(ProgramError::InvalidInstructionData);
    }
    let (market_admin, _) =
        Pubkey::find_program_address(&market_admin_seeds(coin_mint.key), program_id);
    verify_genesis_vault(genesis_vault, &genesis_cfg, &market_admin, program_id)?;
    validate_token_account(user_base_ata, &genesis_cfg.base_mint, user.key)?;

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, user.key);
    let (expected_position, _) = Pubkey::find_program_address(&position_seeds, program_id);
    if *genesis_position.key != expected_position {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut pos = if genesis_position.data_len() == 0 || genesis_position.lamports() == 0 {
        create_pda_account(
            user,
            genesis_position,
            system_program,
            program_id,
            &position_seeds,
            GENESIS_POSITION_SIZE,
        )?;
        GenesisPosition {
            owner: *user.key,
            amount: 0,
            withdrawn: 0,
            vote_units: 0,
            allocated_vote_units: 0,
        }
    } else {
        if genesis_position.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let pos_data = genesis_position.try_borrow_data()?;
        let pos = GenesisPosition::deserialize(&pos_data)?;
        if pos.owner != *user.key {
            return Err(ProgramError::IllegalOwner);
        }
        pos
    };

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        user_base_ata.key,
        genesis_vault.key,
        user.key,
        &[],
        amount,
    )?;
    invoke(
        &xfer_ix,
        &[
            user_base_ata.clone(),
            genesis_vault.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    genesis_cfg.total_deposited = genesis_cfg
        .total_deposited
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    genesis_cfg.insurance_principal_x2 = genesis_cfg
        .insurance_principal_x2
        .checked_add(amount as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    genesis_cfg.backing_principal_x2 = genesis_cfg
        .backing_principal_x2
        .checked_add(amount as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.amount = pos
        .amount
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.vote_units = pos
        .vote_units
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    genesis_cfg.serialize(&mut genesis_cfg_data);
    let mut pos_data = genesis_position.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// genesis_withdraw accounts:
//   [0] user (signer)
//   [1] genesis_config PDA (writable)
//   [2] genesis_position PDA (writable)
//   [3] coin_mint
//   [4] user_base_ata (writable)
//   [5] genesis_vault (writable)
//   [6] market_admin PDA
//   [7] token_program
//
// Data: none. Withdraw retires the user's vote position and returns up to
// their deposited principal, pro-rated by the recovered vault balance.

fn process_genesis_withdraw<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let user_base_ata = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_finalized() {
        msg!("genesis distribution is not finalized");
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    validate_token_account(user_base_ata, &cfg.base_mint, user.key)?;

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, user.key);
    if *genesis_position.key != Pubkey::find_program_address(&position_seeds, program_id).0 {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_position.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pos_data = genesis_position.try_borrow_data()?;
    let mut pos = GenesisPosition::deserialize(&pos_data)?;
    drop(pos_data);
    if pos.owner != *user.key {
        return Err(ProgramError::IllegalOwner);
    }
    let remaining_principal = pos.amount.saturating_sub(pos.withdrawn);
    if remaining_principal == 0 {
        return Ok(());
    }
    let outstanding_principal = cfg.outstanding_principal();
    if outstanding_principal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let vault_balance = load_token_account(genesis_vault)?.amount;
    let actual =
        genesis_recoverable_principal(remaining_principal, vault_balance, outstanding_principal)?;
    cfg.total_withdrawn = cfg
        .total_withdrawn
        .checked_add(actual)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.withdrawn = pos
        .withdrawn
        .checked_add(actual)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.vote_units = 0;
    pos.allocated_vote_units = 0;
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    let mut pos_data = genesis_position.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    drop(pos_data);
    drop(cfg_data);

    if actual > 0 {
        let bump_bytes = [admin_bump];
        let signer_seeds: [&[u8]; 3] = [
            b"percolator_market_admin",
            coin_mint.key.as_ref(),
            &bump_bytes,
        ];
        let xfer_ix = spl_token::instruction::transfer(
            token_program.key,
            genesis_vault.key,
            user_base_ata.key,
            market_admin.key,
            &[],
            actual,
        )?;
        invoke_signed(
            &xfer_ix,
            &[
                genesis_vault.clone(),
                user_base_ata.clone(),
                market_admin.clone(),
                token_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    Ok(())
}

fn require_genesis_governance(
    program_id: &Pubkey,
    payer: &AccountInfo,
    authority: &AccountInfo,
    coin_mint: &AccountInfo,
    coin_cfg_account: &AccountInfo,
) -> Result<CoinConfig, ProgramError> {
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    Ok(coin_cfg)
}

// init_genesis_distribution accounts:
//   [0] payer/proposer (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA
//   [4] distribution proposal PDA (writable, to create)
//   [5] destination COIN token account
//   [6] system_program
//
// Data: proposal_id (u64), amount (u64)

fn process_init_genesis_distribution<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let proposal_id = read_u64(data)?;
    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    require_live(&coin_cfg)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() || amount > cfg.reward_supply {
        return Err(ProgramError::InvalidInstructionData);
    }
    let destination_token = load_token_account(destination)?;
    if destination_token.mint != *coin_mint.key {
        return Err(ProgramError::InvalidAccountData);
    }

    let proposal_id_bytes = proposal_id.to_le_bytes();
    let seeds = genesis_distribution_seeds(genesis_cfg_account.key, &proposal_id_bytes);
    let expected = Pubkey::find_program_address(&seeds, program_id).0;
    if *distribution_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    create_pda_account(
        payer,
        distribution_account,
        system_program,
        program_id,
        &seeds,
        GENESIS_DISTRIBUTION_SIZE,
    )?;

    let proposal = GenesisDistribution {
        genesis_cfg: *genesis_cfg_account.key,
        destination: *destination.key,
        proposal_id,
        amount,
        yes_votes: 0,
        no_votes: 0,
        executed: 0,
    };
    let mut proposal_data = distribution_account.try_borrow_mut_data()?;
    proposal.serialize(&mut proposal_data);
    Ok(())
}

// vote_genesis_distribution accounts:
//   [0] voter (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA
//   [4] genesis_position PDA
//   [5] distribution proposal PDA (writable)
//   [6] vote record PDA (writable, created if missing)
//   [7] system_program
//
// Data: support (u8; 0 = no, 1 = yes)

fn process_vote_genesis_distribution<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let voter = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;
    let vote_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let support = read_u8(data)?;
    if support > 1 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !voter.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    require_live(&coin_cfg)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, voter.key);
    if *genesis_position.key != Pubkey::find_program_address(&position_seeds, program_id).0
        || genesis_position.owner != program_id
    {
        return Err(ProgramError::InvalidSeeds);
    }
    let pos_data = genesis_position.try_borrow_data()?;
    let pos = GenesisPosition::deserialize(&pos_data)?;
    if pos.owner != *voter.key || pos.vote_units == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    drop(pos_data);

    if distribution_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut proposal_data = distribution_account.try_borrow_mut_data()?;
    let mut proposal = GenesisDistribution::deserialize(&proposal_data)?;
    if proposal.genesis_cfg != *genesis_cfg_account.key || proposal.is_executed() {
        return Err(ProgramError::InvalidAccountData);
    }

    let vote_seeds = genesis_distribution_vote_seeds(distribution_account.key, voter.key);
    let expected_vote = Pubkey::find_program_address(&vote_seeds, program_id).0;
    if *vote_account.key != expected_vote {
        return Err(ProgramError::InvalidSeeds);
    }
    let vote = if vote_account.data_len() == 0 || vote_account.lamports() == 0 {
        create_pda_account(
            voter,
            vote_account,
            system_program,
            program_id,
            &vote_seeds,
            GENESIS_DISTRIBUTION_VOTE_SIZE,
        )?;
        None
    } else {
        if vote_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let vote_data = vote_account.try_borrow_data()?;
        let vote = GenesisDistributionVote::deserialize(&vote_data)?;
        if vote.proposal != *distribution_account.key || vote.voter != *voter.key {
            return Err(ProgramError::InvalidAccountData);
        }
        Some(vote)
    };

    if let Some(old_vote) = vote {
        if old_vote.support == 1 {
            proposal.yes_votes = proposal
                .yes_votes
                .checked_sub(old_vote.weight)
                .ok_or(ProgramError::InvalidAccountData)?;
        } else {
            proposal.no_votes = proposal
                .no_votes
                .checked_sub(old_vote.weight)
                .ok_or(ProgramError::InvalidAccountData)?;
        }
    }
    if support == 1 {
        proposal.yes_votes = proposal
            .yes_votes
            .checked_add(pos.vote_units)
            .ok_or(ProgramError::ArithmeticOverflow)?;
    } else {
        proposal.no_votes = proposal
            .no_votes
            .checked_add(pos.vote_units)
            .ok_or(ProgramError::ArithmeticOverflow)?;
    }

    let new_vote = GenesisDistributionVote {
        proposal: *distribution_account.key,
        voter: *voter.key,
        weight: pos.vote_units,
        support,
    };
    proposal.serialize(&mut proposal_data);
    let mut vote_data = vote_account.try_borrow_mut_data()?;
    new_vote.serialize(&mut vote_data);
    Ok(())
}

// genesis_mint_reward accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] genesis_config PDA (writable)
//   [3] coin_mint (writable)
//   [4] coin_config PDA
//   [5] destination COIN token account (writable)
//   [6] mint_authority PDA
//   [7] distribution proposal PDA (writable)
//   [8] token_program
//
// Data: amount (u64)

fn process_genesis_mint_reward<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_token_program(token_program)?;
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != *coin_mint.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() {
        msg!("genesis distribution already finalized");
        return Err(ProgramError::InvalidInstructionData);
    }
    if distribution_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut proposal_data = distribution_account.try_borrow_mut_data()?;
    let mut proposal = GenesisDistribution::deserialize(&proposal_data)?;
    if proposal.genesis_cfg != *genesis_cfg_account.key
        || proposal.destination != *destination.key
        || proposal.amount != amount
        || proposal.is_executed()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if proposal.yes_votes <= cfg.total_deposited / 2 {
        msg!("genesis distribution proposal lacks majority approval");
        return Err(ProgramError::InvalidInstructionData);
    }
    cfg.minted_supply = cfg
        .minted_supply
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if cfg.minted_supply > cfg.reward_supply {
        return Err(ProgramError::InvalidInstructionData);
    }
    proposal.executed = 1;

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    proposal.serialize(&mut proposal_data);
    drop(proposal_data);
    drop(cfg_data);
    mint_coin(
        token_program,
        coin_mint,
        destination,
        mint_authority,
        amount,
        &signer_seeds,
    )
}

// finalize_genesis accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] genesis_config PDA (writable)
//   [3] coin_mint
//   [4] coin_config PDA
//
// Data: none

fn process_finalize_genesis<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;

    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_kicked() {
        msg!("genesis market must be kicked before finalization");
        return Err(ProgramError::InvalidInstructionData);
    }
    if cfg.minted_supply != cfg.reward_supply {
        msg!("genesis reward supply is not fully distributed");
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.finalized = 1;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// draw_genesis_surplus accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] genesis_config PDA
//   [3] coin_mint
//   [4] coin_config PDA
//   [5] destination base-token account (writable)
//   [6] genesis_vault (writable)
//   [7] market_admin PDA
//   [8] token_program
//
// Data: amount (u64)

fn process_draw_genesis_surplus<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_token_program(token_program)?;
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != cfg.base_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    let vault_balance = load_token_account(genesis_vault)?.amount;
    let available = vault_balance.saturating_sub(cfg.outstanding_principal());
    if amount > available {
        msg!("genesis surplus draw exceeds recovered surplus");
        return Err(ProgramError::InsufficientFunds);
    }

    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        genesis_vault.key,
        destination.key,
        market_admin.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &xfer_ix,
        &[
            genesis_vault.clone(),
            destination.clone(),
            market_admin.clone(),
            token_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// kickstart_genesis_market accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] genesis_config PDA (writable)
//   [5] market_admin PDA
//   [6] market_slab (writable)
//   [7] genesis_vault (writable; source, owned by market_admin PDA)
//   [8] percolator_vault (writable)
//   [9] percolator_vault_pda
//   [10] percolator_program
//   [11] token_program
//
// Data: backing_domain (u8), backing_expiry_slot (u64)

fn process_kickstart_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let backing_domain = read_u8(data)?;
    let backing_expiry_slot = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() || cfg.is_kicked() || cfg.total_deposited == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let percolator_cfg = load_percolator_market_config(market_slab, &cfg.base_mint)?;
    if percolator_cfg.admin != market_admin.key.to_bytes()
        || percolator_cfg.insurance_authority != market_admin.key.to_bytes()
        || percolator_cfg.insurance_operator != market_admin.key.to_bytes()
        || percolator_cfg.backing_bucket_authority != market_admin.key.to_bytes()
    {
        msg!("genesis market must be controlled by the COIN market-admin PDA");
        return Err(ProgramError::InvalidAccountData);
    }
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.base_mint,
    )?;
    let vault_balance = load_token_account(genesis_vault)?.amount;
    if vault_balance < cfg.total_deposited {
        return Err(ProgramError::InsufficientFunds);
    }
    let insurance_amount = cfg.total_deposited / 2;
    let backing_amount = cfg.total_deposited.saturating_sub(insurance_amount);
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    if insurance_amount > 0 {
        let mut insurance_ix_data = alloc::vec::Vec::with_capacity(17);
        insurance_ix_data.push(PERC_IX_TOP_UP_INSURANCE);
        insurance_ix_data.extend_from_slice(&(insurance_amount as u128).to_le_bytes());
        let ix = solana_program::instruction::Instruction {
            program_id: *percolator_program.key,
            accounts: alloc::vec![
                solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
                solana_program::instruction::AccountMeta::new(*market_slab.key, false),
                solana_program::instruction::AccountMeta::new(*genesis_vault.key, false),
                solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
                solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: insurance_ix_data,
        };
        invoke_signed(
            &ix,
            &[
                market_admin.clone(),
                market_slab.clone(),
                genesis_vault.clone(),
                percolator_vault.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    if backing_amount > 0 {
        let mut backing_ix_data = alloc::vec::Vec::with_capacity(27);
        backing_ix_data.push(PERC_IX_TOP_UP_BACKING_BUCKET);
        backing_ix_data.push(backing_domain);
        backing_ix_data.extend_from_slice(&(backing_amount as u128).to_le_bytes());
        backing_ix_data.extend_from_slice(&backing_expiry_slot.to_le_bytes());
        let ix = solana_program::instruction::Instruction {
            program_id: *percolator_program.key,
            accounts: alloc::vec![
                solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
                solana_program::instruction::AccountMeta::new(*market_slab.key, false),
                solana_program::instruction::AccountMeta::new(*genesis_vault.key, false),
                solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
                solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: backing_ix_data,
        };
        invoke_signed(
            &ix,
            &[
                market_admin.clone(),
                market_slab.clone(),
                genesis_vault.clone(),
                percolator_vault.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    cfg.kicked = 1;
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// recover_genesis_market accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] genesis_config PDA
//   [5] market_admin PDA
//   [6] market_slab (writable)
//   [7] genesis_vault (writable; destination, owned by market_admin PDA)
//   [8] percolator_vault (writable)
//   [9] percolator_vault_pda
//   [10] percolator_program
//   [11] token_program
//   [12] optional percolator ledger account (required for backing earnings)
//
// Data: recovery_kind (u8), domain (u8), amount (u64)

fn process_recover_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let recovery_kind = read_u8(data)?;
    let domain = read_u8(data)?;
    let amount = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;

    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_kicked() || cfg.is_finalized() {
        msg!("genesis market recovery requires kicked, unfinalized genesis");
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let percolator_cfg = load_percolator_market_config(market_slab, &cfg.base_mint)?;
    if percolator_cfg.admin != market_admin.key.to_bytes()
        || percolator_cfg.insurance_authority != market_admin.key.to_bytes()
        || percolator_cfg.insurance_operator != market_admin.key.to_bytes()
        || percolator_cfg.backing_bucket_authority != market_admin.key.to_bytes()
    {
        msg!("genesis recovery market must be controlled by the COIN market-admin PDA");
        return Err(ProgramError::InvalidAccountData);
    }
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.base_mint,
    )?;

    let ix_data = genesis_recovery_ix_data(recovery_kind, domain, amount)?;
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];

    let mut metas = alloc::vec::Vec::new();
    let mut cpi_accounts = alloc::vec::Vec::new();
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *market_admin.key,
        true,
    ));
    metas.push(solana_program::instruction::AccountMeta::new(
        *market_slab.key,
        false,
    ));
    cpi_accounts.push(market_admin.clone());
    cpi_accounts.push(market_slab.clone());

    let ledger_account = if recovery_kind == GENESIS_RECOVER_BACKING_EARNINGS {
        let ledger_account = iter.next().ok_or(ProgramError::NotEnoughAccountKeys)?;
        if iter.next().is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
        if ledger_account.owner != &percolator_abi::id() {
            return Err(ProgramError::IllegalOwner);
        }
        Some(ledger_account)
    } else {
        if iter.next().is_some() {
            msg!("ledger account is only accepted for backing earnings recovery");
            return Err(ProgramError::InvalidInstructionData);
        }
        None
    };

    if let Some(ledger_account) = ledger_account {
        metas.push(solana_program::instruction::AccountMeta::new(
            *ledger_account.key,
            false,
        ));
        cpi_accounts.push(ledger_account.clone());
    }

    metas.push(solana_program::instruction::AccountMeta::new(
        *genesis_vault.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new(
        *percolator_vault.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *percolator_vault_pda.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *token_program.key,
        false,
    ));
    cpi_accounts.push(genesis_vault.clone());
    cpi_accounts.push(percolator_vault.clone());
    cpi_accounts.push(percolator_vault_pda.clone());
    cpi_accounts.push(token_program.clone());

    cpi_accounts.push(percolator_program.clone());
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: metas,
        data: ix_data,
    };
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

// approve_builder accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] builder_program
//   [5] builder_approval PDA (writable, created or updated)
//   [6] system_program
//   [7] clock
//
// Data: code_hash ([u8;32]), terms_hash ([u8;32]), enabled (u8)

fn process_approve_builder<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let builder_program = next_account_info(iter)?;
    let approval_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let code_hash = read_bytes32(data)?;
    let terms_hash = read_bytes32(data)?;
    let enabled = read_u8(data)?;
    if enabled > 1 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    if !builder_program.executable {
        msg!("builder approval target must be an executable program account");
        return Err(ProgramError::InvalidAccountData);
    }
    if builder_program.owner != &solana_program::bpf_loader::ID
        && builder_program.owner != &solana_program::bpf_loader_upgradeable::ID
    {
        msg!("builder approval target must be owned by a BPF loader");
        return Err(ProgramError::IllegalOwner);
    }
    let seeds = builder_approval_seeds(coin_mint.key, builder_program.key, &code_hash);
    let expected = Pubkey::find_program_address(&seeds, program_id).0;
    if *approval_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if approval_account.data_len() == 0 || approval_account.lamports() == 0 {
        create_pda_account(
            payer,
            approval_account,
            system_program,
            program_id,
            &seeds,
            BUILDER_APPROVAL_SIZE,
        )?;
    } else if approval_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    } else {
        let approval_data = approval_account.try_borrow_data()?;
        let existing = BuilderApproval::deserialize(&approval_data)?;
        if existing.coin_mint != *coin_mint.key
            || existing.builder_program != *builder_program.key
            || existing.code_hash != code_hash
        {
            return Err(ProgramError::InvalidAccountData);
        }
        drop(approval_data);
    }
    let clock = Clock::from_account_info(clock_info)?;
    let approval = BuilderApproval {
        coin_mint: *coin_mint.key,
        builder_program: *builder_program.key,
        code_hash,
        terms_hash,
        approved_slot: clock.slot,
        enabled,
    };
    let mut approval_data = approval_account.try_borrow_mut_data()?;
    approval.serialize(&mut approval_data);
    Ok(())
}

// ============================================================================
// risk vault wiring — governed setup, depositor-controlled principal
// ============================================================================
// init_risk_vault accounts:
//   [0] payer (signer, writable)
//   [1] authority (signer, governance PDA)
//   [2] market_slab (read-only)
//   [3] risk_vault PDA (writable, to create)
//   [4] coin_mint (read-only)
//   [5] coin_config PDA (read-only)
//   [6] collateral_mint (read-only)
//   [7] token_vault PDA (writable, to create)
//   [8] engine_ledger PDA (writable, to create; owned by percolator)
//   [9] token_program
//   [10] rent sysvar
//   [11] system_program
//   [12] percolator_program
//   [13] fee_destination token account (required when dao_fee_bps > 0)
//
// Data: kind (u8), domain (u8), lockup_slots (u64),
//       withdraw_delay_slots (u64), dao_fee_bps (u16)

fn process_init_risk_vault<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let token_vault = next_account_info(iter)?;
    let engine_ledger = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;

    let kind = read_u8(data)?;
    let domain = read_u8(data)?;
    let suffix = risk_suffix(kind, domain)?;
    let lockup_slots = read_u64(data)?;
    let withdraw_delay_slots = read_u64(data)?;
    let dao_fee_bps = read_u16(data)?;
    if dao_fee_bps > 10_000 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    if collateral_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint_data = collateral_mint.try_borrow_data()?;
    spl_token::state::Mint::unpack(&mint_data)?;
    drop(mint_data);
    load_percolator_market_config(market_slab, collateral_mint.key)?;
    let fee_destination = if dao_fee_bps == 0 {
        Pubkey::default()
    } else {
        if kind == RISK_KIND_INSURANCE {
            msg!("insurance risk vault cannot charge a DAO fee");
            return Err(ProgramError::InvalidInstructionData);
        }
        let fee_destination = next_account_info(iter)?;
        let insurance_suffix = risk_suffix(RISK_KIND_INSURANCE, 0)?;
        let expected_fee_vault = Pubkey::find_program_address(
            &risk_token_vault_seeds(market_slab.key, &insurance_suffix),
            program_id,
        )
        .0;
        if *fee_destination.key != expected_fee_vault {
            msg!("risk vault DAO fee must flow to the main insurance token vault");
            return Err(ProgramError::InvalidSeeds);
        }
        let expected_fee_owner = Pubkey::find_program_address(
            &risk_vault_seeds(market_slab.key, &insurance_suffix),
            program_id,
        )
        .0;
        validate_token_account(fee_destination, collateral_mint.key, &expected_fee_owner)?;
        *fee_destination.key
    };

    let risk_vault_seed_arr = risk_vault_seeds(market_slab.key, &suffix);
    create_pda_account(
        payer,
        risk_vault,
        system_program,
        program_id,
        &risk_vault_seed_arr,
        RISK_VAULT_SIZE,
    )?;

    let ledger_size = if kind == RISK_KIND_INSURANCE {
        percolator_abi::INSURANCE_LEDGER_ACCOUNT_LEN
    } else {
        percolator_abi::BACKING_DOMAIN_LEDGER_ACCOUNT_LEN
    };
    let risk_ledger_seed_arr = risk_ledger_seeds(market_slab.key, &suffix);
    create_pda_account_with_owner(
        payer,
        engine_ledger,
        system_program,
        program_id,
        &risk_ledger_seed_arr,
        ledger_size,
        percolator_program.key,
    )?;

    let token_vault_seeds_arr = risk_token_vault_seeds(market_slab.key, &suffix);
    let (expected_token_vault, token_vault_bump) =
        Pubkey::find_program_address(&token_vault_seeds_arr, program_id);
    if *token_vault.key != expected_token_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    let token_vault_bump_bytes = [token_vault_bump];
    let token_vault_signer: [&[u8]; 4] = [
        b"risk_token_vault",
        market_slab.key.as_ref(),
        &suffix,
        &token_vault_bump_bytes,
    ];
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            token_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[payer.clone(), token_vault.clone(), system_program.clone()],
        &[&token_vault_signer],
    )?;
    let init_ix = spl_token::instruction::initialize_account2(
        token_program.key,
        token_vault.key,
        collateral_mint.key,
        risk_vault.key,
    )?;
    invoke(
        &init_ix,
        &[
            token_vault.clone(),
            collateral_mint.clone(),
            rent_sysvar.clone(),
            token_program.clone(),
        ],
    )?;

    let cfg = RiskVaultCfg {
        kind,
        domain,
        market_slab: *market_slab.key,
        coin_mint: *coin_mint.key,
        collateral_mint: *collateral_mint.key,
        token_vault: *token_vault.key,
        engine_ledger: *engine_ledger.key,
        lockup_slots,
        withdraw_delay_slots,
        total_deposited: 0,
        total_withdrawn: 0,
        total_shares: 0,
        reward_per_share_stored: 0,
        loss_per_share_stored: 0,
        recovery_per_share_stored: 0,
        last_reward_counter: 0,
        last_loss_counter: 0,
        last_recovery_counter: 0,
        dao_fee_bps,
        fee_destination,
    };
    let mut cfg_data = risk_vault.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// register_risk_vault_authority accounts:
//   [0] current_authority (signer)
//   [1] risk_vault PDA (read-only; signs CPI via invoke_signed)
//   [2] market_slab (writable)
//   [3] percolator_program
//
// Data: percolator_authority_kind (u8)

fn process_register_risk_vault_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let current_authority = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !current_authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;
    let authority_kind = read_u8(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let cfg_data = risk_vault.try_borrow_data()?;
    let cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    drop(cfg_data);
    let risk_bump = verify_risk_vault_pda(risk_vault, &cfg, program_id)?;
    if *market_slab.key != cfg.market_slab || market_slab.owner != &percolator_abi::id() {
        return Err(ProgramError::InvalidAccountData);
    }
    match (cfg.kind, authority_kind) {
        (RISK_KIND_INSURANCE, PERC_AUTHORITY_INSURANCE)
        | (RISK_KIND_INSURANCE, PERC_AUTHORITY_INSURANCE_OPERATOR)
        | (RISK_KIND_BACKING, PERC_AUTHORITY_BACKING_BUCKET) => {}
        _ => return Err(ProgramError::InvalidInstructionData),
    }

    let mut ix_data = alloc::vec::Vec::with_capacity(34);
    ix_data.push(PERC_IX_UPDATE_AUTHORITY);
    ix_data.push(authority_kind);
    ix_data.extend_from_slice(risk_vault.key.as_ref());

    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*current_authority.key, true),
            solana_program::instruction::AccountMeta::new_readonly(*risk_vault.key, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
        ],
        data: ix_data,
    };
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let risk_bump_bytes = [risk_bump];
    let signer_seeds: [&[u8]; 4] = [
        b"risk_vault",
        cfg.market_slab.as_ref(),
        &suffix,
        &risk_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[
            current_authority.clone(),
            risk_vault.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// risk_deposit accounts:
//   [0] user (signer)
//   [1] risk_vault PDA (writable)
//   [2] risk_position PDA (writable, created if absent)
//   [3] market_slab (writable)
//   [4] user_collateral_ata (writable)
//   [5] risk_token_vault PDA (writable)
//   [6] percolator_vault (writable)
//   [7] percolator_vault_pda
//   [8] engine_ledger PDA (writable)
//   [9] token_program
//   [10] percolator_program
//   [11] system_program
//   [12] clock
//
// Data: amount (u64), expiry_slot (u64; backing only, ignored for insurance)

fn process_risk_deposit<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let risk_token_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let engine_ledger = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    let expiry_slot = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;

    let cfg_data = risk_vault.try_borrow_data()?;
    let mut cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    let risk_bump = verify_risk_cfg_accounts(
        program_id,
        &cfg,
        risk_vault,
        market_slab,
        risk_token_vault,
        engine_ledger,
        token_program,
    )?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.collateral_mint,
    )?;
    let clock = Clock::from_account_info(clock_info)?;

    drop(cfg_data);
    let mut pos = risk_position_for_user(
        program_id,
        user,
        risk_vault,
        position_account,
        Some(system_program),
    )?;
    settle_risk_position(&mut pos, &cfg);

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        user_ata.key,
        risk_token_vault.key,
        user.key,
        &[],
        amount,
    )?;
    invoke(
        &xfer_ix,
        &[
            user_ata.clone(),
            risk_token_vault.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    let mut ix_data = alloc::vec::Vec::with_capacity(26);
    if cfg.kind == RISK_KIND_INSURANCE {
        ix_data.push(PERC_IX_TOP_UP_INSURANCE);
        ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    } else {
        ix_data.push(PERC_IX_TOP_UP_BACKING_BUCKET);
        ix_data.push(cfg.domain);
        ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
        ix_data.extend_from_slice(&expiry_slot.to_le_bytes());
    }
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*risk_vault.key, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*risk_token_vault.key, false),
            solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            solana_program::instruction::AccountMeta::new(*engine_ledger.key, false),
        ],
        data: ix_data,
    };
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let risk_bump_bytes = [risk_bump];
    let signer_seeds: [&[u8]; 4] = [
        b"risk_vault",
        cfg.market_slab.as_ref(),
        &suffix,
        &risk_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[
            risk_vault.clone(),
            market_slab.clone(),
            risk_token_vault.clone(),
            percolator_vault.clone(),
            token_program.clone(),
            engine_ledger.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )?;

    cfg.total_deposited = cfg
        .total_deposited
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cfg.total_shares = cfg
        .total_shares
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.shares = pos
        .shares
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.deposit_slot = clock.slot;
    pos.reward_per_share_paid = cfg.reward_per_share_stored;
    pos.loss_per_share_paid = cfg.loss_per_share_stored;
    pos.recovery_per_share_paid = cfg.recovery_per_share_stored;

    let mut cfg_data = risk_vault.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    let mut pos_data = position_account.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// risk_request_withdraw accounts:
//   [0] user (signer)
//   [1] risk_vault PDA (read-only)
//   [2] risk_position PDA (writable)
//   [3] clock
//
// Data: amount (u64)

fn process_risk_request_withdraw<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let cfg_data = risk_vault.try_borrow_data()?;
    let cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    verify_risk_vault_pda(risk_vault, &cfg, program_id)?;
    drop(cfg_data);

    let mut pos = risk_position_for_user(program_id, user, risk_vault, position_account, None)?;
    settle_risk_position(&mut pos, &cfg);
    if pos.pending_withdraw_shares != 0 || amount > available_risk_shares(&pos) {
        return Err(ProgramError::InsufficientFunds);
    }
    let clock = Clock::from_account_info(clock_info)?;
    pos.pending_withdraw_shares = amount;
    pos.withdraw_request_slot = clock.slot;
    let mut pos_data = position_account.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// risk_withdraw accounts:
//   [0] user (signer)
//   [1] risk_vault PDA (writable)
//   [2] risk_position PDA (writable)
//   [3] market_slab (writable)
//   [4] user_collateral_ata (writable)
//   [5] risk_token_vault PDA (writable)
//   [6] percolator_vault (writable)
//   [7] percolator_vault_pda
//   [8] engine_ledger PDA (writable)
//   [9] token_program
//   [10] percolator_program
//   [11] clock

fn process_risk_withdraw<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let risk_token_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let engine_ledger = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;

    let cfg_data = risk_vault.try_borrow_data()?;
    let mut cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    let risk_bump = verify_risk_cfg_accounts(
        program_id,
        &cfg,
        risk_vault,
        market_slab,
        risk_token_vault,
        engine_ledger,
        token_program,
    )?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.collateral_mint,
    )?;
    drop(cfg_data);

    let mut pos = risk_position_for_user(program_id, user, risk_vault, position_account, None)?;
    settle_risk_position(&mut pos, &cfg);
    let amount = pos.pending_withdraw_shares;
    let available_principal = pos
        .shares
        .checked_sub(pos.pending_losses)
        .ok_or(ProgramError::InvalidAccountData)?;
    if amount == 0 || amount > available_principal {
        return Err(ProgramError::InsufficientFunds);
    }
    let clock = Clock::from_account_info(clock_info)?;
    let unlock_slot = pos
        .deposit_slot
        .checked_add(cfg.lockup_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let withdraw_slot = pos
        .withdraw_request_slot
        .checked_add(cfg.withdraw_delay_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if clock.slot < unlock_slot || clock.slot < withdraw_slot {
        msg!("risk withdrawal still locked");
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut ix_data = alloc::vec::Vec::with_capacity(18);
    if cfg.kind == RISK_KIND_INSURANCE {
        ix_data.push(PERC_IX_WITHDRAW_INSURANCE_LIMITED);
        ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    } else {
        ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET);
        ix_data.push(cfg.domain);
        ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    }
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*risk_vault.key, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*risk_token_vault.key, false),
            solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
            solana_program::instruction::AccountMeta::new_readonly(
                *percolator_vault_pda.key,
                false
            ),
            solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            solana_program::instruction::AccountMeta::new(*engine_ledger.key, false),
        ],
        data: ix_data,
    };
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let risk_bump_bytes = [risk_bump];
    let signer_seeds: [&[u8]; 4] = [
        b"risk_vault",
        cfg.market_slab.as_ref(),
        &suffix,
        &risk_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[
            risk_vault.clone(),
            market_slab.clone(),
            risk_token_vault.clone(),
            percolator_vault.clone(),
            percolator_vault_pda.clone(),
            token_program.clone(),
            engine_ledger.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )?;

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        risk_token_vault.key,
        user_ata.key,
        risk_vault.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &xfer_ix,
        &[
            risk_token_vault.clone(),
            user_ata.clone(),
            risk_vault.clone(),
            token_program.clone(),
        ],
        &[&signer_seeds],
    )?;

    cfg.total_withdrawn = cfg
        .total_withdrawn
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cfg.total_shares = cfg
        .total_shares
        .checked_sub(amount)
        .ok_or(ProgramError::InvalidAccountData)?;
    pos.shares = pos
        .shares
        .checked_sub(amount)
        .ok_or(ProgramError::InvalidAccountData)?;
    pos.pending_withdraw_shares = 0;
    pos.withdraw_request_slot = 0;
    pos.pending_losses = core::cmp::min(pos.pending_losses, pos.shares);

    let mut cfg_data = risk_vault.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    let mut pos_data = position_account.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// risk_claim_rewards accounts:
//   [0] user (signer)
//   [1] risk_vault PDA (writable)
//   [2] risk_position PDA (writable)
//   [3] market_slab (writable)
//   [4] user_collateral_ata (writable)
//   [5] risk_token_vault PDA (writable)
//   [6] percolator_vault (writable)
//   [7] percolator_vault_pda
//   [8] engine_ledger PDA (writable)
//   [9] token_program
//   [10] percolator_program
//   [11] fee_destination token account (required when accrued fee > 0)

fn process_risk_claim_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let risk_vault = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let risk_token_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let engine_ledger = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;

    let cfg_data = risk_vault.try_borrow_data()?;
    let cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    let risk_bump = verify_risk_cfg_accounts(
        program_id,
        &cfg,
        risk_vault,
        market_slab,
        risk_token_vault,
        engine_ledger,
        token_program,
    )?;
    if cfg.kind != RISK_KIND_BACKING {
        msg!("only backing risk vaults can claim engine earnings");
        return Err(ProgramError::InvalidInstructionData);
    }
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.collateral_mint,
    )?;
    drop(cfg_data);

    let mut pos = risk_position_for_user(program_id, user, risk_vault, position_account, None)?;
    settle_risk_position(&mut pos, &cfg);
    let amount = pos.pending_rewards;
    if amount == 0 {
        let mut pos_data = position_account.try_borrow_mut_data()?;
        pos.serialize(&mut pos_data);
        return Ok(());
    }

    let fee = ((amount as u128)
        .checked_mul(cfg.dao_fee_bps as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / 10_000) as u64;
    let net = amount
        .checked_sub(fee)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let fee_destination = if fee == 0 {
        None
    } else {
        let fee_destination = next_account_info(iter)?;
        if *fee_destination.key != cfg.fee_destination {
            msg!("risk vault fee destination mismatch");
            return Err(ProgramError::InvalidAccountData);
        }
        let fee_token = load_token_account(fee_destination)?;
        if fee_token.mint != cfg.collateral_mint {
            msg!("risk vault fee destination mint mismatch");
            return Err(ProgramError::InvalidAccountData);
        }
        Some(fee_destination)
    };

    let mut ix_data = alloc::vec::Vec::with_capacity(18);
    ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS);
    ix_data.push(cfg.domain);
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*risk_vault.key, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*engine_ledger.key, false),
            solana_program::instruction::AccountMeta::new(*risk_token_vault.key, false),
            solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
            solana_program::instruction::AccountMeta::new_readonly(
                *percolator_vault_pda.key,
                false
            ),
            solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: ix_data,
    };
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let risk_bump_bytes = [risk_bump];
    let signer_seeds: [&[u8]; 4] = [
        b"risk_vault",
        cfg.market_slab.as_ref(),
        &suffix,
        &risk_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[
            risk_vault.clone(),
            market_slab.clone(),
            engine_ledger.clone(),
            risk_token_vault.clone(),
            percolator_vault.clone(),
            percolator_vault_pda.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )?;

    if fee > 0 {
        let fee_destination = fee_destination.ok_or(ProgramError::NotEnoughAccountKeys)?;
        let fee_ix = spl_token::instruction::transfer(
            token_program.key,
            risk_token_vault.key,
            fee_destination.key,
            risk_vault.key,
            &[],
            fee,
        )?;
        invoke_signed(
            &fee_ix,
            &[
                risk_token_vault.clone(),
                fee_destination.clone(),
                risk_vault.clone(),
                token_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    if net > 0 {
        let net_ix = spl_token::instruction::transfer(
            token_program.key,
            risk_token_vault.key,
            user_ata.key,
            risk_vault.key,
            &[],
            net,
        )?;
        invoke_signed(
            &net_ix,
            &[
                risk_token_vault.clone(),
                user_ata.clone(),
                risk_vault.clone(),
                token_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }

    pos.pending_rewards = 0;
    let mut pos_data = position_account.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// sync_risk_vault accounts:
//   [0] risk_vault PDA (writable)
//   [1] market_slab (writable)
//   [2] engine_ledger PDA (writable)
//   [3] percolator_program

fn process_sync_risk_vault<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let risk_vault = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let engine_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    verify_percolator_program(percolator_program)?;
    let cfg_data = risk_vault.try_borrow_data()?;
    let mut cfg = RiskVaultCfg::deserialize(&cfg_data)?;
    let risk_bump = verify_risk_vault_pda(risk_vault, &cfg, program_id)?;
    if *market_slab.key != cfg.market_slab || *engine_ledger.key != cfg.engine_ledger {
        return Err(ProgramError::InvalidAccountData);
    }
    if market_slab.owner != &percolator_abi::id() || engine_ledger.owner != &percolator_abi::id() {
        return Err(ProgramError::IllegalOwner);
    }
    drop(cfg_data);

    let mut ix_data = alloc::vec::Vec::with_capacity(2);
    if cfg.kind == RISK_KIND_INSURANCE {
        ix_data.push(PERC_IX_SYNC_INSURANCE_LEDGER);
    } else {
        ix_data.push(PERC_IX_SYNC_BACKING_DOMAIN_LEDGER);
        ix_data.push(cfg.domain);
    }
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*risk_vault.key, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*engine_ledger.key, false),
        ],
        data: ix_data,
    };
    let suffix = risk_suffix(cfg.kind, cfg.domain)?;
    let risk_bump_bytes = [risk_bump];
    let signer_seeds: [&[u8]; 4] = [
        b"risk_vault",
        cfg.market_slab.as_ref(),
        &suffix,
        &risk_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[
            risk_vault.clone(),
            market_slab.clone(),
            engine_ledger.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )?;

    let ledger_data = engine_ledger.try_borrow_data()?;
    let (reward_counter, loss_counter, recovery_counter) = if cfg.kind == RISK_KIND_INSURANCE {
        let ledger = percolator_abi::read_insurance_ledger(&ledger_data)?;
        if ledger.market_group != cfg.market_slab.to_bytes()
            || ledger.authority != risk_vault.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (0, ledger.cumulative_loss_atoms, 0)
    } else {
        let ledger = percolator_abi::read_backing_domain_ledger(&ledger_data)?;
        if ledger.market_group != cfg.market_slab.to_bytes()
            || ledger.authority != risk_vault.key.to_bytes()
            || ledger.domain != cfg.domain as u16
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (
            ledger.total_earnings_atoms,
            ledger.cumulative_loss_atoms,
            ledger.cumulative_recovery_atoms,
        )
    };
    drop(ledger_data);

    let reward_delta = reward_counter.saturating_sub(cfg.last_reward_counter);
    let loss_delta = loss_counter.saturating_sub(cfg.last_loss_counter);
    let recovery_delta = recovery_counter.saturating_sub(cfg.last_recovery_counter);
    checked_add_per_share(
        &mut cfg.reward_per_share_stored,
        reward_delta,
        cfg.total_shares,
    )?;
    checked_add_per_share(&mut cfg.loss_per_share_stored, loss_delta, cfg.total_shares)?;
    checked_add_per_share(
        &mut cfg.recovery_per_share_stored,
        recovery_delta,
        cfg.total_shares,
    )?;
    cfg.last_reward_counter = reward_counter;
    cfg.last_loss_counter = loss_counter;
    cfg.last_recovery_counter = recovery_counter;

    let mut cfg_data = risk_vault.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

/// Helper: borrow MRC data and verify the account is a valid MRC PDA for the given slab.
fn mrc_account_data_ref<'a>(
    mrc_account: &'a AccountInfo,
    program_id: &Pubkey,
    market_slab: &Pubkey,
) -> Result<core::cell::Ref<'a, &'a mut [u8]>, ProgramError> {
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let data = mrc_account.try_borrow_data()?;
    // Basic size/disc check — the full PDA check must be done by the caller
    // after reading the slab key from the data.
    if data.len() < MRC_SIZE || data[..8] != MRC_DISC {
        return Err(ProgramError::InvalidAccountData);
    }
    let cfg = MarketRewardsCfg::deserialize(&data)?;
    if cfg.market_slab != *market_slab {
        msg!("MRC market slab mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(data)
}

// ============================================================================
// u256 arithmetic helpers
// ============================================================================

fn mul_u128_wide(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = (ll >> 64) + (lh & 0xFFFF_FFFF_FFFF_FFFF) + (hl & 0xFFFF_FFFF_FFFF_FFFF);
    let lo = (ll & 0xFFFF_FFFF_FFFF_FFFF) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);

    (lo, hi)
}

/// Divide a u256 (n_lo, n_hi) by a u128 divisor. Returns u128 (saturates on overflow).
fn div_u256_by_u128(n_lo: u128, n_hi: u128, d: u128) -> u128 {
    if d == 0 {
        return u128::MAX;
    }
    if n_hi == 0 {
        return n_lo / d;
    }
    if n_hi >= d {
        return u128::MAX;
    } // result would overflow u128

    // Long division: process n_lo bits from high to low.
    // After processing all of n_hi (which is < d), remainder = n_hi.
    let mut rem: u128 = n_hi;
    let mut quot: u128 = 0;

    for i in (0..128u32).rev() {
        let bit = (n_lo >> i) & 1;
        let overflow = rem >> 127 != 0;
        rem = rem.wrapping_shl(1) | bit;

        if overflow || rem >= d {
            rem = rem.wrapping_sub(d);
            quot |= 1u128 << i;
        }
    }

    quot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_vault_round_trips_fixed_layout() {
        let cfg = RiskVaultCfg {
            kind: RISK_KIND_BACKING,
            domain: 3,
            market_slab: Pubkey::new_unique(),
            coin_mint: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            token_vault: Pubkey::new_unique(),
            engine_ledger: Pubkey::new_unique(),
            lockup_slots: 11,
            withdraw_delay_slots: 7,
            total_deposited: 100,
            total_withdrawn: 25,
            total_shares: 75,
            reward_per_share_stored: 9 * FP,
            loss_per_share_stored: FP / 2,
            recovery_per_share_stored: FP / 4,
            last_reward_counter: 123,
            last_loss_counter: 45,
            last_recovery_counter: 6,
            dao_fee_bps: 250,
            fee_destination: Pubkey::new_unique(),
        };

        let mut bytes = [0u8; RISK_VAULT_SIZE];
        cfg.serialize(&mut bytes);
        let decoded = RiskVaultCfg::deserialize(&bytes).unwrap();

        assert_eq!(decoded.kind, cfg.kind);
        assert_eq!(decoded.domain, cfg.domain);
        assert_eq!(decoded.market_slab, cfg.market_slab);
        assert_eq!(decoded.token_vault, cfg.token_vault);
        assert_eq!(decoded.engine_ledger, cfg.engine_ledger);
        assert_eq!(decoded.lockup_slots, cfg.lockup_slots);
        assert_eq!(decoded.withdraw_delay_slots, cfg.withdraw_delay_slots);
        assert_eq!(decoded.reward_per_share_stored, cfg.reward_per_share_stored);
        assert_eq!(decoded.loss_per_share_stored, cfg.loss_per_share_stored);
        assert_eq!(
            decoded.recovery_per_share_stored,
            cfg.recovery_per_share_stored
        );
        assert_eq!(decoded.dao_fee_bps, cfg.dao_fee_bps);
        assert_eq!(decoded.fee_destination, cfg.fee_destination);
    }

    #[test]
    fn genesis_config_round_trips_fixed_layout() {
        let cfg = GenesisConfig {
            coin_mint: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            token_vault: Pubkey::new_unique(),
            total_deposited: 101,
            total_withdrawn: 1,
            reward_supply: 1_000_000,
            minted_supply: 250_000,
            insurance_principal_x2: 101,
            backing_principal_x2: 101,
            finalized: 1,
            kicked: 1,
        };

        let mut bytes = [0u8; GENESIS_CFG_SIZE];
        cfg.serialize(&mut bytes);
        let decoded = GenesisConfig::deserialize(&bytes).unwrap();

        assert_eq!(decoded.coin_mint, cfg.coin_mint);
        assert_eq!(decoded.base_mint, cfg.base_mint);
        assert_eq!(decoded.token_vault, cfg.token_vault);
        assert_eq!(decoded.total_deposited, cfg.total_deposited);
        assert_eq!(decoded.total_withdrawn, cfg.total_withdrawn);
        assert_eq!(decoded.reward_supply, cfg.reward_supply);
        assert_eq!(decoded.minted_supply, cfg.minted_supply);
        assert_eq!(decoded.insurance_principal_x2, cfg.insurance_principal_x2);
        assert_eq!(decoded.backing_principal_x2, cfg.backing_principal_x2);
        assert!(decoded.is_finalized());
        assert!(decoded.is_kicked());
        assert_eq!(decoded.outstanding_principal(), 100);
    }

    #[test]
    fn genesis_withdrawal_returns_up_to_principal_pro_rata() {
        assert_eq!(genesis_recoverable_principal(10, 100, 100).unwrap(), 10);
        assert_eq!(genesis_recoverable_principal(10, 50, 100).unwrap(), 5);
        assert_eq!(genesis_recoverable_principal(3, 2, 10).unwrap(), 0);
        assert!(genesis_recoverable_principal(1, 0, 0).is_err());
    }

    #[test]
    fn genesis_recovery_builds_only_supported_percolator_withdrawals() {
        let insurance = genesis_recovery_ix_data(GENESIS_RECOVER_INSURANCE_LIMITED, 7, 5).unwrap();
        assert_eq!(insurance[0], PERC_IX_WITHDRAW_INSURANCE_LIMITED);
        assert_eq!(insurance.len(), 17);
        assert_eq!(u128::from_le_bytes(insurance[1..17].try_into().unwrap()), 5);

        let backing = genesis_recovery_ix_data(GENESIS_RECOVER_BACKING, 3, 9).unwrap();
        assert_eq!(backing[0], PERC_IX_WITHDRAW_BACKING_BUCKET);
        assert_eq!(backing[1], 3);
        assert_eq!(u128::from_le_bytes(backing[2..18].try_into().unwrap()), 9);

        let terminal = genesis_recovery_ix_data(GENESIS_RECOVER_INSURANCE_TERMINAL, 0, 11).unwrap();
        assert_eq!(terminal[0], PERC_IX_WITHDRAW_INSURANCE);
        assert_eq!(terminal.len(), 17);

        assert!(genesis_recovery_ix_data(99, 0, 1).is_err());
        assert!(genesis_recovery_ix_data(GENESIS_RECOVER_BACKING, 0, 0).is_err());
    }

    #[test]
    fn insurance_risk_vaults_are_main_market_only() {
        assert!(risk_suffix(RISK_KIND_INSURANCE, 0).is_ok());
        assert!(risk_suffix(RISK_KIND_INSURANCE, 1).is_err());
        assert!(risk_suffix(RISK_KIND_BACKING, 1).is_ok());
    }

    #[test]
    fn futarchy_admin_proxy_is_lifecycle_scoped() {
        assert!(!percolator_admin_tag_allowed(PERC_IX_INIT_MARKET));
        assert!(percolator_admin_tag_allowed(
            PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY
        ));
        assert!(percolator_admin_tag_allowed(PERC_IX_UPDATE_ASSET_LIFECYCLE));
        assert!(percolator_admin_tag_allowed(PERC_IX_RESOLVE_MARKET));
        assert!(percolator_admin_tag_allowed(PERC_IX_CLOSE_SLAB));
        assert!(!percolator_admin_tag_allowed(PERC_IX_UPDATE_AUTHORITY));
        assert!(!percolator_admin_tag_allowed(PERC_IX_TOP_UP_INSURANCE));
        assert!(!percolator_admin_tag_allowed(
            PERC_IX_WITHDRAW_BACKING_BUCKET
        ));
    }

    #[test]
    fn risk_position_settlement_tracks_rewards_and_losses() {
        let cfg = RiskVaultCfg {
            kind: RISK_KIND_INSURANCE,
            domain: 0,
            market_slab: Pubkey::new_unique(),
            coin_mint: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            token_vault: Pubkey::new_unique(),
            engine_ledger: Pubkey::new_unique(),
            lockup_slots: 0,
            withdraw_delay_slots: 0,
            total_deposited: 100,
            total_withdrawn: 0,
            total_shares: 100,
            reward_per_share_stored: FP / 4,
            loss_per_share_stored: FP / 8,
            recovery_per_share_stored: 0,
            last_reward_counter: 0,
            last_loss_counter: 0,
            last_recovery_counter: 0,
            dao_fee_bps: 0,
            fee_destination: Pubkey::default(),
        };
        let mut pos = RiskPosition {
            owner: Pubkey::new_unique(),
            shares: 40,
            deposit_slot: 0,
            pending_withdraw_shares: 0,
            withdraw_request_slot: 0,
            reward_per_share_paid: 0,
            loss_per_share_paid: 0,
            recovery_per_share_paid: 0,
            pending_rewards: 0,
            pending_losses: 0,
        };

        settle_risk_position(&mut pos, &cfg);

        assert_eq!(pos.pending_rewards, 10);
        assert_eq!(pos.pending_losses, 5);
        assert_eq!(available_risk_shares(&pos), 35);

        let recovered_cfg = RiskVaultCfg {
            recovery_per_share_stored: FP / 16,
            ..cfg
        };
        settle_risk_position(&mut pos, &recovered_cfg);
        assert_eq!(pos.pending_losses, 3);
        assert_eq!(available_risk_shares(&pos), 37);
    }
}
