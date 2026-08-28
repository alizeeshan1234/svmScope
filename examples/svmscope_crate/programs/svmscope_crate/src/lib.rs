use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

declare_id!("41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u");

const MAX_VESTING_DURATION_SECONDS: i64 = 10 * 365 * 24 * 60 * 60;

#[program]
pub mod svmscope_crate {
    use super::*;

    pub fn initialize_counter(ctx: Context<InitializeCounter>) -> Result<()> {
        ctx.accounts.counter.count = 0;
        msg!("Counter initialized successfully!");
        Ok(())
    }

    pub fn increment_counter(ctx: Context<IncrementCounter>) -> Result<()> {
        let counter = &mut ctx.accounts.counter;
        counter.count = counter
            .count
            .checked_add(1)
            .ok_or(VestingError::MathOverflow)?;
        msg!("Count incremented by 1 to : {}", counter.count);
        Ok(())
    }

    /// Create a linear SOL vesting schedule and escrow `amount` lamports in the
    /// program-owned schedule account. Vesting accrues from `start_ts`, but no
    /// claim is permitted before `cliff_ts`.
    pub fn create_vesting(
        ctx: Context<CreateVesting>,
        schedule_id: u64,
        amount: u64,
        start_ts: i64,
        cliff_ts: i64,
        end_ts: i64,
    ) -> Result<()> {
        require!(amount > 0, VestingError::InvalidAmount);
        require!(
            start_ts <= cliff_ts && cliff_ts < end_ts,
            VestingError::InvalidSchedule
        );
        let duration = end_ts
            .checked_sub(start_ts)
            .ok_or(VestingError::MathOverflow)?;
        require!(duration > 0, VestingError::InvalidSchedule);
        require!(
            duration <= MAX_VESTING_DURATION_SECONDS,
            VestingError::ScheduleTooLong
        );

        let schedule = &mut ctx.accounts.schedule;
        schedule.creator = ctx.accounts.creator.key();
        schedule.beneficiary = ctx.accounts.beneficiary.key();
        schedule.schedule_id = schedule_id;
        schedule.start_ts = start_ts;
        schedule.cliff_ts = cliff_ts;
        schedule.end_ts = end_ts;
        schedule.total_amount = amount;
        schedule.claimed_amount = 0;
        schedule.bump = ctx.bumps.schedule;

        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.creator.to_account_info(),
                    to: schedule.to_account_info(),
                },
            ),
            amount,
        )?;

        emit!(VestingCreated {
            schedule: schedule.key(),
            creator: schedule.creator,
            beneficiary: schedule.beneficiary,
            schedule_id,
            amount,
            start_ts,
            cliff_ts,
            end_ts,
        });
        msg!(
            "Vesting schedule {} created for {} lamports",
            schedule_id,
            amount
        );
        Ok(())
    }

    /// Claim every lamport vested at the current Clock timestamp and not
    /// previously claimed. A pre-cliff call deliberately lands as a program
    /// failure, making it an ideal svmscope time-travel transaction.
    pub fn claim_vested(ctx: Context<ClaimVested>, schedule_id: u64) -> Result<()> {
        let _ = schedule_id;
        let now = Clock::get()?.unix_timestamp;
        let schedule = &mut ctx.accounts.schedule;
        require!(now >= schedule.cliff_ts, VestingError::CliffNotReached);

        let vested = schedule.vested_amount(now)?;
        let claimable = vested
            .checked_sub(schedule.claimed_amount)
            .ok_or(VestingError::MathOverflow)?;
        require!(claimable > 0, VestingError::NothingToClaim);

        let schedule_info = schedule.to_account_info();
        let beneficiary_info = ctx.accounts.beneficiary.to_account_info();
        let rent_reserve = Rent::get()?.minimum_balance(8 + VestingSchedule::INIT_SPACE);
        let remaining = schedule_info
            .lamports()
            .checked_sub(claimable)
            .ok_or(VestingError::EscrowUnderfunded)?;
        require!(remaining >= rent_reserve, VestingError::EscrowUnderfunded);

        schedule.claimed_amount = schedule
            .claimed_amount
            .checked_add(claimable)
            .ok_or(VestingError::MathOverflow)?;
        **schedule_info.try_borrow_mut_lamports()? = remaining;
        **beneficiary_info.try_borrow_mut_lamports()? = beneficiary_info
            .lamports()
            .checked_add(claimable)
            .ok_or(VestingError::MathOverflow)?;

        emit!(VestingClaimed {
            schedule: schedule.key(),
            beneficiary: schedule.beneficiary,
            amount: claimable,
            claimed_total: schedule.claimed_amount,
            timestamp: now,
        });
        msg!(
            "Beneficiary claimed {} lamports ({} total)",
            claimable,
            schedule.claimed_amount
        );
        Ok(())
    }

    /// Recover the schedule account's rent after every vested lamport has been
    /// claimed. Anchor sends the remaining rent reserve back to the creator.
    pub fn close_vesting(ctx: Context<CloseVesting>, schedule_id: u64) -> Result<()> {
        let _ = schedule_id;
        require!(
            ctx.accounts.schedule.claimed_amount == ctx.accounts.schedule.total_amount,
            VestingError::NotFullyClaimed
        );
        msg!("Fully claimed vesting schedule closed");
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeCounter<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(
        init,
        payer = signer,
        space = 8 + Counter::INIT_SPACE,
        seeds = [b"counter"],
        bump
    )]
    pub counter: Account<'info, Counter>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct IncrementCounter<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(mut, seeds = [b"counter"], bump)]
    pub counter: Account<'info, Counter>,
}

