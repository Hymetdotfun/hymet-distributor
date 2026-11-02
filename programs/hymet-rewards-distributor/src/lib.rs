use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    hash::{hash, hashv},
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    program_pack::Pack,
};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

// Dedicated owner-authorized identity, prepared locally; no Mainnet deployment is authorized.
declare_id!("9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px");

pub const HYPE_MINT: Pubkey = pubkey!("98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g");
pub const DBC_PROGRAM: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const DAMM_V2_PROGRAM: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
/// Meteora's DBC -> DAMM v2 migration config for MigrationFeeOption::Customizable (HYMET's only launch config). Only
/// the DBC pool authority can create pools under it, so its pool PDA for (mint, HYPE) is the one migration destination.
pub const DAMM_V2_MIGRATION_CONFIG: Pubkey = pubkey!("A8gMrEPJkacWkcb3DGwtJwTe16HktSEfvwtuDh2MCtck");
pub const PLATFORM_BPS: u64 = 2_000;
pub const BPS_DENOMINATOR: u64 = 10_000;
const HOLDER_CHUNK_DOMAIN: &[u8] = b"HYMET_HOLDER_PAYOUT_V2\0";
/// Holders per payout transaction: with a depth-10 proof the chunk still fits one 1,232-byte transaction.
pub const MAX_CHUNK_ENTRIES: usize = 6;
/// 1,024 chunks keep proofs at depth 10 (6,144 paid holders per epoch, 128-byte bitmap).
pub const MAX_EPOCH_CHUNKS: u32 = 1_024;
/// About 7 days of slots before an incomplete epoch may be closed back into the holder pool.
pub const EPOCH_EXPIRY_SLOTS: u64 = 1_512_000;

#[program]
pub mod hymet_rewards_distributor {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, snapshot_authority: Pubkey) -> Result<()> {
        require_keys_neq!(
            snapshot_authority,
            Pubkey::default(),
            ErrorCode::InvalidRecipient
        );
        require_keys_neq!(
            ctx.accounts.platform_wallet.key(),
            Pubkey::default(),
            ErrorCode::InvalidRecipient
        );
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.authority.key();
        config.platform_wallet = ctx.accounts.platform_wallet.key();
        config.snapshot_authority = snapshot_authority;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    /// Rotate the snapshot authority (the payout keeper key) if it is lost or compromised.
    /// Only the program's current upgrade authority may call it (same ProgramData check as
    /// `initialize`); the new key gains only epoch funding/closing and DAMM registration.
    pub fn set_snapshot_authority(
        ctx: Context<SetSnapshotAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require!(
            valid_new_snapshot_authority(
                new_authority,
                config.snapshot_authority,
                config.platform_wallet,
                ctx.accounts.authority.key()
            ),
            ErrorCode::InvalidRecipient
        );
        let previous = config.snapshot_authority;
        config.snapshot_authority = new_authority;
        emit!(SnapshotAuthorityChanged {
            previous,
            current: new_authority,
            slot: Clock::get()?.slot
        });
        Ok(())
    }

    /// The PDA is DBC's feeClaimer and the owner of the partner DAMM v2 position.
    /// The program intentionally has no general-purpose token withdrawal method.
    pub fn create_fee_pool(
        ctx: Context<CreateFeePool>,
        creator_bps_of_remaining: u16,
    ) -> Result<()> {
        require!(
            creator_bps_of_remaining <= BPS_DENOMINATOR as u16,
            ErrorCode::InvalidSplit
        );
        require_keys_neq!(
            ctx.accounts.creator.key(),
            ctx.accounts.config.platform_wallet,
            ErrorCode::InvalidRecipient
        );
        require_keys_neq!(
            ctx.accounts.token_mint.key(),
            HYPE_MINT,
            ErrorCode::InvalidMint
        );
        require_keys_eq!(
            *ctx.accounts.dbc_pool.owner,
            DBC_PROGRAM,
            ErrorCode::InvalidPool
        );
        require!(!ctx.accounts.dbc_pool.executable, ErrorCode::InvalidPool);
        require!(
            ctx.accounts.hype_mint.decimals == 9
                && ctx.accounts.hype_mint.freeze_authority.is_none(),
            ErrorCode::InvalidMint
        );
        let pool = &mut ctx.accounts.fee_pool;
        pool.creator = ctx.accounts.creator.key();
        pool.token_mint = ctx.accounts.token_mint.key();
        pool.dbc_pool = ctx.accounts.dbc_pool.key();
        pool.damm_pool = Pubkey::default();
        pool.creator_bps_of_remaining = creator_bps_of_remaining;
        pool.creator_claimable = 0;
        pool.holder_unallocated = 0;
        pool.holder_reserved = 0;
        pool.next_epoch = 0;
        pool.total_settled = 0;
        pool.total_platform = 0;
        pool.bump = ctx.bumps.fee_pool;
        pool.last_snapshot_slot = 0;
        Ok(())
    }

    /// Creator (or the snapshot authority) registers the migration destination. DAMM v2 validates that each
    /// claimed position belongs to this pool and that its NFT signer is the PDA.
    pub fn register_damm_pool(ctx: Context<RegisterDammPool>) -> Result<()> {
        require_keys_eq!(
            *ctx.accounts.damm_pool.owner,
            DAMM_V2_PROGRAM,
            ErrorCode::InvalidPool
        );
        require!(!ctx.accounts.damm_pool.executable, ErrorCode::InvalidPool);
        // Only the canonical migration pool: a creator can no longer register an arbitrary DAMM v2 account first and
        // permanently block post-graduation settlement (and holder payouts) for the mint.
        require_keys_eq!(
            ctx.accounts.damm_pool.key(),
            canonical_damm_pool(&ctx.accounts.fee_pool.token_mint),
            ErrorCode::InvalidPool
        );
        require!(
            ctx.accounts.fee_pool.damm_pool == Pubkey::default(),
            ErrorCode::AlreadyRegistered
        );
        ctx.accounts.fee_pool.damm_pool = ctx.accounts.damm_pool.key();
        emit!(MigrationPoolRegistered {
            mint: ctx.accounts.fee_pool.token_mint,
            pool: ctx.accounts.damm_pool.key(),
            slot: Clock::get()?.slot
        });
        Ok(())
    }

