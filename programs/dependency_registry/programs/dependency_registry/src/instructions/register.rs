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

    pub authority: Signer<'info>,

    #[account(
        constraint = program_data.key() == bpf_loader_upgradeable::get_program_data_address(&program_id)
            @ ErrorCode::ProgramDataMismatch,
        constraint = program_data.upgrade_authority_address == Some(authority.key())
            @ ErrorCode::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,

    #[account(
        init,
        payer = payer,
        space = 8 + Protocol::INIT_SPACE,
        seeds = [PROTOCOL_SEED, program_id.as_ref()],
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

    let protocol = &mut ctx.accounts.protocol;
    protocol.authority = ctx.accounts.authority.key();
    protocol.program_id = program_id;
    protocol.alert_url = alert_url;
    protocol.corpus_size = corpus_size;
    protocol.dependency_count = 0;
    protocol.bump = ctx.bumps.protocol;

    Ok(())
}
