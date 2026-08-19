//! Deciding which transition a concluded step's outcome takes.
//!
//! The step decision itself lives on the workflow runner
//! (`session_actor::runner::workflow`); what stays here is the one pure rule
//! both it and the definition validator share.

/// The first transition whose filter admits `outcome`.
///
/// `None` means none matched and the step is terminal. There is no error case:
/// a filter can only name outcomes the producing step declares — checked when
/// the workflow is saved — so there is nothing left to fail on at run time.
/// That is the whole of what replaced an expression evaluator that could panic
/// on a typo, and could turn one into a run that quietly ended as if it had
/// succeeded.
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
    use horsie_models::workflow::OutcomeFilter;

    /// An outcome the step never declared cannot reach here — `submit_result`
    /// rejects it — but if one did, it must match nothing rather than match
    /// everything.
    #[test]
    fn an_unrecognised_outcome_matches_no_filter() {
        let filter = OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
            values: vec!["p0".into()],
        });
        assert!(!filter.matches("p9"));
    }

    #[test]
    fn a_filter_renders_as_the_edge_label_a_reader_sees() {
        let f = OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
            values: vec!["p0".into(), "p1".into()],
        });
        assert_eq!(f.render(), "outcome in [p0, p1]");
        let f = OutcomeFilter::NotIn(horsie_models::workflow::OutcomeNotIn {
            values: vec!["p2".into()],
        });
        assert_eq!(f.render(), "outcome not in [p2]");
    }
}