    /// Claim DBC partner fees. The 13 remaining accounts follow the pinned
    /// claimTradingFee IDL with feeClaimer omitted; the program inserts its PDA.
    pub fn settle_dbc<'info>(ctx: Context<'_, '_, '_, 'info, SettleDbc<'info>>) -> Result<()> {
        let before_hype = ctx.accounts.hype_vault.amount;
        let before_base = ctx.accounts.base_vault.amount;
        validate_dbc_cpi_accounts(
            ctx.remaining_accounts,
            &ctx.accounts.fee_pool,
            &ctx.accounts.base_vault,
            &ctx.accounts.hype_vault,
        )?;
        let rem = ctx.remaining_accounts;
        let mut data = hash(b"global:claim_trading_fee").to_bytes()[..8].to_vec();
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        let metas = vec![
            AccountMeta::new_readonly(*rem[0].key, false),
            AccountMeta::new_readonly(*rem[1].key, false),
            AccountMeta::new(*rem[2].key, false),
            AccountMeta::new(*rem[3].key, false),
            AccountMeta::new(*rem[4].key, false),
            AccountMeta::new(*rem[5].key, false),
            AccountMeta::new(*rem[6].key, false),
            AccountMeta::new_readonly(*rem[7].key, false),
            AccountMeta::new_readonly(*rem[8].key, false),
            AccountMeta::new_readonly(ctx.accounts.fee_pool.key(), true),
            AccountMeta::new_readonly(*rem[9].key, false),
            AccountMeta::new_readonly(*rem[10].key, false),
            AccountMeta::new_readonly(*rem[11].key, false),
            AccountMeta::new_readonly(*rem[12].key, false),
        ];
        let ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: metas,
            data,
        };
        let fee_pool_info = ctx.accounts.fee_pool.to_account_info();
        let mut infos: Vec<AccountInfo<'info>> = rem[..9].iter().cloned().collect();
        infos.push(fee_pool_info);
        infos.extend(rem[9..].iter().cloned());
        let bump = [ctx.accounts.fee_pool.bump];
        let mint = ctx.accounts.fee_pool.token_mint;
        let seeds: &[&[u8]] = &[b"fee-pool", mint.as_ref(), &bump];
        invoke_signed(&ix, &infos, &[seeds])?;
        ctx.accounts.hype_vault.reload()?;
        ctx.accounts.base_vault.reload()?;
        require!(
            ctx.accounts.base_vault.amount == before_base,
            ErrorCode::NonQuoteFee
        );
        let received = ctx
            .accounts
            .hype_vault
            .amount
            .checked_sub(before_hype)
            .ok_or(ErrorCode::Accounting)?;
        settle_hype(
            0,
            &mut ctx.accounts.fee_pool,
            &ctx.accounts.config,
            &ctx.accounts.platform_hype,
            &ctx.accounts.token_program,
            &ctx.accounts.hype_mint,
            &mut ctx.accounts.hype_vault,
            received,
        )
    }

    /// Claim quote-token (HYPE) fees from the registered DAMM v2 partner position.
    /// The 14 remaining accounts follow claimPositionFee with signer omitted.
    pub fn settle_damm_v2<'info>(
        ctx: Context<'_, '_, '_, 'info, SettleDammV2<'info>>,
    ) -> Result<()> {
        require_keys_neq!(
            ctx.accounts.fee_pool.damm_pool,
            Pubkey::default(),
            ErrorCode::InvalidPool
        );
        let before_hype = ctx.accounts.hype_vault.amount;
        let before_base = ctx.accounts.base_vault.amount;
        validate_damm_cpi_accounts(
            ctx.remaining_accounts,
            &ctx.accounts.fee_pool,
            &ctx.accounts.base_vault,
            &ctx.accounts.hype_vault,
            &ctx.accounts.position_nft,
        )?;
        let rem = ctx.remaining_accounts;
        let data = hash(b"global:claim_position_fee").to_bytes()[..8].to_vec();
        let metas = vec![
            AccountMeta::new_readonly(*rem[0].key, false),
            AccountMeta::new_readonly(*rem[1].key, false),
            AccountMeta::new(*rem[2].key, false),
            AccountMeta::new(*rem[3].key, false),
            AccountMeta::new(*rem[4].key, false),
            AccountMeta::new(*rem[5].key, false),
            AccountMeta::new(*rem[6].key, false),
            AccountMeta::new_readonly(*rem[7].key, false),
            AccountMeta::new_readonly(*rem[8].key, false),
            AccountMeta::new_readonly(*rem[9].key, false),
            AccountMeta::new_readonly(ctx.accounts.fee_pool.key(), true),
            AccountMeta::new_readonly(*rem[10].key, false),
            AccountMeta::new_readonly(*rem[11].key, false),
            AccountMeta::new_readonly(*rem[12].key, false),
            AccountMeta::new_readonly(*rem[13].key, false),
        ];
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: metas,
            data,
        };
        let fee_pool_info = ctx.accounts.fee_pool.to_account_info();
        let mut infos: Vec<AccountInfo<'info>> = rem[..10].iter().cloned().collect();
        infos.push(fee_pool_info);
        infos.extend(rem[10..].iter().cloned());
        let bump = [ctx.accounts.fee_pool.bump];
        let mint = ctx.accounts.fee_pool.token_mint;
        let seeds: &[&[u8]] = &[b"fee-pool", mint.as_ref(), &bump];
        invoke_signed(&ix, &infos, &[seeds])?;
        ctx.accounts.hype_vault.reload()?;
        ctx.accounts.base_vault.reload()?;
        require!(
            ctx.accounts.base_vault.amount == before_base,
            ErrorCode::NonQuoteFee
        );
        let received = ctx
            .accounts
            .hype_vault
            .amount
            .checked_sub(before_hype)
            .ok_or(ErrorCode::Accounting)?;
        settle_hype(
            1,
            &mut ctx.accounts.fee_pool,
            &ctx.accounts.config,
            &ctx.accounts.platform_hype,
            &ctx.accounts.token_program,
            &ctx.accounts.hype_mint,
            &mut ctx.accounts.hype_vault,
            received,
        )
    }

    /// Reserve only already-collected partner HYPE for one funded snapshot.
    pub fn fund_holder_epoch(
        ctx: Context<FundHolderEpoch>,
        epoch_number: u64,
        snapshot_slot: u64,
        snapshot_hash: [u8; 32],
        merkle_root: [u8; 32],
        funded_amount: u64,
        chunk_count: u32,
    ) -> Result<()> {
        require!(
            snapshot_slot > ctx.accounts.fee_pool.last_snapshot_slot
                && snapshot_slot < Clock::get()?.slot
                && funded_amount > 0
                && chunk_count > 0
                && chunk_count <= MAX_EPOCH_CHUNKS,
            ErrorCode::InvalidEpoch
        );
        ctx.accounts.fee_pool.last_snapshot_slot = snapshot_slot;
        require!(
            epoch_number == ctx.accounts.fee_pool.next_epoch,
            ErrorCode::InvalidEpoch
        );
        require!(
            can_fund_epoch(ctx.accounts.fee_pool.holder_unallocated, funded_amount),
            ErrorCode::InsufficientFunds
        );
        ctx.accounts.fee_pool.holder_unallocated = ctx
            .accounts
            .fee_pool
            .holder_unallocated
            .checked_sub(funded_amount)
            .ok_or(ErrorCode::Accounting)?;
        ctx.accounts.fee_pool.holder_reserved = ctx
            .accounts
            .fee_pool
            .holder_reserved
            .checked_add(funded_amount)
            .ok_or(ErrorCode::Accounting)?;
        ctx.accounts.fee_pool.next_epoch =
            epoch_number.checked_add(1).ok_or(ErrorCode::Accounting)?;
        let epoch = &mut ctx.accounts.epoch;
        epoch.fee_pool = ctx.accounts.fee_pool.key();
        epoch.number = epoch_number;
        epoch.snapshot_slot = snapshot_slot;
        epoch.snapshot_hash = snapshot_hash;
        epoch.merkle_root = merkle_root;
        epoch.funded_amount = funded_amount;
        epoch.claimed_amount = 0;
        epoch.chunk_count = chunk_count;
        epoch.paid_chunks = 0;
        epoch.bump = ctx.bumps.epoch;
        epoch.paid_bitmap = vec![0; bitmap_len(chunk_count)];
        emit!(HolderEpochFunded {
            mint: ctx.accounts.fee_pool.token_mint,
            epoch: epoch_number,
            epoch_account: epoch.key(),
            snapshot_slot,
            snapshot_hash,
            merkle_root,
            funded_amount,
            chunk_count,
            slot: Clock::get()?.slot
        });
        Ok(())
    }

    /// Push one batch ("chunk") of a funded epoch to holders. Permissionless: the
    /// Merkle root fixed at funding decides every recipient and amount, each HYPE
    /// destination must be the wallet's canonical HYPE ATA, and the epoch bitmap
    /// lets each chunk pay exactly once. Holders never sign or claim.
    pub fn distribute_holder_chunk<'info>(
        ctx: Context<'_, '_, '_, 'info, DistributeHolderChunk<'info>>,
        chunk_index: u32,
        entries: Vec<PayoutEntry>,
        proof: Vec<[u8; 32]>,
    ) -> Result<()> {
        require!(
            !entries.is_empty()
                && entries.len() <= MAX_CHUNK_ENTRIES
                && proof.len() <= 32
                && ctx.remaining_accounts.len() == entries.len() * 2,
            ErrorCode::InvalidProof
        );
        let epoch = &mut ctx.accounts.epoch;
        require_keys_eq!(
            epoch.fee_pool,
            ctx.accounts.fee_pool.key(),
            ErrorCode::InvalidEpoch
        );
        require!(chunk_index < epoch.chunk_count, ErrorCode::InvalidEpoch);
        require!(
            !is_chunk_paid(&epoch.paid_bitmap, chunk_index),
            ErrorCode::AlreadyPaid
        );
        let mint = ctx.accounts.fee_pool.token_mint;
        let wallets: Vec<Pubkey> = ctx
            .remaining_accounts
            .chunks(2)
            .map(|pair| *pair[0].key)
            .collect();
        for (i, pair) in ctx.remaining_accounts.chunks(2).enumerate() {
            let entry = &entries[i];
            require!(
                entry.token_balance > 0 && entry.reward > 0,
                ErrorCode::InvalidProof
            );
            require!(
                i == 0 || wallets[i - 1].to_bytes() < wallets[i].to_bytes(),
                ErrorCode::InvalidProof
            );
            require_keys_eq!(
                *pair[1].key,
                anchor_spl::associated_token::get_associated_token_address(
                    pair[0].key,
                    &HYPE_MINT
                ),
                ErrorCode::InvalidRecipient
            );
            require!(pair[1].is_writable, ErrorCode::InvalidRecipient);
        }
        let leaf = chunk_leaf(&mint, epoch.number, chunk_index, &wallets, &entries);
        require!(
            verify_proof(leaf, &proof) == epoch.merkle_root,
            ErrorCode::InvalidProof
        );
        let total = entries
            .iter()
            .try_fold(0u64, |sum, e| sum.checked_add(e.reward))
            .ok_or(ErrorCode::Accounting)?;
        require!(
            can_claim(epoch.funded_amount, epoch.claimed_amount, total),
            ErrorCode::OverClaim
        );
        mark_chunk_paid(&mut epoch.paid_bitmap, chunk_index);
        epoch.paid_chunks = epoch
            .paid_chunks
            .checked_add(1)
            .ok_or(ErrorCode::Accounting)?;
        epoch.claimed_amount = epoch
            .claimed_amount
            .checked_add(total)
            .ok_or(ErrorCode::Accounting)?;
        let epoch_number = epoch.number;
        ctx.accounts.fee_pool.holder_reserved = ctx
            .accounts
            .fee_pool
            .holder_reserved
            .checked_sub(total)
            .ok_or(ErrorCode::Accounting)?;
        let slot = Clock::get()?.slot;
        for (i, pair) in ctx.remaining_accounts.chunks(2).enumerate() {
            transfer_from_vault_to(
                &ctx.accounts.token_program,
                &ctx.accounts.hype_mint,
                &ctx.accounts.hype_vault,
                &pair[1],
                &ctx.accounts.fee_pool,
                entries[i].reward,
            )?;
            emit!(HolderPaid {
                mint,
                epoch: epoch_number,
                chunk: chunk_index,
                wallet: wallets[i],
                amount: entries[i].reward,
                slot
            });
        }
        ctx.accounts.hype_vault.reload()?;
        ensure_solvent(&ctx.accounts.fee_pool, &ctx.accounts.hype_vault)
    }

    /// Close a fully paid epoch, or an expired one. Any unpaid reserve returns to
    /// the holder pool (never to HYMET or the creator); rent returns to the authority.
    pub fn close_holder_epoch(ctx: Context<CloseHolderEpoch>) -> Result<()> {
        let epoch = &ctx.accounts.epoch;
        require_keys_eq!(
            epoch.fee_pool,
            ctx.accounts.fee_pool.key(),
            ErrorCode::InvalidEpoch
        );
        let slot = Clock::get()?.slot;
        require!(
            can_close_epoch(epoch.paid_chunks, epoch.chunk_count, epoch.snapshot_slot, slot),
            ErrorCode::EpochActive
        );
        let unpaid = epoch
            .funded_amount
            .checked_sub(epoch.claimed_amount)
            .ok_or(ErrorCode::Accounting)?;
        let pool = &mut ctx.accounts.fee_pool;
        pool.holder_reserved = pool
            .holder_reserved
            .checked_sub(unpaid)
            .ok_or(ErrorCode::Accounting)?;
        pool.holder_unallocated = pool
            .holder_unallocated
            .checked_add(unpaid)
            .ok_or(ErrorCode::Accounting)?;
        emit!(HolderEpochClosed {
            mint: pool.token_mint,
            epoch: epoch.number,
            paid_amount: epoch.claimed_amount,
            returned_amount: unpaid,
            slot
        });
        Ok(())
    }

    pub fn claim_creator(ctx: Context<ClaimCreator>) -> Result<()> {
        let amount = ctx.accounts.fee_pool.creator_claimable;
        require!(amount > 0, ErrorCode::NothingToClaim);
        ctx.accounts.fee_pool.creator_claimable = 0;
        transfer_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.hype_mint,
            &ctx.accounts.hype_vault,
            &ctx.accounts.creator_hype,
            &ctx.accounts.fee_pool,
            amount,
        )?;
        ctx.accounts.hype_vault.reload()?;
        emit!(CreatorClaimed {
            mint: ctx.accounts.fee_pool.token_mint,
            wallet: ctx.accounts.creator.key(),
            amount,
            slot: Clock::get()?.slot
        });
        ensure_solvent(&ctx.accounts.fee_pool, &ctx.accounts.hype_vault)
    }
}

