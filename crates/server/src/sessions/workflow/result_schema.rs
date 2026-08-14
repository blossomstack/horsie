//! What a workflow step promises to return, compiled into a tool schema.
//!
//! A step declares the values its `outcome` may take and any extra fields it
//! carries. horsie turns that into the input schema of `submit_result`, adding
//! the two fields every step's result has whether it asked for them or not:
//!
//! - `outcome` — a string enum. The *only* thing transitions read, which is
//!   what keeps routing a definition-time decision rather than an expression
//!   evaluated against a shape nobody validated.
//! - `description` — a markdown summary of what the step did. Its
//!   documentation is horsie's rather than the author's: this is what the next
//!   step is handed, so what it must contain is a property of the system.
//!
//! Validation lives here too, and runs inside `submit_result` rather than in
//! the agent loop. A rejected payload is then an ordinary tool error the model
//! sees and re-issues, bounded by the loop's own retry budget — no privileged
//! machinery, and the check sits with the schema that defines it.

use horsie_models::workflow::{StepField, StepFieldType, StepOutcome};
use serde_json::{Map, Value, json};

/// The tool a step finishes with.
pub const SUBMIT_RESULT_TOOL: &str = "submit_result";

/// The field every transition reads.
pub const OUTCOME_FIELD: &str = "outcome";

/// The field the next step is handed.
pub const DESCRIPTION_FIELD: &str = "description";

/// What `outcome` means when a step does not say. Deliberately two values and
/// no more: a step that needs a third is saying something specific, and should
/// name it.
pub fn default_outcomes() -> Vec<StepOutcome> {
    vec![
        StepOutcome {
            value: "success".to_string(),
            description: "The step did what it was asked to do.".to_string(),
        },
        StepOutcome {
            value: "failure".to_string(),
            description: "The step could not do what it was asked to do.".to_string(),
        },
    ]
}

/// A step's declared outcomes, or the default pair.
pub fn outcomes_or_default(declared: Option<&Vec<StepOutcome>>) -> Vec<StepOutcome> {
    match declared {
        Some(list) if !list.is_empty() => list.clone(),
        _ => default_outcomes(),
    }
}

fn type_of(kind: &StepFieldType) -> Value {
    match kind {
        StepFieldType::String => json!({ "type": "string" }),
        StepFieldType::Number => json!({ "type": "number" }),
        StepFieldType::Boolean => json!({ "type": "boolean" }),
        StepFieldType::StringList => json!({ "type": "array", "items": { "type": "string" } }),
    }
}

/// The input schema of this step's `submit_result` tool.
pub fn result_schema(outcomes: &[StepOutcome], fields: &[StepField]) -> Value {
    let values: Vec<Value> = outcomes.iter().map(|o| json!(o.value)).collect();
    // Each value's own meaning, in the property's documentation: the enum alone
    // tells the model what it may say, not what any of it means.
    let meanings = outcomes
        .iter()
        .map(|o| format!("{}: {}", o.value, o.description))
        .collect::<Vec<_>>()
        .join("\n");

    let mut properties = Map::new();
    properties.insert(
        OUTCOME_FIELD.to_string(),
        json!({
            "type": "string",
            "enum": values,
            "description": format!(
                "How this step ended. This is what decides which step runs next.\n{meanings}"
            ),
        }),
    );
    properties.insert(
        DESCRIPTION_FIELD.to_string(),
        json!({
            "type": "string",
            "description":
                "A markdown summary of what you did and what you found. The next step is \
                 handed this and nothing else of your work, so make it self-contained.",
        }),
    );

    let mut required = vec![json!(OUTCOME_FIELD), json!(DESCRIPTION_FIELD)];
    for field in fields {
        let mut schema = type_of(&field.kind);
        if let Some(obj) = schema.as_object_mut() {
            obj.insert("description".to_string(), json!(field.description));
        }
        properties.insert(field.name.clone(), schema);
        if field.required.unwrap_or(false) {
            required.push(json!(field.name));
        }
    }

    json!({
        "type": "object",
        "required": required,
        "properties": Value::Object(properties),
    })
}

