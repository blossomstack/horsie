//! The `---`-fenced key/value header Claude Code puts on skills, agents and
//! commands.
//!
//! Frontmatter is YAML, but consumers only need a small set of scalar fields;
//! structured provider-specific fields are validated and ignored.

/// Split `---\n<frontmatter>\n---\n<body>` into its two halves.
///
/// `None` when the leading fence is absent or never closed — a document with no
/// header, rather than one with an empty header.
#[must_use]
pub fn split(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let mut idx = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..idx], &rest[idx + line.len()..]));
        }
        idx += line.len();
    }
    None
}

/// Every scalar key/value pair in a YAML frontmatter mapping, in declaration
/// order. Structured fields are valid YAML but are not exposed to consumers
/// that only need scalar metadata.
#[must_use]
pub fn pairs(front: &str) -> Option<Vec<(String, String)>> {
    let value: serde_yaml::Value = serde_yaml::from_str(front).ok()?;
    let mapping = value.as_mapping()?;
    let mut out = Vec::new();
    for (key, value) in mapping {
        let (Some(key), Some(value)) = (key.as_str(), value.as_str()) else {
            continue;
        };
        out.push((key.to_string(), value.to_string()));
    }
    Some(out)
}

/// Render a `---`-fenced header from scalar pairs, so that [`split`] and
/// [`pairs`] read back exactly what was written.
///
/// Through `serde_yaml` rather than `format!`, because frontmatter is real
/// YAML: a value holding `: `, a leading `-`, or a newline is not a scalar
/// unless it is quoted, and a hand-built header carrying one either fails to
/// parse or — worse — parses as something else. Anything writing a header this
/// crate will later read must come through here.
#[must_use]
pub fn render(pairs: &[(&str, &str)]) -> String {
    let mut mapping = serde_yaml::Mapping::new();
    for (key, value) in pairs {
        mapping.insert((*key).into(), (*value).into());
    }
    let body = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .unwrap_or_else(|_| String::new());
    format!("---\n{}---\n", body)
}

/// Strip one matched pair of surrounding quotes.
#[must_use]
pub fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Split a comma-separated frontmatter list, dropping empties.
#[must_use]
pub fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| unquote(s.trim()).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {

    /// The round trip that makes the renderer safe to point at agent-authored
    /// text: whatever goes in comes back out, including the values that would
    /// break a header built by hand.
    #[test]
    fn rendered_pairs_read_back_verbatim() {
        for value in [
            "plain",
            "has: a colon",
            "first\n---\nname: evil",
            "- leading dash",
            "#hash",
            "  padded  ",
            "quote\"inside",
            "",
        ] {
            let doc = format!("{}body", render(&[("name", "x"), ("description", value)]));
            let (front, body) = split(&doc).expect("a rendered header must split");
            let pairs = pairs(front).expect("a rendered header must parse");
            let got: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(
                got,
                vec![("name", "x"), ("description", value)],
                "value {value:?} did not survive"
            );
            assert_eq!(body, "body");
        }
    }
    use super::*;

    #[test]
    fn splits_a_fenced_header_from_its_body() {
        let (front, body) = split("---\nname: x\n---\nthe body\n").unwrap();
        assert_eq!(front, "name: x\n");
        assert_eq!(body, "the body\n");
    }

    #[test]
    fn tolerates_crlf() {
        let (front, body) = split("---\r\nname: x\r\n---\r\nbody").unwrap();
        assert_eq!(
            pairs(front).unwrap(),
            vec![("name".to_string(), "x".to_string())]
        );
        assert_eq!(body, "body");
    }

    #[test]
    fn an_absent_or_unclosed_fence_is_no_header() {
        assert!(split("no fence here").is_none());
        assert!(split("---\nname: x\nstill going").is_none());
    }

    #[test]
    fn reads_flat_pairs_in_order_and_unquotes() {
        let (front, _) = split("---\nname: x\ndescription: \"a, b\"\n---\n").unwrap();
        assert_eq!(
            pairs(front).unwrap(),
            vec![
                ("name".to_string(), "x".to_string()),
                ("description".to_string(), "a, b".to_string())
            ]
        );
    }

    /// A line that is not a pair means the document is not this format, and
    /// reading past it would invent fields out of prose.
    #[test]
    fn a_non_pair_line_rejects_the_whole_header() {
        let (front, _) = split("---\nname: x\njust prose\n---\n").unwrap();
        assert!(pairs(front).is_none());
    }

    #[test]
    fn accepts_multiline_yaml_fields() {
        let (front, _) = split(
            "---\nname: impeccable\ndescription: Design fluency\nallowed-tools:\n  - Bash(npx impeccable *)\n  - Bash(node scripts/*)\n---\n",
        )
        .unwrap();
        let pairs = pairs(front).unwrap();
        assert!(
            pairs
                .iter()
                .any(|(key, value)| *key == "name" && *value == "impeccable")
        );
        assert!(
            pairs
                .iter()
                .any(|(key, value)| *key == "description" && *value == "Design fluency")
        );
    }

    #[test]
    fn ignores_unknown_structured_fields() {
        let (front, _) = split(
            "---\nname: x\nmetadata:\n  nested: true\n  values:\n    - one\n    - two\ndescription: d\n---\n",
        )
        .unwrap();
        let pairs = pairs(front).unwrap();
        assert!(
            pairs
                .iter()
                .any(|(key, value)| *key == "name" && *value == "x")
        );
        assert!(
            pairs
                .iter()
                .any(|(key, value)| *key == "description" && *value == "d")
        );
    }

    #[test]
    fn comma_lists_drop_blanks_and_whitespace() {
        assert_eq!(comma_list("Read, Grep ,, Bash"), ["Read", "Grep", "Bash"]);
        assert!(comma_list("  ").is_empty());
    }
}
