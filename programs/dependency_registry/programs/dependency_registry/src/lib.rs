pub mod constants;
pub mod error;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;

pub use constants::*;
pub use instructions::*;
pub use state::*;

declare_id!("4nH59dWUJ5rgTZJTybPbfGY1sgBDwKgrKMBXpRtdxhhg");

#[program]
pub mod dependency_registry {
    use super::*;

    pub fn register(
        ctx: Context<Register>,
        program_id: Pubkey,
        alert_url: String,
        corpus_size: u16,
    ) -> Result<()> {
        handle_register(ctx, program_id, alert_url, corpus_size)
    }

    pub fn add_dependency(ctx: Context<AddDependency>, dependency_program_id: Pubkey) -> Result<()> {
        handle_add_dependency(ctx, dependency_program_id)
    }

    pub fn remove_dependency(ctx: Context<RemoveDependency>) -> Result<()> {
        handle_remove_dependency(ctx)
    }

    pub fn set_alerts(ctx: Context<SetAlerts>, alerts_enabled: bool) -> Result<()> {
        handle_set_alerts(ctx, alerts_enabled)
    }

    pub fn update_protocol(ctx: Context<UpdateProtocol>, params: UpdateParameters) -> Result<()> {
        handle_update_protocol(ctx, params)
    }

    pub fn unregister(ctx: Context<Unregister>) -> Result<()> {
        handle_unregister(ctx)
    }
}
