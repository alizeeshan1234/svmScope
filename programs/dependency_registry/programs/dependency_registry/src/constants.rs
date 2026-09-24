use anchor_lang::prelude::*;

#[constant]
pub const PROTOCOL_SEED: &[u8] = b"protocol";

#[constant]
pub const DEPENDENCY_SEED: &[u8] = b"dependency";

pub const MAX_ALERT_URL_LEN: usize = 128;

#[constant]
pub const MAX_DEPENDENCIES: u8 = 32;
