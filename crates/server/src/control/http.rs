//! The HTTP surface of the control plane: every JSON route, folded out of the
//! operation table.

use serde_json::Value;

/// Fold path and query params into the input object.
///
/// Fill-missing-only, deliberately. Every resource here treats its path as the
/// id of record and rejects a body whose `name` disagrees with it — see
/// `AgentService::replace`. Overwriting the body from the path would make that
/// check unreachable, turning a rejected rename into a silent no-op rename.
pub(crate) fn merge_params(input: &mut Value, params: impl Iterator<Item = (String, String)>) {
    if !input.is_object() {
        *input = Value::Object(serde_json::Map::new());
    }
    let Some(object) = input.as_object_mut() else {
        return;
    };
    for (key, value) in params {
        object.entry(key).or_insert_with(|| Value::String(value));
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
    use serde_json::json;

    #[test]
    fn a_param_fills_a_missing_key() {
        let mut input = json!({});
        merge_params(
            &mut input,
            [("name".to_string(), "deploy".to_string())].into_iter(),
        );
        assert_eq!(input["name"], "deploy");
    }

    #[test]
    fn a_param_never_overwrites_the_body() {
        // The services' name-immutability checks depend on seeing the caller's
        // mismatched body, not a silently corrected one.
        let mut input = json!({"name": "renamed"});
        merge_params(
            &mut input,
            [("name".to_string(), "original".to_string())].into_iter(),
        );
        assert_eq!(input["name"], "renamed");
    }

    #[test]
    fn a_non_object_body_becomes_an_object() {
        let mut input = json!(null);
        merge_params(
            &mut input,
            [("name".to_string(), "deploy".to_string())].into_iter(),
        );
        assert_eq!(input, json!({"name": "deploy"}));
    }
}
