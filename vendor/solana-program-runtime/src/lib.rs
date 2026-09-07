#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::indexing_slicing)]

pub use solana_sbpf;
pub mod cpi;
pub mod deploy;
pub mod execution_budget;
pub mod invoke_context;
/// svmscope patch: a thread-local observer called after every instruction at
/// every depth (top-level and CPI) has executed, before its frame pops. The
/// hook receives the invoke context — whose transaction context still holds
/// the finished instruction's frame and account state — and whether it
/// succeeded. Install it on the thread that will run the transaction.
pub mod instruction_hook {
    use crate::invoke_context::InvokeContext;
    use std::cell::RefCell;

    /// The observer signature.
    pub type Hook = Box<dyn for<'a, 'b> FnMut(&InvokeContext<'a, 'b>, bool)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Install `hook` for the current thread (replacing any previous one).
    pub fn set(hook: Hook) {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }

    /// Remove the current thread's hook.
    pub fn clear() {
        HOOK.with(|h| *h.borrow_mut() = None);
    }

    pub(crate) fn fire(ctx: &InvokeContext<'_, '_>, ok: bool) {
        HOOK.with(|h| {
            if let Ok(mut guard) = h.try_borrow_mut() {
                if let Some(f) = guard.as_mut() {
                    f(ctx, ok);
                }
            }
        });
    }
}
pub mod loaded_programs;
pub mod loading_task;
pub mod mem_pool;
pub mod memory;
pub mod memory_context;
pub mod program_cache_entry;
pub mod program_metrics;
pub mod serialization;
pub mod stable_log;
pub mod sysvar_cache;
pub mod vm;

// re-exports for macros
pub mod __private {
    pub use {
        crate::vm::{MEMORY_POOL, calculate_heap_cost, create_vm},
        solana_account::ReadableAccount,
        solana_hash::Hash,
        solana_instruction::error::InstructionError,
        solana_rent::Rent,
        solana_transaction_context::transaction::TransactionContext,
    };
}
