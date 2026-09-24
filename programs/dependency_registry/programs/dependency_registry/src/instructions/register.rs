use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;

use crate::constants::{MAX_ALERT_URL_LEN, PROTOCOL_SEED};
use crate::error::ErrorCode;
use crate::state::Protocol;

#[derive(Accounts)]
#[instruction(program_id: Pubkey)]
pub struct Register<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// Becomes the protocol authority. With `program_data` present it must be
    /// the program's upgrade authority on this cluster.
    pub authority: Signer<'info>,

    /// The ProgramData account of `program_id`, proving who controls it.
    /// Optional: a program that lives only on another cluster has none here,
    /// and the engine verifies the entry against that cluster instead.
    pub program_data: Option<Account<'info, ProgramData>>,

    #[account(
        init,
        payer = payer,
        space = 8 + Protocol::INIT_SPACE,
        seeds = [PROTOCOL_SEED, program_id.as_ref(), authority.key().as_ref()],
        bump,
    )]
    pub protocol: Account<'info, Protocol>,

    pub system_program: Program<'info, System>,
}

pub fn handle_register(
    ctx: Context<Register>,
    program_id: Pubkey,
    alert_url: String,
    corpus_size: u16,
) -> Result<()> {
    require!(alert_url.len() <= MAX_ALERT_URL_LEN, ErrorCode::AlertUrlTooLong);

    let proven = match ctx.accounts.program_data.as_ref() {
        Some(program_data) => {
            require_keys_eq!(
                program_data.key(),
                bpf_loader_upgradeable::get_program_data_address(&program_id),
                ErrorCode::ProgramDataMismatch
            );
            require!(
                program_data.upgrade_authority_address == Some(ctx.accounts.authority.key()),
                ErrorCode::NotUpgradeAuthority
            );
            true
        }
        None => false,
    };

    let protocol = &mut ctx.accounts.protocol;
    protocol.authority = ctx.accounts.authority.key();
    protocol.program_id = program_id;
    protocol.alert_url = alert_url;
    protocol.corpus_size = corpus_size;
    protocol.dependency_count = 0;
    protocol.proven = proven;
    protocol.bump = ctx.bumps.protocol;

    Ok(())
}
