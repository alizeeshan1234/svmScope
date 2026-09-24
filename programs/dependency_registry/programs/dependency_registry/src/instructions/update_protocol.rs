use anchor_lang::prelude::*;

use crate::constants::{MAX_ALERT_URL_LEN, PROTOCOL_SEED};
use crate::error::ErrorCode;
use crate::state::Protocol;

#[derive(Accounts)]
pub struct UpdateProtocol<'info> {
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PROTOCOL_SEED, protocol.program_id.as_ref(), protocol.authority.as_ref()],
        bump = protocol.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub protocol: Account<'info, Protocol>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateParameters {
    pub alert_url: Option<String>,
    pub corpus_size: Option<u16>,
}

pub fn handle_update_protocol(ctx: Context<UpdateProtocol>, params: UpdateParameters) -> Result<()> {
    let protocol = &mut ctx.accounts.protocol;

    if let Some(alert_url) = params.alert_url {
        require!(alert_url.len() <= MAX_ALERT_URL_LEN, ErrorCode::AlertUrlTooLong);
        protocol.alert_url = alert_url;
    }
    if let Some(corpus_size) = params.corpus_size {
        protocol.corpus_size = corpus_size;
    }

    Ok(())
}