fn settle_hype<'info>(
    source: u8,
    fee_pool: &mut Account<'info, FeePool>,
    config: &Account<'info, DistributorConfig>,
    platform_hype: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    hype_mint: &Account<'info, Mint>,
    hype_vault: &mut Account<'info, TokenAccount>,
    received: u64,
) -> Result<()> {
    if received == 0 {
        return Ok(());
    }
    let platform = ((received as u128 * PLATFORM_BPS as u128) / BPS_DENOMINATOR as u128) as u64;
    let remainder = received
        .checked_sub(platform)
        .ok_or(ErrorCode::Accounting)?;
    let creator = ((remainder as u128 * fee_pool.creator_bps_of_remaining as u128)
        / BPS_DENOMINATOR as u128) as u64;
    let holders = remainder
        .checked_sub(creator)
        .ok_or(ErrorCode::Accounting)?;
    require_keys_neq!(
        config.platform_wallet,
        fee_pool.creator,
        ErrorCode::InvalidRecipient
    );
    require_keys_eq!(
        platform_hype.owner,
        config.platform_wallet,
        ErrorCode::InvalidRecipient
    );
    require_keys_eq!(platform_hype.mint, HYPE_MINT, ErrorCode::InvalidMint);
    if platform > 0 {
        transfer_from_vault(
            token_program,
            hype_mint,
            hype_vault,
            platform_hype,
            fee_pool,
            platform,
        )?;
        hype_vault.reload()?;
    }
    fee_pool.creator_claimable = fee_pool
        .creator_claimable
        .checked_add(creator)
        .ok_or(ErrorCode::Accounting)?;
    fee_pool.holder_unallocated = fee_pool
        .holder_unallocated
        .checked_add(holders)
        .ok_or(ErrorCode::Accounting)?;
    fee_pool.total_settled = fee_pool
        .total_settled
        .checked_add(received)
        .ok_or(ErrorCode::Accounting)?;
    fee_pool.total_platform = fee_pool
        .total_platform
        .checked_add(platform)
        .ok_or(ErrorCode::Accounting)?;
    emit!(FeesSettled {
        mint: fee_pool.token_mint,
        source,
        distributable: received,
        hymet: platform,
        creator,
        holders,
        slot: Clock::get()?.slot
    });
    ensure_solvent(fee_pool, hype_vault)
}

