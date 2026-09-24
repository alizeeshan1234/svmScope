use anchor_lang::prelude::*;

use crate::constants::PROTOCOL_SEED;
use crate::error::ErrorCode;
use crate::state::Protocol;

#[derive(Accounts)]
pub struct TransferAuthority<'info> {
    pub authority: Signer<'info>,

    /// CHECK: any key may become the new authority; it need not sign.
    pub new_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub protocol: Account<'info, Protocol>,
}

pub fn handle_transfer_authority(ctx: Context<TransferAuthority>) -> Result<()> {
    ctx.accounts.protocol.authority = ctx.accounts.new_authority.key();
    Ok(())
}
