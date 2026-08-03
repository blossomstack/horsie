//! The routine timer: a clock on top of [`RoutineRunner`].
//!
//! It owns no schedule logic of its own — it asks the service which routines
//! have come due, works out where each one's next firing lands, and hands both
//! to the runner. Ticking is a plain method taking `now_ms` so tests drive time
//! rather than sleep through it.

use crate::routines::runner::RoutineRunner;
use crate::routines::service::{RoutineService, next_run_at};
use std::sync::Arc;
use std::time::Duration;

/// How often the timer looks for due routines. Well under the 60s minimum
/// interval, so a routine fires within a tick of when it was due.
pub const TICK_INTERVAL: Duration = Duration::from_secs(15);

pub struct RoutineScheduler {
    runner: Arc<RoutineRunner>,
    routines: Arc<RoutineService>,
}

impl RoutineScheduler {
    pub fn new(runner: Arc<RoutineRunner>, routines: Arc<RoutineService>) -> Self {
        Self { runner, routines }
    }

    /// Fire every routine due at `now_ms`.
    ///
    /// Each one is *claimed first* — its timer moved to the next firing before
    /// the run is started. Two things follow, both deliberate: a run that takes
    /// longer than a tick cannot be found still-due and started twice, and a
    /// routine whose vendor is offline waits out its interval instead of being
    /// retried on every tick.
    pub async fn tick(&self, now_ms: u64) {
        let due = match self.routines.due(now_ms).await {
            Ok(due) => due,
            Err(e) => {
                tracing::error!(error = %e, "reading due routines failed");
                return;
            }
        };
        for routine in due {
            let next = next_run_at(&routine.schedule, routine.enabled, now_ms);
            if let Err(e) = self.routines.arm(&routine.name, next).await {
                // Unclaimed, so skipped: running it now would leave it due and
                // firing again on the next tick, and every tick after that.
                tracing::error!(routine = %routine.name, error = %e, "arming a routine failed");
                continue;
            }
            if let Err(e) = self.runner.run(&routine.name, now_ms).await {
                tracing::warn!(routine = %routine.name, error = %e, "routine run did not start");
            }
        }
    }

    /// Run the timer until the process ends.
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK_INTERVAL);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                self.tick(horsie_models::now_ms()).await;
            }
        });
    }
}
