//! The `---`-fenced key/value header Claude Code puts on skills, agents and
//! commands.
//!
//! Only flat `key: value` scalars are read, which is all the format uses. It is
//! not YAML and does not try to be: a parser that accepted nested structures
//! would accept documents no consumer of this format can produce.

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

/// Every `key: value` pair in a header, in declaration order.
///
/// A line with no colon ends the read: the header is flat by construction, so a
/// line that is not a pair means the document is not what it claimed to be, and
/// guessing past it would invent fields.
#[must_use]
pub fn pairs(front: &str) -> Option<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        out.push((key.trim(), unquote(value.trim())));
    }
    Some(out)
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
        assert_eq!(pairs(front).unwrap(), vec![("name", "x")]);
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
            vec![("name", "x"), ("description", "a, b")]
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
    fn comma_lists_drop_blanks_and_whitespace() {
        assert_eq!(comma_list("Read, Grep ,, Bash"), ["Read", "Grep", "Bash"]);
        assert!(comma_list("  ").is_empty());
    }
}