fn transfer_from_vault<'info>(
    token_program: &Program<'info, Token>,
    hype_mint: &Account<'info, Mint>,
    source: &Account<'info, TokenAccount>,
    destination: &Account<'info, TokenAccount>,
    fee_pool: &Account<'info, FeePool>,
    amount: u64,
) -> Result<()> {
    let bump = [fee_pool.bump];
    let mint = fee_pool.token_mint;
    let seeds: &[&[u8]] = &[b"fee-pool", mint.as_ref(), &bump];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: source.to_account_info(),
                mint: hype_mint.to_account_info(),
                to: destination.to_account_info(),
                authority: fee_pool.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        9,
    )
}

/// Same as transfer_from_vault, for a destination validated by the caller.
fn transfer_from_vault_to<'info>(
    token_program: &Program<'info, Token>,
    hype_mint: &Account<'info, Mint>,
    source: &Account<'info, TokenAccount>,
    destination: &AccountInfo<'info>,
    fee_pool: &Account<'info, FeePool>,
    amount: u64,
) -> Result<()> {
    let bump = [fee_pool.bump];
    let mint = fee_pool.token_mint;
    let seeds: &[&[u8]] = &[b"fee-pool", mint.as_ref(), &bump];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: source.to_account_info(),
                mint: hype_mint.to_account_info(),
                to: destination.clone(),
                authority: fee_pool.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        9,
    )
}

/// domain | mint | epoch | chunk | count | (wallet | token balance | HYPE reward)*
fn chunk_leaf(
    mint: &Pubkey,
    epoch: u64,
    chunk_index: u32,
    wallets: &[Pubkey],
    entries: &[PayoutEntry],
) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(HOLDER_CHUNK_DOMAIN.len() + 45 + entries.len() * 48);
    bytes.extend_from_slice(HOLDER_CHUNK_DOMAIN);
    bytes.extend_from_slice(mint.as_ref());
    bytes.extend_from_slice(&epoch.to_le_bytes());
    bytes.extend_from_slice(&chunk_index.to_le_bytes());
    bytes.push(entries.len() as u8);
    for (wallet, entry) in wallets.iter().zip(entries) {
        bytes.extend_from_slice(wallet.as_ref());
        bytes.extend_from_slice(&entry.token_balance.to_le_bytes());
        bytes.extend_from_slice(&entry.reward.to_le_bytes());
    }
    hash(&bytes).to_bytes()
}

fn verify_proof(leaf: [u8; 32], proof: &[[u8; 32]]) -> [u8; 32] {
    proof.iter().fold(leaf, |node, sibling| {
        if node <= *sibling {
            hashv(&[&node, sibling]).to_bytes()
        } else {
            hashv(&[sibling, &node]).to_bytes()
        }
    })
}

fn bitmap_len(chunks: u32) -> usize {
    chunks.div_ceil(8) as usize
}
fn is_chunk_paid(bitmap: &[u8], chunk: u32) -> bool {
    bitmap[(chunk / 8) as usize] & (1 << (chunk % 8)) != 0
}
fn mark_chunk_paid(bitmap: &mut [u8], chunk: u32) {
    bitmap[(chunk / 8) as usize] |= 1 << (chunk % 8);
}
fn can_close_epoch(paid: u32, chunks: u32, snapshot_slot: u64, now: u64) -> bool {
    paid == chunks
        || snapshot_slot
            .checked_add(EPOCH_EXPIRY_SLOTS)
            .is_some_and(|expiry| now >= expiry)
}

fn ensure_solvent(pool: &Account<FeePool>, vault: &Account<TokenAccount>) -> Result<()> {
    let liabilities = pool
        .creator_claimable
        .checked_add(pool.holder_unallocated)
        .and_then(|x| x.checked_add(pool.holder_reserved))
        .ok_or(ErrorCode::Accounting)?;
    require!(vault.amount >= liabilities, ErrorCode::Insolvent);
    Ok(())
}

fn can_fund_epoch(unallocated: u64, amount: u64) -> bool {
    amount > 0 && amount <= unallocated
}
fn can_claim(funded: u64, claimed: u64, amount: u64) -> bool {
    amount > 0
        && claimed
            .checked_add(amount)
            .is_some_and(|total| total <= funded)
}
/// The keeper role stays separate from the fee wallet and the upgrade authority.
fn valid_new_snapshot_authority(
    new_authority: Pubkey,
    current: Pubkey,
    platform_wallet: Pubkey,
    upgrade_authority: Pubkey,
) -> bool {
    new_authority != Pubkey::default()
        && new_authority != current
        && new_authority != platform_wallet
        && new_authority != upgrade_authority
}
fn is_snapshot_authority(configured: Pubkey, signer: Pubkey) -> bool {
    configured == signer
}

