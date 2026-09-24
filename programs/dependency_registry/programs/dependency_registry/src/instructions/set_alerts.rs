use anchor_lang::prelude::*;

use crate::constants::{DEPENDENCY_SEED, PROTOCOL_SEED};
use crate::error::ErrorCode;
use crate::state::{Dependency, Protocol};

#[derive(Accounts)]
pub struct SetAlerts<'info> {
    pub authority: Signer<'info>,

    #[account(
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub protocol: Account<'info, Protocol>,

    #[account(
        mut,
        seeds = [DEPENDENCY_SEED, protocol.key().as_ref(), dependency.program_id.as_ref()],
        bump = dependency.bump,
        has_one = protocol,
    )]
    pub dependency: Account<'info, Dependency>,
}

pub fn handle_set_alerts(ctx: Context<SetAlerts>, alerts_enabled: bool) -> Result<()> {
    ctx.accounts.dependency.alerts_enabled = alerts_enabled;
    Ok(())
}
