use anchor_lang::prelude::*;

use crate::constants::{DEPENDENCY_SEED, MAX_DEPENDENCIES, PROTOCOL_SEED};
use crate::error::ErrorCode;
use crate::state::{Dependency, Protocol};

#[derive(Accounts)]
#[instruction(dependency_program_id: Pubkey)]
pub struct AddDependency<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref(), protocol.authority.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub protocol: Account<'info, Protocol>,

    #[account(
        init,
        payer = payer,
        space = 8 + Dependency::INIT_SPACE,
        seeds = [DEPENDENCY_SEED, protocol.key().as_ref(), dependency_program_id.as_ref()],
        bump,
    )]
    pub dependency: Account<'info, Dependency>,

    pub system_program: Program<'info, System>,
}

pub fn handle_add_dependency(ctx: Context<AddDependency>, dependency_program_id: Pubkey) -> Result<()> {
    let protocol = &mut ctx.accounts.protocol;
    require!(
        protocol.dependency_count < MAX_DEPENDENCIES,
        ErrorCode::TooManyDependencies
    );
    protocol.dependency_count = protocol
        .dependency_count
        .checked_add(1)
        .ok_or(ErrorCode::Overflow)?;

    let dependency = &mut ctx.accounts.dependency;
    dependency.protocol = protocol.key();
    dependency.program_id = dependency_program_id;
    dependency.last_checked_slot = 0;
    dependency.alerts_enabled = true;
    dependency.bump = ctx.bumps.dependency;

    Ok(())
}