fn validate_dbc_cpi_accounts(
    r: &[AccountInfo<'_>],
    pool: &Account<FeePool>,
    base: &Account<TokenAccount>,
    hype: &Account<TokenAccount>,
) -> Result<()> {
    require!(r.len() == 13, ErrorCode::InvalidCpiAccounts);
    require_keys_eq!(
        *r[0].key,
        pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM"),
        ErrorCode::InvalidProgram
    );
    require_keys_eq!(*r[1].owner, DBC_PROGRAM, ErrorCode::InvalidProgram);
    require_keys_eq!(*r[2].key, pool.dbc_pool, ErrorCode::InvalidPool);
    require_keys_eq!(*r[2].owner, DBC_PROGRAM, ErrorCode::InvalidPool);
    require_keys_eq!(*r[3].key, base.key(), ErrorCode::InvalidVault);
    require_keys_eq!(*r[4].key, hype.key(), ErrorCode::InvalidVault);
    require_keys_eq!(*r[3].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[4].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[5].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[6].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[7].key, pool.token_mint, ErrorCode::InvalidMint);
    require_keys_eq!(*r[8].key, HYPE_MINT, ErrorCode::InvalidMint);
    require_keys_eq!(
        token_account_mint(&r[3])?,
        pool.token_mint,
        ErrorCode::InvalidMint
    );
    require_keys_eq!(
        token_account_mint(&r[4])?,
        HYPE_MINT,
        ErrorCode::InvalidMint
    );
    require_keys_eq!(
        token_account_mint(&r[5])?,
        pool.token_mint,
        ErrorCode::InvalidMint
    );
    require_keys_eq!(
        token_account_mint(&r[6])?,
        HYPE_MINT,
        ErrorCode::InvalidMint
    );
    require_keys_eq!(
        *r[9].key,
        anchor_spl::token::ID,
        ErrorCode::InvalidTokenProgram
    );
    require_keys_eq!(
        *r[10].key,
        anchor_spl::token::ID,
        ErrorCode::InvalidTokenProgram
    );
    require_keys_eq!(*r[12].key, DBC_PROGRAM, ErrorCode::InvalidProgram);
    require!(r[12].executable, ErrorCode::InvalidProgram);
    require_keys_eq!(
        *r[11].key,
        Pubkey::find_program_address(&[b"__event_authority"], &DBC_PROGRAM).0,
        ErrorCode::InvalidProgram
    );
    Ok(())
}

fn validate_damm_cpi_accounts(
    r: &[AccountInfo<'_>],
    pool: &Account<FeePool>,
    base: &Account<TokenAccount>,
    hype: &Account<TokenAccount>,
    position_nft: &InterfaceAccount<anchor_spl::token_interface::TokenAccount>,
) -> Result<()> {
    require!(r.len() == 14, ErrorCode::InvalidCpiAccounts);
    require_keys_eq!(
        *r[0].key,
        pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC"),
        ErrorCode::InvalidProgram
    );
    require_keys_eq!(*r[1].key, pool.damm_pool, ErrorCode::InvalidPool);
    require_keys_eq!(*r[1].owner, DAMM_V2_PROGRAM, ErrorCode::InvalidPool);
    require_keys_eq!(*r[2].owner, DAMM_V2_PROGRAM, ErrorCode::InvalidPosition);
    let a_mint = *r[7].key;
    let b_mint = *r[8].key;
    let (a_recipient, b_recipient) = if a_mint == pool.token_mint && b_mint == HYPE_MINT {
        (base.key(), hype.key())
    } else if a_mint == HYPE_MINT && b_mint == pool.token_mint {
        (hype.key(), base.key())
    } else {
        return err!(ErrorCode::InvalidMint);
    };
    require_keys_eq!(*r[3].key, a_recipient, ErrorCode::InvalidVault);
    require_keys_eq!(*r[4].key, b_recipient, ErrorCode::InvalidVault);
    require_keys_eq!(*r[3].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[4].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[5].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(*r[6].owner, anchor_spl::token::ID, ErrorCode::InvalidVault);
    require_keys_eq!(token_account_mint(&r[3])?, a_mint, ErrorCode::InvalidMint);
    require_keys_eq!(token_account_mint(&r[4])?, b_mint, ErrorCode::InvalidMint);
    require_keys_eq!(token_account_mint(&r[5])?, a_mint, ErrorCode::InvalidMint);
    require_keys_eq!(token_account_mint(&r[6])?, b_mint, ErrorCode::InvalidMint);
    require_keys_eq!(*r[9].key, position_nft.key(), ErrorCode::InvalidPosition);
    require_keys_eq!(
        *r[10].key,
        anchor_spl::token::ID,
        ErrorCode::InvalidTokenProgram
    );
    require_keys_eq!(
        *r[11].key,
        anchor_spl::token::ID,
        ErrorCode::InvalidTokenProgram
    );
    require_keys_eq!(*r[13].key, DAMM_V2_PROGRAM, ErrorCode::InvalidProgram);
    require!(r[13].executable, ErrorCode::InvalidProgram);
    require_keys_eq!(
        *r[12].key,
        Pubkey::find_program_address(&[b"__event_authority"], &DAMM_V2_PROGRAM).0,
        ErrorCode::InvalidProgram
    );
    require_keys_eq!(position_nft.owner, pool.key(), ErrorCode::InvalidPosition);
    require!(position_nft.amount == 1, ErrorCode::InvalidPosition);
    Ok(())
}

fn token_account_mint(account: &AccountInfo<'_>) -> Result<Pubkey> {
    let data = account.try_borrow_data()?;
    let token = anchor_spl::token::spl_token::state::Account::unpack(&data)
        .map_err(|_| error!(ErrorCode::InvalidVault))?;
    Ok(token.mint)
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    /// Only the program's upgrade authority may create the one-time config, so the
    /// config PDA cannot be front-run between deployment and initialization.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: Recorded as the HYMET platform fee recipient; it only receives HYPE.
    pub platform_wallet: UncheckedAccount<'info>,
    #[account(init, payer=authority, space=8+DistributorConfig::INIT_SPACE, seeds=[b"config"], bump)]
    pub config: Account<'info, DistributorConfig>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ ErrorCode::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::HymetRewardsDistributor>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ ErrorCode::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetSnapshotAuthority<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump)]
    pub config: Account<'info, DistributorConfig>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ ErrorCode::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::HymetRewardsDistributor>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ ErrorCode::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
}

#[derive(Accounts)]
pub struct CreateFeePool<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump)]
    pub config: Account<'info, DistributorConfig>,
    #[account(address=HYPE_MINT)]
    pub hype_mint: Account<'info, Mint>,
    /// CHECK: DBC ownership and executable status are verified in the handler.
    pub dbc_pool: UncheckedAccount<'info>,
    pub token_mint: Account<'info, Mint>,
    #[account(init, payer=creator, space=8+FeePool::INIT_SPACE, seeds=[b"fee-pool", token_mint.key().as_ref()], bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(init, payer=creator, associated_token::mint=token_mint, associated_token::authority=fee_pool)]
    pub base_vault: Account<'info, TokenAccount>,
    #[account(init, payer=creator, associated_token::mint=hype_mint, associated_token::authority=fee_pool)]
    pub hype_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterDammPool<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(seeds=[b"config"], bump=config.bump)]
    pub config: Account<'info, DistributorConfig>,
    /// The creator, or HYMET's snapshot authority so the payout keeper can continue after graduation.
    #[account(constraint = registrar.key() == fee_pool.creator || registrar.key() == config.snapshot_authority @ ErrorCode::Unauthorized)]
    pub registrar: Signer<'info>,
    /// CHECK: The CP-AMM owner and executable flag are checked in the handler.
    pub damm_pool: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SettleDbc<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(seeds=[b"config"], bump=config.bump)]
    pub config: Account<'info, DistributorConfig>,
    #[account(mut, associated_token::mint=fee_pool.token_mint, associated_token::authority=fee_pool)]
    pub base_vault: Account<'info, TokenAccount>,
    #[account(mut, associated_token::mint=hype_mint, associated_token::authority=fee_pool)]
    pub hype_vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint=hype_mint, token::authority=config.platform_wallet)]
    pub platform_hype: Account<'info, TokenAccount>,
    #[account(address=HYPE_MINT)]
    pub hype_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub caller: Signer<'info>,
}

