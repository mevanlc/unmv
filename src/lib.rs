//! Planning and executing best-effort, no-overwrite filesystem move reversals.

mod context;
mod engine;
mod plan;
mod platform;

pub use context::{Context, Shell, shell_integration};
pub use engine::{Event, ExecutionOptions, Summary, execute, execute_with};
pub use plan::{Action, Inference, Mode, MvKind, Plan, infer};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cooperative cancellation keeps copied sources intact until verification completes.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.is_cancelled(), "operation cancelled");
        Ok(())
    }
}
