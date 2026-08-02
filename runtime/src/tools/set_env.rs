use crate::state::RuntimeState;
use horsie_models::runtime::{SetEnvInput, ToolError, ToolOutput, ToolResult};

/// Record an env set (`value` present) or unset (absent) for the agent's
/// future bash commands. The value is never echoed back — confirmations name
/// only the variable, so secrets don't land in the conversation history.
pub fn exec(state: &RuntimeState, agent: &str, input: SetEnvInput) -> ToolResult {
    if input.name.is_empty() || input.name.contains(['=', '\0']) {
        return ToolResult::Err(ToolError {
            reason: format!("invalid environment variable name: '{}'", input.name),
        });
    }
    if let Some(value) = &input.value
        && value.contains('\0')
    {
        return ToolResult::Err(ToolError {
            reason: "environment variable value contains NUL".to_string(),
        });
    }
    let verb = if input.value.is_some() {
        "set"
    } else {
        "unset"
    };
    let name = input.name.clone();
    state.apply_env(agent, input.name, input.value);
    ToolResult::Ok(ToolOutput {
        stdout: format!("{verb} {name}"),
        stderr: String::new(),
        exit_code: 0,
    })
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

    fn input(name: &str, value: Option<&str>) -> SetEnvInput {
        SetEnvInput {
            name: name.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn set_is_recorded_and_confirmed_without_the_value() {
        let state = RuntimeState::new();
        let r = exec(&state, "a", input("TOKEN", Some("s3cret")));
        match r {
            ToolResult::Ok(o) => {
                assert_eq!(o.stdout, "set TOKEN");
                assert!(!o.stdout.contains("s3cret"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        assert_eq!(
            state.env_overlay("a").sets,
            vec![("TOKEN".to_string(), "s3cret".to_string())]
        );
    }

    #[test]
    fn unset_is_recorded() {
        let state = RuntimeState::new();
        let r = exec(&state, "a", input("TOKEN", None));
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "unset TOKEN"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        assert_eq!(state.env_overlay("a").unsets, vec!["TOKEN".to_string()]);
    }

    #[test]
    fn invalid_names_are_rejected_and_change_nothing() {
        let state = RuntimeState::new();
        for bad in ["", "A=B", "A\0B"] {
            let r = exec(&state, "a", input(bad, Some("1")));
            assert!(matches!(r, ToolResult::Err(_)), "accepted '{bad:?}'");
        }
        assert!(state.env_overlay("a").sets.is_empty());
    }

    #[test]
    fn nul_in_value_is_rejected() {
        let state = RuntimeState::new();
        let r = exec(&state, "a", input("A", Some("x\0y")));
        assert!(matches!(r, ToolResult::Err(_)));
    }
}