#[derive(Accounts)]
pub struct SettleDammV2<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(seeds=[b"config"], bump=config.bump)]
    pub config: Account<'info, DistributorConfig>,
    #[account(mut, associated_token::mint=fee_pool.token_mint, associated_token::authority=fee_pool)]
    pub base_vault: Account<'info, TokenAccount>,
    #[account(mut, associated_token::mint=hype_mint, associated_token::authority=fee_pool)]
    pub hype_vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint=hype_mint, token::authority=config.platform_wallet)]
    pub platform_hype: Account<'info, TokenAccount>,
    #[account(address=HYPE_MINT)]
    pub hype_mint: Account<'info, Mint>,
    /// CHECK: The SPL token account owner and amount are checked by Anchor and handler.
    pub position_nft: InterfaceAccount<'info, anchor_spl::token_interface::TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub caller: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(epoch_number: u64, snapshot_slot: u64, snapshot_hash: [u8; 32], merkle_root: [u8; 32], funded_amount: u64, chunk_count: u32)]
pub struct FundHolderEpoch<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(seeds=[b"config"], bump=config.bump, constraint=is_snapshot_authority(config.snapshot_authority, authority.key()) @ ErrorCode::Unauthorized)]
    pub config: Account<'info, DistributorConfig>,
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(init, payer=authority, space=RewardEpoch::space(chunk_count), seeds=[b"epoch", fee_pool.key().as_ref(), &epoch_number.to_le_bytes()], bump)]
    pub epoch: Account<'info, RewardEpoch>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DistributeHolderChunk<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(mut, seeds=[b"epoch", fee_pool.key().as_ref(), &epoch.number.to_le_bytes()], bump=epoch.bump)]
    pub epoch: Account<'info, RewardEpoch>,
    #[account(mut, associated_token::mint=hype_mint, associated_token::authority=fee_pool)]
    pub hype_vault: Account<'info, TokenAccount>,
    #[account(address=HYPE_MINT)]
    pub hype_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    /// Any keeper; pays only the transaction fee.
    pub payer: Signer<'info>,
}

#[derive(Accounts)]
pub struct CloseHolderEpoch<'info> {
    #[account(mut, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(seeds=[b"config"], bump=config.bump, constraint=is_snapshot_authority(config.snapshot_authority, authority.key()) @ ErrorCode::Unauthorized)]
    pub config: Account<'info, DistributorConfig>,
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(mut, close=authority, seeds=[b"epoch", fee_pool.key().as_ref(), &epoch.number.to_le_bytes()], bump=epoch.bump)]
    pub epoch: Account<'info, RewardEpoch>,
}

#[derive(Accounts)]
pub struct ClaimCreator<'info> {
    #[account(mut, has_one=creator, seeds=[b"fee-pool", fee_pool.token_mint.as_ref()], bump=fee_pool.bump)]
    pub fee_pool: Account<'info, FeePool>,
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, associated_token::mint=hype_mint, associated_token::authority=fee_pool)]
    pub hype_vault: Account<'info, TokenAccount>,
    #[account(mut, associated_token::mint=hype_mint, associated_token::authority=creator)]
    pub creator_hype: Account<'info, TokenAccount>,
    #[account(address=HYPE_MINT)]
    pub hype_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

