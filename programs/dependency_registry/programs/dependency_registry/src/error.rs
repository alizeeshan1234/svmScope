use anchor_lang::prelude::*;

#[error_code]
pub enum ErrorCode {
    #[msg("Signer is not the protocol authority")]
    Unauthorized,
    #[msg("Signer is not the upgrade authority of the program being registered")]
    NotUpgradeAuthority,
    #[msg("The program data account does not belong to the program being registered")]
    ProgramDataMismatch,
    #[msg("alert_url exceeds the maximum length")]
    AlertUrlTooLong,
    #[msg("The protocol already has the maximum number of dependencies")]
    TooManyDependencies,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Remove the protocol's dependencies before unregistering it")]
    HasDependencies,
}
