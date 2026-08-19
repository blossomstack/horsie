//! The one piece of the old workflow driver that survived the runner swap.
//!
//! `WorkflowOrchestrator` is gone: deciding what a run does next is
//! [`crate::sessions::runners::workflow::State`]'s, where the run's own slice
//! is. What is left is the pure transition lookup both shapes needed, which
//! never belonged to the orchestrator in the first place.

pub fn next_transition(
    transitions: &[crate::sessions::workflow::TransitionSpec],
    outcome: &str,
) -> Option<(String, Option<String>)> {
    for t in transitions {
        let Some(filter) = &t.when else {
            return Some((t.to.clone(), None));
        };
        if filter.matches(outcome) {
            return Some((t.to.clone(), Some(filter.render())));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::workflow::{OutcomeFilter, TransitionSpec};

    fn to(target: &str, values: &[&str]) -> TransitionSpec {
        TransitionSpec {
            to: target.into(),
            when: (!values.is_empty()).then(|| {
                OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                })
            }),
        }
    }

    /// The first edge whose filter matches wins, and the branch that was taken
    /// is reported so the run log can say which.
    #[test]
    fn a_matching_condition_picks_its_branch_and_names_it() {
        let edges = vec![to("fix", &["failed"]), to("ship", &["passed"])];
        let (target, via) = next_transition(&edges, "passed").unwrap();
        assert_eq!(target, "ship");
        assert!(via.is_some(), "a conditional edge reports which filter matched");
    }

    /// An edge with no filter is the catch-all, and a failing condition falls
    /// through to it rather than ending the run.
    ///
    /// Kept when `WorkflowOrchestrator` was deleted: its own test for this case
    /// went with it, and the runner's tests cover the matching branch but not
    /// the fall-through.
    #[test]
    fn a_failing_condition_falls_through_to_the_catch_all() {
        let edges = vec![to("fix", &["failed"]), to("ship", &[])];
        let (target, via) = next_transition(&edges, "passed").unwrap();
        assert_eq!(target, "ship");
        assert!(via.is_none(), "an unconditional edge names no filter");
    }

    /// No edge matches and none is a catch-all: the step routes nowhere, which
    /// is what finishes a run rather than an error.
    #[test]
    fn no_matching_edge_and_no_catch_all_routes_nowhere() {
        let edges = vec![to("fix", &["failed"])];
        assert!(next_transition(&edges, "passed").is_none());
    }
}
