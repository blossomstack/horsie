//! Ordered, strictly-exhausting programmed outcomes for test doubles.
//!
//! The point is the exhaustion behaviour: running past the end is an error, never
//! a wrap-around. A double that cycles turns "my test over-ran its script" into a
//! silent repeated response, which is exactly how iteration-count bugs hide.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Returned when a [`Script`] is asked for a step it does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("script '{label}' exhausted after {taken} step(s)")]
pub struct ScriptExhausted {
    pub label: &'static str,
    pub taken: usize,
}

/// An ordered list of programmed outcomes, consumed once.
pub struct Script<T> {
    label: &'static str,
    steps: Mutex<VecDeque<T>>,
    repeating: Option<Box<dyn Fn() -> T + Send + Sync>>,
    taken: AtomicUsize,
}

impl<T> Script<T> {
    /// A script that yields `steps` in order, then errors.
    pub fn of(steps: impl IntoIterator<Item = T>) -> Self {
        Self {
            label: "script",
            steps: Mutex::new(steps.into_iter().collect()),
            repeating: None,
            taken: AtomicUsize::new(0),
        }
    }

    /// A one-step script.
    pub fn once(step: T) -> Self {
        Self::of([step])
    }

    /// Name the script so `ScriptExhausted` says which one ran out.
    #[must_use]
    pub fn labelled(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    /// After the scripted steps, keep yielding values built by `f`. Opting into a
    /// steady state has to be said out loud — it is not the default.
    #[must_use]
    pub fn then_repeating_with(mut self, f: impl Fn() -> T + Send + Sync + 'static) -> Self {
        self.repeating = Some(Box::new(f));
        self
    }

    /// Take the next programmed outcome.
    pub fn next_step(&self) -> Result<T, ScriptExhausted> {
        let next = self
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(step) => {
                self.taken.fetch_add(1, Ordering::Relaxed);
                Ok(step)
            }
            None => match &self.repeating {
                Some(f) => {
                    self.taken.fetch_add(1, Ordering::Relaxed);
                    Ok(f())
                }
                None => Err(ScriptExhausted {
                    label: self.label,
                    taken: self.taken.load(Ordering::Relaxed),
                }),
            },
        }
    }

    /// How many steps have been consumed.
    pub fn taken(&self) -> usize {
        self.taken.load(Ordering::Relaxed)
    }
}

impl<T: Clone + Send + Sync + 'static> Script<T> {
    /// Sugar over [`Script::then_repeating_with`] for cloneable values.
    #[must_use]
    pub fn then_repeating(self, steady: T) -> Self {
        self.then_repeating_with(move || steady.clone())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn returns_steps_in_order() {
        let s = Script::of([1, 2, 3]);
        assert_eq!(s.next_step().unwrap(), 1);
        assert_eq!(s.next_step().unwrap(), 2);
        assert_eq!(s.next_step().unwrap(), 3);
    }

    #[test]
    fn errors_instead_of_cycling_when_exhausted() {
        let s = Script::of([1]).labelled("counter");
        assert_eq!(s.next_step().unwrap(), 1);
        let err = s.next_step().unwrap_err();
        assert_eq!(err.label, "counter");
        assert_eq!(err.taken, 1);
    }

    #[test]
    fn then_repeating_serves_the_steady_value_forever() {
        let s = Script::of([1, 2]).then_repeating(9);
        assert_eq!(s.next_step().unwrap(), 1);
        assert_eq!(s.next_step().unwrap(), 2);
        assert_eq!(s.next_step().unwrap(), 9);
        assert_eq!(s.next_step().unwrap(), 9);
    }

    #[test]
    fn then_repeating_with_supports_non_clone_values() {
        // The real motivator: `Result<_, LlmError>` is not Clone.
        let s: Script<Result<u8, String>> =
            Script::of([Ok(1)]).then_repeating_with(|| Err("boom".to_string()));
        assert_eq!(s.next_step().unwrap(), Ok(1));
        assert_eq!(s.next_step().unwrap(), Err("boom".to_string()));
        assert_eq!(s.next_step().unwrap(), Err("boom".to_string()));
    }

    #[test]
    fn taken_counts_consumed_steps() {
        let s = Script::of([1, 2, 3]);
        let _ = s.next_step();
        let _ = s.next_step();
        assert_eq!(s.taken(), 2);
    }
}
