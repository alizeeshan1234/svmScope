use anchor_lang::prelude::*;

use crate::constants::{DEPENDENCY_SEED, PROTOCOL_SEED};
use crate::error::ErrorCode;
use crate::state::{Dependency, Protocol};

#[derive(Accounts)]
pub struct RemoveDependency<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub protocol: Account<'info, Protocol>,

    #[account(
        mut,
        close = authority,
        seeds = [DEPENDENCY_SEED, protocol.key().as_ref(), dependency.program_id.as_ref()],
        bump = dependency.bump,
        has_one = protocol,
    )]
    pub dependency: Account<'info, Dependency>,
}

pub fn handle_remove_dependency(ctx: Context<RemoveDependency>) -> Result<()> {
    let protocol = &mut ctx.accounts.protocol;
    protocol.dependency_count = protocol
        .dependency_count
        .checked_sub(1)
        .ok_or(ErrorCode::Overflow)?;
    Ok(())
}
