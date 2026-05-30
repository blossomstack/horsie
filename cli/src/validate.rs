use crate::config::OctoberConfig;
use models::workflow::WorkflowDefinition;
use std::collections::HashSet;

/// Structural + semantic checks. Returns ALL errors (empty = valid):
/// - `start` names an existing agent;
/// - every transition `to` names an existing agent;
/// - every transition `condition` parses (evaluated once with a placeholder
///   `output` binding — a bad condition is silently swallowed at runtime, so we
///   surface parse failures here);
/// - every `agent.model` is in config `models`;
/// - every referenced `model.provider` is in config `providers`.
pub fn validate(def: &WorkflowDefinition, cfg: &OctoberConfig) -> Vec<String> {
    let mut errs = Vec::new();
    let names: HashSet<&str> = def.agents.iter().map(|a| a.name.as_str()).collect();

    if !names.contains(def.start.as_str()) {
        errs.push(format!("start agent '{}' is not defined", def.start));
    }

    for a in &def.agents {
        if !cfg.models.contains_key(&a.model) {
            errs.push(format!(
                "agent '{}' uses model '{}' which is not in config.models",
                a.name, a.model
            ));
        }
        if let Some(transitions) = &a.transitions {
            for t in transitions {
                if !names.contains(t.to.as_str()) {
                    errs.push(format!(
                        "agent '{}' has a transition to undefined agent '{}'",
                        a.name, t.to
                    ));
                }
                if let Some(cond) = &t.condition
                    && let Err(e) = eval::Expr::new(cond)
                        .value("output", serde_json::json!({}))
                        .exec()
                {
                    errs.push(format!(
                        "agent '{}' transition condition `{cond}` failed to evaluate: {e}",
                        a.name
                    ));
                }
            }
        }
    }

    for (model_key, mc) in &cfg.models {
        if !cfg.providers.contains_key(&mc.provider) {
            errs.push(format!(
                "model '{model_key}' references unknown provider '{}'",
                mc.provider
            ));
        }
    }

    errs
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use models::workflow::{WorkflowAgentDef, WorkflowTransition};
    use serde_json::json;

    fn cfg() -> OctoberConfig {
        serde_json::from_str(
            r#"{
                "providers": { "local": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "local", "model_id": "id" } }
            }"#,
        )
        .unwrap()
    }

    fn agent(name: &str, model: &str) -> WorkflowAgentDef {
        WorkflowAgentDef {
            name: name.into(),
            system_prompt: None,
            model: model.into(),
            output_schema: Some(json!({"type": "object"})),
            allow_ask_user: false,
            transitions: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: Some(vec![]),
        }
    }

    #[test]
    fn valid_workflow_has_no_errors() {
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![agent("a", "m")],
        };
        assert!(validate(&def, &cfg()).is_empty());
    }

    #[test]
    fn unknown_start_is_reported() {
        let def = WorkflowDefinition {
            start: "missing".into(),
            agents: vec![agent("a", "m")],
        };
        let errs = validate(&def, &cfg());
        assert!(errs.iter().any(|e| e.contains("start agent 'missing'")));
    }

    #[test]
    fn unknown_transition_target_is_reported() {
        let mut a = agent("a", "m");
        a.transitions = Some(vec![WorkflowTransition {
            to: "ghost".into(),
            condition: None,
        }]);
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![a],
        };
        let errs = validate(&def, &cfg());
        assert!(errs.iter().any(|e| e.contains("undefined agent 'ghost'")));
    }

    #[test]
    fn unknown_model_is_reported() {
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![agent("a", "nope")],
        };
        let errs = validate(&def, &cfg());
        assert!(errs.iter().any(|e| e.contains("model 'nope'")));
    }

    #[test]
    fn unknown_provider_is_reported() {
        let bad: OctoberConfig = serde_json::from_str(
            r#"{
                "providers": {},
                "models": { "m": { "provider": "ghost", "model_id": "id" } }
            }"#,
        )
        .unwrap();
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![agent("a", "m")],
        };
        let errs = validate(&def, &bad);
        assert!(errs.iter().any(|e| e.contains("unknown provider 'ghost'")));
    }

    #[test]
    fn field_access_condition_against_placeholder_does_not_false_positive() {
        // `output.score > 80` against `{}` must not be reported as a parse failure.
        let mut a = agent("a", "m");
        let b = agent("b", "m");
        a.transitions = Some(vec![WorkflowTransition {
            to: "b".into(),
            condition: Some("output.score > 80".into()),
        }]);
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![a, b],
        };
        assert!(
            validate(&def, &cfg()).is_empty(),
            "valid field-access condition wrongly flagged"
        );
    }

    #[test]
    fn syntactically_broken_condition_is_reported() {
        let mut a = agent("a", "m");
        let b = agent("b", "m");
        a.transitions = Some(vec![WorkflowTransition {
            to: "b".into(),
            condition: Some("output.score >>> ".into()),
        }]);
        let def = WorkflowDefinition {
            start: "a".into(),
            agents: vec![a, b],
        };
        assert!(
            validate(&def, &cfg())
                .iter()
                .any(|e| e.contains("failed to evaluate")),
            "broken condition should be reported"
        );
    }
}