#[account]
#[derive(InitSpace)]
pub struct DistributorConfig {
    pub admin: Pubkey,
    pub platform_wallet: Pubkey,
    pub snapshot_authority: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct FeePool {
    pub creator: Pubkey,
    pub token_mint: Pubkey,
    pub dbc_pool: Pubkey,
    pub damm_pool: Pubkey,
    pub creator_bps_of_remaining: u16,
    pub creator_claimable: u64,
    pub holder_unallocated: u64,
    pub holder_reserved: u64,
    pub next_epoch: u64,
    pub total_settled: u64,
    pub total_platform: u64,
    pub bump: u8,
    /// Appended so earlier field offsets are unchanged; epochs must use newer snapshots.
    pub last_snapshot_slot: u64,
}

#[account]
pub struct RewardEpoch {
    pub fee_pool: Pubkey,
    pub number: u64,
    pub snapshot_slot: u64,
    pub snapshot_hash: [u8; 32],
    pub merkle_root: [u8; 32],
    pub funded_amount: u64,
    pub claimed_amount: u64,
    pub chunk_count: u32,
    pub paid_chunks: u32,
    pub bump: u8,
    pub paid_bitmap: Vec<u8>,
}

impl RewardEpoch {
    pub fn space(chunk_count: u32) -> usize {
        8 + 32 + 8 + 8 + 32 + 32 + 8 + 8 + 4 + 4 + 1 + 4 + bitmap_len(chunk_count)
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct PayoutEntry {
    pub token_balance: u64,
    pub reward: u64,
}

#[event]
pub struct SnapshotAuthorityChanged {
    pub previous: Pubkey,
    pub current: Pubkey,
    pub slot: u64,
}

#[event]
pub struct FeesSettled {
    pub mint: Pubkey,
    pub source: u8,
    pub distributable: u64,
    pub hymet: u64,
    pub creator: u64,
    pub holders: u64,
    pub slot: u64,
}

#[event]
pub struct HolderEpochFunded {
    pub mint: Pubkey,
    pub epoch: u64,
    pub epoch_account: Pubkey,
    pub snapshot_slot: u64,
    pub snapshot_hash: [u8; 32],
    pub merkle_root: [u8; 32],
    pub funded_amount: u64,
    pub chunk_count: u32,
    pub slot: u64,
}

#[event]
pub struct HolderPaid {
    pub mint: Pubkey,
    pub epoch: u64,
    pub chunk: u32,
    pub wallet: Pubkey,
    pub amount: u64,
    pub slot: u64,
}

#[event]
pub struct HolderEpochClosed {
    pub mint: Pubkey,
    pub epoch: u64,
    pub paid_amount: u64,
    pub returned_amount: u64,
    pub slot: u64,
}

#[event]
pub struct CreatorClaimed {
    pub mint: Pubkey,
    pub wallet: Pubkey,
    pub amount: u64,
    pub slot: u64,
}

#[event]
pub struct MigrationPoolRegistered {
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub slot: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Recipient configuration is invalid")]
    InvalidRecipient,
    #[msg("Token mint is not the fixed HYPE quote mint or is not supported")]
    InvalidMint,
    #[msg("Distribution split is outside 0-100 percent")]
    InvalidSplit,
    #[msg("Pool or migration state is invalid")]
    InvalidPool,
    #[msg("A DAMM migration pool is already registered")]
    AlreadyRegistered,
    #[msg("Fee CPI account list does not match the pinned Meteora interface")]
    InvalidCpiAccounts,
    #[msg("Fee vault account does not match this token pool")]
    InvalidVault,
    #[msg("A non-quote token fee was returned; settlement was rolled back")]
    NonQuoteFee,
    #[msg("The supplied token program is not the supported SPL Token program")]
    InvalidTokenProgram,
    #[msg("The supplied external program is not the pinned Meteora program")]
    InvalidProgram,
    #[msg("The DAMM position NFT is not held by the distributor PDA")]
    InvalidPosition,
    #[msg("Snapshot publisher is not authorized")]
    Unauthorized,
    #[msg("Reward epoch parameters are invalid")]
    InvalidEpoch,
    #[msg("The funded HYPE balance is insufficient")]
    InsufficientFunds,
    #[msg("Merkle proof is invalid")]
    InvalidProof,
    #[msg("Claim exceeds the funded epoch amount")]
    OverClaim,
    #[msg("No creator HYPE fees are claimable")]
    NothingToClaim,
    #[msg("Accounting overflow or invariant failure")]
    Accounting,
    #[msg("Vault does not cover all recorded liabilities")]
    Insolvent,
    #[msg("Only the program upgrade authority may initialize the distributor")]
    NotUpgradeAuthority,
    #[msg("This payout chunk was already paid")]
    AlreadyPaid,
    #[msg("Epoch is still paying out and has not expired")]
    EpochActive,
}

/// DAMM v2 pool PDA: ["pool", config, larger mint, smaller mint] (Meteora's getFirstKey/getSecondKey order).
pub fn canonical_damm_pool(token_mint: &Pubkey) -> Pubkey {
    let (first, second) = if token_mint.to_bytes() > HYPE_MINT.to_bytes() {
        (*token_mint, HYPE_MINT)
    } else {
        (HYPE_MINT, *token_mint)
    };
    Pubkey::find_program_address(
        &[b"pool", DAMM_V2_MIGRATION_CONFIG.as_ref(), first.as_ref(), second.as_ref()],
        &DAMM_V2_PROGRAM,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_accepts_only_the_canonical_migration_pool() {
        // Vectors from @meteora-ag/dynamic-bonding-curve-sdk deriveDammV2PoolAddress with Mainnet HYPE; both key orders.
        let trz = pubkey!("2Rrynqg8C6eSiFeJCf8XqVSfPpxdNoXFCv2juunkHoM6");
        let wsol = pubkey!("So11111111111111111111111111111111111111112");
        assert_eq!(canonical_damm_pool(&trz), pubkey!("FSx7CMySvPhUFgYcbLmq2Y6wpwft6TbEQvfPFtRQdtEv"));
        assert_eq!(canonical_damm_pool(&wsol), pubkey!("7w45HnbFD1EXT2SdUwpCKRb9o3obTyySMP98MUpuGKhE"));
        // The same pair under another (public, static) DAMM v2 config is a pool anyone could create: rejected.
        assert_ne!(canonical_damm_pool(&trz), pubkey!("65JjCV2MiiHRYjS36Rj7vnkgt3JS31Jg3NvqB5ehuoNz"));
        assert_ne!(canonical_damm_pool(&trz), canonical_damm_pool(&Pubkey::new_unique()));
    }

    fn fixture_info(
        key: Pubkey,
        owner: Pubkey,
        data: Vec<u8>,
        executable: bool,
    ) -> AccountInfo<'static> {
        AccountInfo::new(
            Box::leak(Box::new(key)),
            false,
            true,
            Box::leak(Box::new(10_000_000)),
            Box::leak(data.into_boxed_slice()),
            Box::leak(Box::new(owner)),
            executable,
            0,
        )
    }

    fn fixture_token(key: Pubkey, mint: Pubkey, authority: Pubkey) -> AccountInfo<'static> {
        let mut data = vec![0; anchor_spl::token::spl_token::state::Account::LEN];
        let state = anchor_spl::token::spl_token::state::Account {
            mint,
            owner: authority,
            amount: 100,
            state: anchor_spl::token::spl_token::state::AccountState::Initialized,
            ..Default::default()
        };
        anchor_spl::token::spl_token::state::Account::pack(state, &mut data).unwrap();
        fixture_info(key, anchor_spl::token::ID, data, false)
    }

    #[test]
    fn dbc_remaining_accounts_accept_token_program_at_index_nine_not_inserted_signer() {
        // Serialized synthetic account fixtures test validation only, not Meteora CPI execution.
        let mint = Pubkey::new_unique();
        let pool_key = Pubkey::find_program_address(&[b"fee-pool", mint.as_ref()], &crate::ID).0;
        let dbc_pool = Pubkey::new_unique();
        let value = FeePool {
            creator: Pubkey::new_unique(),
            token_mint: mint,
            dbc_pool,
            damm_pool: Pubkey::default(),
            creator_bps_of_remaining: 5000,
            creator_claimable: 0,
            holder_unallocated: 0,
            holder_reserved: 0,
            next_epoch: 0,
            total_settled: 0,
            total_platform: 0,
            bump: 0,
            last_snapshot_slot: 0,
        };
        let mut bytes = Vec::new();
        value.try_serialize(&mut bytes).unwrap();
        let pool_info = Box::leak(Box::new(fixture_info(pool_key, crate::ID, bytes, false)));
        let pool = Account::<FeePool>::try_from(pool_info).unwrap();
        let base_info = Box::leak(Box::new(fixture_token(
            Pubkey::new_unique(),
            mint,
            pool_key,
        )));
        let hype_info = Box::leak(Box::new(fixture_token(
            Pubkey::new_unique(),
            HYPE_MINT,
            pool_key,
        )));
        let base = Account::<TokenAccount>::try_from(base_info).unwrap();
        let hype = Account::<TokenAccount>::try_from(hype_info).unwrap();
        let authority = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
        let token_program = fixture_info(anchor_spl::token::ID, Pubkey::default(), vec![], true);
        let mut remaining = vec![
            fixture_info(authority, Pubkey::default(), vec![], false),
            fixture_info(Pubkey::new_unique(), DBC_PROGRAM, vec![], false),
            fixture_info(dbc_pool, DBC_PROGRAM, vec![], false),
            base_info.clone(),
            hype_info.clone(),
            fixture_token(Pubkey::new_unique(), mint, authority),
            fixture_token(Pubkey::new_unique(), HYPE_MINT, authority),
            fixture_info(mint, anchor_spl::token::ID, vec![], false),
            fixture_info(HYPE_MINT, anchor_spl::token::ID, vec![], false),
            token_program.clone(),
            token_program,
            fixture_info(
                Pubkey::find_program_address(&[b"__event_authority"], &DBC_PROGRAM).0,
                Pubkey::default(),
                vec![],
                false,
            ),
            fixture_info(DBC_PROGRAM, Pubkey::default(), vec![], true),
        ];
        assert!(validate_dbc_cpi_accounts(&remaining, &pool, &base, &hype).is_ok());
        remaining[9] = pool_info.clone();
        assert!(validate_dbc_cpi_accounts(&remaining, &pool, &base, &hype).is_err());
    }

    fn split(amount: u64, creator_bps: u16) -> (u64, u64, u64) {
        let platform = (amount as u128 * PLATFORM_BPS as u128 / BPS_DENOMINATOR as u128) as u64;
        let remainder = amount - platform;
        let creator = (remainder as u128 * creator_bps as u128 / BPS_DENOMINATOR as u128) as u64;
        (platform, creator, remainder - creator)
    }

    #[test]
    fn fee_split_conserves_every_base_unit_and_never_shares_platform_with_creator() {
        for amount in [0, 1, 4, 101, u32::MAX as u64, u64::MAX / 2] {
            let (platform, creator, holders) = split(amount, 5_000);
            assert_eq!(platform, (amount as u128 * 2_000 / 10_000) as u64);
            assert_eq!(platform + creator + holders, amount);
        }
        let (platform, creator, holders) = split(10_005, 10_000);
        assert_eq!((platform, creator, holders), (2_001, 8_004, 0));
    }

    fn entries(rewards: &[u64]) -> Vec<PayoutEntry> {
        rewards
            .iter()
            .map(|r| PayoutEntry {
                token_balance: r * 40,
                reward: *r,
            })
            .collect()
    }

    #[test]
    fn chunk_leaf_matches_the_typescript_planner() {
        // Same vector as lib/holder-payouts.mjs chunkLeaf (see tests/holder-payouts.test.ts inputs).
        let mint = Pubkey::new_from_array([7; 32]);
        let wallets = [Pubkey::new_from_array([1; 32]), Pubkey::new_from_array([2; 32])];
        let rows = vec![
            PayoutEntry { token_balance: 1000, reward: 25 },
            PayoutEntry { token_balance: 2000, reward: 50 },
        ];
        let expected = "d364bea020c6939853ac56c695df8f26a1df3e843d27cb2164f1a8493e6e3796";
        let leaf = chunk_leaf(&mint, 5, 3, &wallets, &rows);
        let hex: String = leaf.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn chunk_leaf_binds_epoch_chunk_wallets_balances_and_rewards() {
        let mint = Pubkey::new_unique();
        let wallets = [Pubkey::new_unique(), Pubkey::new_unique()];
        let rows = entries(&[10, 20]);
        let leaf = chunk_leaf(&mint, 3, 0, &wallets, &rows);
        assert_ne!(leaf, chunk_leaf(&mint, 4, 0, &wallets, &rows));
        assert_ne!(leaf, chunk_leaf(&mint, 3, 1, &wallets, &rows));
        assert_ne!(leaf, chunk_leaf(&mint, 3, 0, &[wallets[1], wallets[0]], &rows));
        assert_ne!(leaf, chunk_leaf(&mint, 3, 0, &wallets, &entries(&[10, 21])));
        assert_ne!(leaf, chunk_leaf(&mint, 3, 0, &wallets[..1], &rows[..1]));
    }

    #[test]
    fn sorted_pair_proof_verifies_only_the_committed_chunk() {
        let a = [1u8; 32];
        let b = [9u8; 32];
        let c = [5u8; 32];
        let pair = |x: [u8; 32], y: [u8; 32]| {
            if x <= y {
                hashv(&[&x, &y]).to_bytes()
            } else {
                hashv(&[&y, &x]).to_bytes()
            }
        };
        let root = pair(pair(a, b), c);
        assert_eq!(verify_proof(a, &[b, c]), root);
        assert_eq!(verify_proof(b, &[a, c]), root);
        assert_ne!(verify_proof(c, &[a, b]), root);
        assert_eq!(verify_proof(root, &[]), root);
    }

    #[test]
    fn bitmap_marks_each_chunk_once_and_epoch_space_tracks_chunks() {
        let mut bitmap = vec![0u8; bitmap_len(MAX_EPOCH_CHUNKS)];
        assert_eq!(bitmap.len(), 128);
        for chunk in [0, 7, 8, 1023] {
            assert!(!is_chunk_paid(&bitmap, chunk));
            mark_chunk_paid(&mut bitmap, chunk);
            assert!(is_chunk_paid(&bitmap, chunk));
        }
        assert!(!is_chunk_paid(&bitmap, 1));
        assert_eq!(bitmap_len(1), 1);
        assert_eq!(bitmap_len(9), 2);
        assert_eq!(RewardEpoch::space(1), 150);
        let mut bytes = Vec::new();
        RewardEpoch {
            fee_pool: Pubkey::default(),
            number: 0,
            snapshot_slot: 0,
            snapshot_hash: [0; 32],
            merkle_root: [0; 32],
            funded_amount: 0,
            claimed_amount: 0,
            chunk_count: 9,
            paid_chunks: 0,
            bump: 0,
            paid_bitmap: vec![0; bitmap_len(9)],
        }
        .try_serialize(&mut bytes)
        .unwrap();
        assert_eq!(bytes.len(), RewardEpoch::space(9));
    }

    #[test]
    fn epochs_close_only_when_fully_paid_or_expired() {
        assert!(can_close_epoch(4, 4, 100, 101));
        assert!(!can_close_epoch(3, 4, 100, 100 + EPOCH_EXPIRY_SLOTS - 1));
        assert!(can_close_epoch(3, 4, 100, 100 + EPOCH_EXPIRY_SLOTS));
        assert!(!can_close_epoch(0, 1, u64::MAX, u64::MAX));
    }

    #[test]
    fn epoch_funding_requires_existing_unallocated_hype() {
        assert!(can_fund_epoch(100, 100));
        assert!(!can_fund_epoch(99, 100));
        assert!(!can_fund_epoch(100, 0));
    }

    #[test]
    fn claim_total_cannot_exceed_funded_epoch_even_across_wallets() {
        assert!(can_claim(100, 60, 40));
        assert!(!can_claim(100, 60, 41));
        assert!(!can_claim(100, u64::MAX, 1));
        assert!(!can_claim(100, 0, 0));
    }

    #[test]
    fn snapshot_authority_rotation_keeps_roles_separate() {
        let current = Pubkey::new_unique();
        let platform = Pubkey::new_unique();
        let upgrade = Pubkey::new_unique();
        let next = Pubkey::new_unique();
        assert!(valid_new_snapshot_authority(next, current, platform, upgrade));
        assert!(!valid_new_snapshot_authority(Pubkey::default(), current, platform, upgrade));
        assert!(!valid_new_snapshot_authority(current, current, platform, upgrade));
        assert!(!valid_new_snapshot_authority(platform, current, platform, upgrade));
        assert!(!valid_new_snapshot_authority(upgrade, current, platform, upgrade));
    }

    #[test]
    fn snapshot_authority_is_exact() {
        let authority = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        assert!(is_snapshot_authority(authority, authority));
        assert!(!is_snapshot_authority(authority, other));
    }
}
