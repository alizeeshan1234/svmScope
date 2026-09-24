use anchor_lang::prelude::*;

use crate::constants::MAX_ALERT_URL_LEN;

/// One registered protocol. PDA on `["protocol", program_id, authority]`,
/// so several parties may register the same program and only the entry
/// whose authority holds the program's upgrade authority is the one the
/// engine trusts. The authority is a seed and cannot change; to hand over,
/// register anew and `unregister` the old entry.
#[account]
#[derive(Debug, InitSpace)]
pub struct Protocol {
    pub authority: Pubkey,
    pub program_id: Pubkey,
    #[max_len(MAX_ALERT_URL_LEN)]
    pub alert_url: String,
    pub corpus_size: u16,
    pub dependency_count: u8,
    /// Whether `register` was given the program data account and checked the
    /// upgrade authority on this cluster. A program that lives only on
    /// another cluster registers without it; the engine verifies there.
    pub proven: bool,
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