/// Check a submitted result against what the step declared.
///
/// A JSON Schema `enum` is advisory — providers do return values outside one —
/// so this is the only thing standing between an undeclared outcome and a
/// driver trying to route on it.
pub fn validate_result(
    value: &Value,
    outcomes: &[StepOutcome],
    fields: &[StepField],
) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err("the result must be an object".to_string());
    };

    let outcome = object
        .get(OUTCOME_FIELD)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("'{OUTCOME_FIELD}' is required and must be a string"))?;
    if !outcomes.iter().any(|o| o.value == outcome) {
        let allowed = outcomes
            .iter()
            .map(|o| o.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "'{outcome}' is not one of this step's outcomes: {allowed}"
        ));
    }

    let description = object
        .get(DESCRIPTION_FIELD)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("'{DESCRIPTION_FIELD}' is required and must be a string"))?;
    if description.trim().is_empty() {
        return Err(format!(
            "'{DESCRIPTION_FIELD}' must say what this step did — the next step is handed it"
        ));
    }

    for field in fields {
        let Some(present) = object.get(&field.name) else {
            if field.required.unwrap_or(false) {
                return Err(format!("'{}' is required", field.name));
            }
            continue;
        };
        // A present-but-null optional field is the model saying "nothing here",
        // which is the same as leaving it out.
        if present.is_null() && !field.required.unwrap_or(false) {
            continue;
        }
        let ok = match field.kind {
            StepFieldType::String => present.is_string(),
            StepFieldType::Number => present.is_number(),
            StepFieldType::Boolean => present.is_boolean(),
            StepFieldType::StringList => present
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
        };
        if !ok {
            return Err(format!(
                "'{}' must be {}",
                field.name,
                match field.kind {
                    StepFieldType::String => "a string",
                    StepFieldType::Number => "a number",
                    StepFieldType::Boolean => "true or false",
                    StepFieldType::StringList => "an array of strings",
                }
            ));
        }
    }
    Ok(())
}

