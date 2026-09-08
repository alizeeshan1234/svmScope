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

    /// Whether the instruction is about to run or has just finished.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Phase {
        /// The frame is pushed; nothing of the instruction has executed yet.
        Enter,
        /// The instruction has executed and its frame has been popped;
        /// `ok` is its real verdict, frame-exit failures included.
        Exit {
            /// Whether the instruction succeeded.
            ok: bool,
        },
    }

    /// The frame an event is about: its stack height (1 = top-level) and
    /// program. Captured before the frame is popped, so it is valid on `Exit`
    /// even though the instruction context is gone by then.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Frame {
        /// Stack height of the instruction (1 = top-level, 2 = its CPI, …).
        pub stack_height: usize,
        /// The program that ran, when the context could name it.
        pub program: Option<solana_pubkey::Pubkey>,
    }

    /// The observer signature.
    pub type Hook = Box<dyn for<'a, 'b> FnMut(&InvokeContext<'a, 'b>, Phase, Frame)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Removes the thread's hook when dropped, so a panic or early return in
    /// the caller cannot leave a stale observer installed for later
    /// transactions on the same thread.
    pub struct HookGuard(());

    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOK.with(|h| {
                if let Ok(mut g) = h.try_borrow_mut() {
                    *g = None;
                }
            });
        }
    }

    /// Install `hook` for the current thread for the lifetime of the returned
    /// guard, replacing any previous one.
    #[must_use = "the hook is removed when the guard is dropped"]
    pub fn install(hook: Hook) -> HookGuard {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
        HookGuard(())
    }

    /// The hook is taken out of the cell for the duration of the call, so a
    /// hook may install or remove hooks without a re-entrant borrow; a
    /// re-entrant `fire` (the runtime does not do this) sees no hook.
    pub(crate) fn fire(ctx: &InvokeContext<'_, '_>, phase: Phase, frame: Frame) {
        let taken = HOOK.with(|h| h.try_borrow_mut().ok().and_then(|mut g| g.take()));
        if let Some(mut f) = taken {
            f(ctx, phase, frame);
            HOOK.with(|h| {
                if let Ok(mut g) = h.try_borrow_mut() {
                    if g.is_none() {
                        *g = Some(f);
                    }
                }
            });
        }
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
