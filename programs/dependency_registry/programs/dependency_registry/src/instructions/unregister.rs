use anchor_lang::prelude::*;

use crate::constants::PROTOCOL_SEED;
use crate::error::ErrorCode;
use crate::state::Protocol;

#[derive(Accounts)]
pub struct Unregister<'info> {
    /// Receives the closed account's rent.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        close = authority,
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref(), protocol.authority.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
        constraint = protocol.dependency_count == 0 @ ErrorCode::HasDependencies,
    )]
    pub protocol: Account<'info, Protocol>,
}

pub fn handle_unregister(_ctx: Context<Unregister>) -> Result<()> {
    Ok(())
}
