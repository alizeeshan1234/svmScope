use anchor_lang::prelude::*;

use crate::constants::MAX_ALERT_URL_LEN;

#[account]
#[derive(Debug, InitSpace)]
pub struct Protocol {
    pub authority: Pubkey,
    pub program_id: Pubkey,
    #[max_len(MAX_ALERT_URL_LEN)]
    pub alert_url: String,
    pub corpus_size: u16,
    pub dependency_count: u8,
    pub bump: u8,
}

#[account]
#[derive(Debug, InitSpace)]
pub struct Dependency {
    pub protocol: Pubkey,
    pub program_id: Pubkey,
    pub last_checked_slot: u64,
    pub alerts_enabled: bool,
    pub bump: u8,
}