/// A result rendered as the next step's incoming text.
///
/// Markdown rather than JSON: `description` exists precisely to be read by
/// whoever comes next, and `{"outcome":"success","description":"…"}` buries it
/// in punctuation. The outcome leads because it is the one thing the reader can
/// act on, the description follows as prose, and the declared fields come last
/// as a list.
pub fn render_result(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return value.to_string();
    };
    let mut out = String::new();
    if let Some(outcome) = object.get(OUTCOME_FIELD).and_then(Value::as_str) {
        out.push_str(&format!("**outcome:** {outcome}\n\n"));
    }
    if let Some(description) = object.get(DESCRIPTION_FIELD).and_then(Value::as_str) {
        out.push_str(description.trim_end());
        out.push('\n');
    }
    // Sorted by name, never in map order: whether a `serde_json::Map` iterates
    // in insertion or alphabetical order depends on the `preserve_order`
    // feature, which cargo unifies across a workspace build. What one step
    // hands the next must not change with how the binary was compiled.
    let mut extras: Vec<(&String, &Value)> = object
        .iter()
        .filter(|(k, _)| k.as_str() != OUTCOME_FIELD && k.as_str() != DESCRIPTION_FIELD)
        .collect();
    extras.sort_by(|a, b| a.0.cmp(b.0));
    if !extras.is_empty() {
        out.push('\n');
        for (name, v) in extras {
            let rendered = match v {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .map(|i| {
                        i.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| i.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => v.to_string(),
            };
            out.push_str(&format!("- **{name}:** {rendered}\n"));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn outcomes() -> Vec<StepOutcome> {
        vec![
            StepOutcome {
                value: "p0".into(),
                description: "drop everything".into(),
            },
            StepOutcome {
                value: "p2".into(),
                description: "file it".into(),
            },
        ]
    }

    fn field(name: &str, kind: StepFieldType, required: bool) -> StepField {
        StepField {
            name: name.into(),
            kind,
            description: format!("the {name}"),
            required: Some(required),
        }
    }

    #[test]
    fn outcome_is_a_required_enum_carrying_each_values_meaning() {
        let schema = result_schema(&outcomes(), &[]);
        let outcome = &schema["properties"][OUTCOME_FIELD];
        assert_eq!(outcome["enum"], json!(["p0", "p2"]));
        let doc = outcome["description"].as_str().unwrap();
        assert!(doc.contains("p0: drop everything"), "{doc}");
        assert!(doc.contains("p2: file it"), "{doc}");
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(OUTCOME_FIELD))
        );
    }

    /// The author declares *what* the values mean; what `description` is for is
    /// horsie's to say, because the next step's input depends on it.
    #[test]
    fn description_is_required_and_documented_by_horsie() {
        let schema = result_schema(&outcomes(), &[]);
        assert_eq!(schema["properties"][DESCRIPTION_FIELD]["type"], "string");
        assert!(
            schema["properties"][DESCRIPTION_FIELD]["description"]
                .as_str()
                .unwrap()
                .contains("next step")
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(DESCRIPTION_FIELD))
        );
    }

    #[test]
    fn a_declared_field_keeps_its_type_and_description() {
        let fields = vec![
            field("files", StepFieldType::StringList, true),
            field("count", StepFieldType::Number, false),
        ];
        let schema = result_schema(&outcomes(), &fields);
        assert_eq!(schema["properties"]["files"]["type"], "array");
        assert_eq!(schema["properties"]["files"]["items"]["type"], "string");
        assert_eq!(schema["properties"]["files"]["description"], "the files");
        assert_eq!(schema["properties"]["count"]["type"], "number");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("files")));
        assert!(
            !required.contains(&json!("count")),
            "an optional field is offered, not demanded"
        );
    }

    #[test]
    fn the_default_outcomes_are_success_and_failure() {
        let schema = result_schema(&outcomes_or_default(None), &[]);
        assert_eq!(
            schema["properties"][OUTCOME_FIELD]["enum"],
            json!(["success", "failure"])
        );
    }

    #[test]
    fn an_empty_declared_list_falls_back_to_the_default() {
        assert_eq!(outcomes_or_default(Some(&Vec::new())).len(), 2);
    }

    /// A schema `enum` is advisory on the wire. Without this check an
    /// undeclared outcome reaches the driver, which then matches no transition
    /// and quietly ends the run as though the step had finished the graph.
    #[test]
    fn validate_rejects_an_outcome_outside_the_enum() {
        let err = validate_result(
            &json!({"outcome": "p1", "description": "did it"}),
            &outcomes(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("'p1' is not one of"), "{err}");
        assert!(err.contains("p0, p2"), "the model is told what it may say");
    }

    #[test]
    fn validate_rejects_a_missing_or_empty_description() {
        let err = validate_result(&json!({"outcome": "p0"}), &outcomes(), &[]).unwrap_err();
        assert!(err.contains("description"), "{err}");
        let err = validate_result(
            &json!({"outcome": "p0", "description": "   "}),
            &outcomes(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("what this step did"), "{err}");
    }

    #[test]
    fn validate_rejects_a_missing_required_field() {
        let fields = vec![field("files", StepFieldType::StringList, true)];
        let err = validate_result(
            &json!({"outcome": "p0", "description": "did it"}),
            &outcomes(),
            &fields,
        )
        .unwrap_err();
        assert!(err.contains("'files' is required"), "{err}");
    }

    #[test]
    fn validate_rejects_a_field_of_the_wrong_type() {
        let fields = vec![field("count", StepFieldType::Number, false)];
        let err = validate_result(
            &json!({"outcome": "p0", "description": "did it", "count": "seven"}),
            &outcomes(),
            &fields,
        )
        .unwrap_err();
        assert!(err.contains("'count' must be a number"), "{err}");
    }

    #[test]
    fn validate_accepts_an_absent_or_null_optional_field() {
        let fields = vec![field("count", StepFieldType::Number, false)];
        assert!(
            validate_result(
                &json!({"outcome": "p0", "description": "did it"}),
                &outcomes(),
                &fields
            )
            .is_ok()
        );
        assert!(
            validate_result(
                &json!({"outcome": "p0", "description": "did it", "count": null}),
                &outcomes(),
                &fields
            )
            .is_ok()
        );
    }

    #[test]
    fn render_leads_with_the_outcome_then_the_description_then_the_fields() {
        let rendered = render_result(&json!({
            "outcome": "p0",
            "description": "Found the regression in `journal.rs`.",
            "files": ["a.rs", "b.rs"],
            "count": 2,
        }));
        assert_eq!(
            rendered,
            "**outcome:** p0\n\nFound the regression in `journal.rs`.\n\n\
             - **count:** 2\n- **files:** a.rs, b.rs"
        );
    }

    /// The run's own input, and anything else not shaped like a result, is
    /// passed through rather than mangled.
    #[test]
    fn render_passes_a_non_object_through() {
        assert_eq!(
            render_result(&json!("the build is red")),
            "\"the build is red\""
        );
    }
}