#[derive(Accounts)]
#[instruction(schedule_id: u64)]
pub struct CreateVesting<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    /// CHECK: The beneficiary need not sign when a schedule is created. Its key
    /// is stored and enforced as the signer by `ClaimVested`.
    pub beneficiary: UncheckedAccount<'info>,

    #[account(
        init,
        payer = creator,
        space = 8 + VestingSchedule::INIT_SPACE,
        seeds = [b"vesting", beneficiary.key().as_ref(), &schedule_id.to_le_bytes()],
        bump
    )]
    pub schedule: Account<'info, VestingSchedule>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(schedule_id: u64)]
pub struct ClaimVested<'info> {
    #[account(mut)]
    pub beneficiary: Signer<'info>,

    #[account(
        mut,
        has_one = beneficiary,
        seeds = [b"vesting", beneficiary.key().as_ref(), &schedule_id.to_le_bytes()],
        bump = schedule.bump
    )]
    pub schedule: Account<'info, VestingSchedule>,
}

#[derive(Accounts)]
#[instruction(schedule_id: u64)]
pub struct CloseVesting<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        mut,
        has_one = creator,
        seeds = [b"vesting", schedule.beneficiary.as_ref(), &schedule_id.to_le_bytes()],
        bump = schedule.bump,
        close = creator
    )]
    pub schedule: Account<'info, VestingSchedule>,
}

#[account]
#[derive(Debug, InitSpace)]
pub struct Counter {
    pub count: u64,
}

#[account]
#[derive(Debug, InitSpace)]
pub struct VestingSchedule {
    pub creator: Pubkey,
    pub beneficiary: Pubkey,
    pub schedule_id: u64,
    pub start_ts: i64,
    pub cliff_ts: i64,
    pub end_ts: i64,
    pub total_amount: u64,
    pub claimed_amount: u64,
    pub bump: u8,
}

impl VestingSchedule {
    pub fn vested_amount(&self, now: i64) -> Result<u64> {
        if now < self.start_ts {
            return Ok(0);
        }
        if now >= self.end_ts {
            return Ok(self.total_amount);
        }

        let elapsed = now
            .checked_sub(self.start_ts)
            .ok_or(VestingError::MathOverflow)? as u128;
        let duration = self
            .end_ts
            .checked_sub(self.start_ts)
            .ok_or(VestingError::MathOverflow)? as u128;
        let vested = (self.total_amount as u128)
            .checked_mul(elapsed)
            .ok_or(VestingError::MathOverflow)?
            .checked_div(duration)
            .ok_or(VestingError::MathOverflow)?;
        u64::try_from(vested).map_err(|_| VestingError::MathOverflow.into())
    }
}

#[event]
pub struct VestingCreated {
    pub schedule: Pubkey,
    pub creator: Pubkey,
    pub beneficiary: Pubkey,
    pub schedule_id: u64,
    pub amount: u64,
    pub start_ts: i64,
    pub cliff_ts: i64,
    pub end_ts: i64,
}

#[event]
pub struct VestingClaimed {
    pub schedule: Pubkey,
    pub beneficiary: Pubkey,
    pub amount: u64,
    pub claimed_total: u64,
    pub timestamp: i64,
}

#[error_code]
pub enum VestingError {
    #[msg("The vesting amount must be greater than zero")]
    InvalidAmount,
    #[msg("Expected start <= cliff < end")]
    InvalidSchedule,
    #[msg("Vesting duration exceeds the ten-year safety limit")]
    ScheduleTooLong,
    #[msg("The vesting cliff has not been reached")]
    CliffNotReached,
    #[msg("No additional lamports are vested yet")]
    NothingToClaim,
    #[msg("Arithmetic overflow while calculating vesting")]
    MathOverflow,
    #[msg("The vesting escrow does not contain enough lamports")]
    EscrowUnderfunded,
    #[msg("The vesting schedule cannot close until it is fully claimed")]
    NotFullyClaimed,
}
