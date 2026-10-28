//! TOML parse errors reported without the parsed source.

/// Where a parse error points: the position and the dotted key, each when the error records one.
#[derive(Debug)]
pub(crate) struct ErrorLocation {
    position: Option<String>,
    pub(crate) key: Option<String>,
}

impl ErrorLocation {
    /// ``line L, column C, in `key` ``, leaving out whatever the error does not record.
    pub(crate) fn describe(&self) -> Option<String> {
        let key = self.key.as_deref().map(|key| format!("in `{key}`"));
        let parts: Vec<&str> = self
            .position
            .as_deref()
            .into_iter()
            .chain(key.as_deref())
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Describes a parse error in `content` as ``line L, column C, in `key`: message``, leaving out the
/// position or key `err` does not record. `toml`'s own `Display` quotes the offending source line,
/// which would print a line of an unrelated file when the input is a mistyped path or a symlink.
pub fn describe_error(content: &str, err: &toml::de::Error) -> String {
    error_location(content, err).describe().map_or_else(
        || err.message().to_owned(),
        |location| format!("{location}: {}", err.message()),
    )
}

/// Where `err` points in `content`.
pub(crate) fn error_location(content: &str, err: &toml::de::Error) -> ErrorLocation {
    let position = err
        .span()
        .and_then(|span| content.get(..span.start))
        .map(|before| {
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            let line = before.matches('\n').count() + 1;
            let column = before[line_start..].chars().count() + 1;
            format!("line {line}, column {column}")
        });
    ErrorLocation {
        position,
        key: error_key(err),
    }
}

/// The parser's message with any user-supplied value removed, keeping ox's own vocabulary such as
/// field names and accepted values. `None` for a message that cannot be reduced safely.
pub(crate) fn redacted_message(err: &toml::de::Error) -> Option<String> {
    let message = err.message();
    if message.starts_with("unknown field") || message.starts_with("missing field") {
        return Some(message.to_owned());
    }
    // Serde's expected lists never contain this separator, so the last one ends the value.
    let (_, expected) = message.rsplit_once(", expected ")?;
    Some(format!("expected {expected}"))
}

/// The dotted key the error names, when it records one.
fn error_key(err: &toml::de::Error) -> Option<String> {
    // Without its input, `Display` follows the message with the key path, as in ``in `a.b` ``.
    let mut bare = err.clone();
    bare.set_input(None);
    let rendered = bare.to_string();
    let key = rendered.strip_prefix(err.message())?.trim();
    key.strip_prefix("in `")?
        .strip_suffix('`')
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use indoc::indoc;

    use super::*;

    fn parse_error(content: &str) -> toml::de::Error {
        toml::from_str::<toml::Table>(content).expect_err("the fixture must fail to parse")
    }

    mod describe_error {
        use super::*;

        #[test]
        fn locates_by_line_and_character_column() {
            // `é` is two bytes, so a byte-based column would read 44.
            let content = indoc! {"
                # header
                https://usér:secret-marker@git.example.com
            "};

            assert_eq!(
                describe_error(content, &parse_error(content)),
                "line 2, column 43: key with no value, expected `=`",
            );
        }

        #[test]
        fn counts_lines_across_crlf() {
            let content = "a = 1\r\nsecret-marker\r\n";

            assert_eq!(
                describe_error(content, &parse_error(content)),
                "line 2, column 14: key with no value, expected `=`",
            );
        }

        #[test]
        fn names_the_key_of_a_value_error() {
            let content = indoc! {"
                [tui]
                status_line = 42
            "};
            let err = toml::from_str::<HashMap<String, HashMap<String, Vec<String>>>>(content)
                .expect_err("a number is not a list");

            assert_eq!(
                describe_error(content, &err),
                "line 2, column 15, in `tui.status_line`: \
                 invalid type: integer `42`, expected a sequence",
            );
        }

        #[test]
        fn without_span_is_the_message_alone() {
            let err = <toml::de::Error as serde::de::Error>::custom("unexpected end");

            assert_eq!(describe_error("secret-marker", &err), "unexpected end");
        }
    }

    mod redacted_message {
        use serde::Deserialize;

        use super::*;

        #[expect(
            dead_code,
            reason = "the fixture fields exist only to shape serde's error messages"
        )]
        #[derive(Debug, Deserialize)]
        struct List {
            items: Vec<String>,
        }

        #[expect(
            dead_code,
            reason = "the fixture fields exist only to shape serde's error messages"
        )]
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            client: Client,
        }

        #[expect(
            dead_code,
            reason = "the fixture fields exist only to shape serde's error messages"
        )]
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Client {
            effort: Effort,
            model: String,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Effort {
            Low,
            Medium,
            High,
        }

        #[test]
        fn keeps_the_expected_shape_and_drops_the_value() {
            let err = toml::from_str::<List>("items = 42").expect_err("a sequence is required");
            assert_eq!(
                redacted_message(&err).as_deref(),
                Some("expected a sequence")
            );

            let unknown_variant = indoc! {"
                [client]
                effort = 'secret-marker'
                model = 'm'
            "};
            let err =
                toml::from_str::<Config>(unknown_variant).expect_err("an unknown variant fails");
            assert_eq!(
                redacted_message(&err).as_deref(),
                Some("expected one of `low`, `medium`, `high`"),
            );

            let wrong_type = indoc! {"
                [client]
                effort = 'low'
                model = ['secret-marker']
            "};
            let err = toml::from_str::<Config>(wrong_type).expect_err("a string is required");
            assert_eq!(redacted_message(&err).as_deref(), Some("expected a string"));
        }

        #[test]
        fn drops_a_value_that_holds_the_separator() {
            let content = indoc! {"
                [client]
                effort = 'secret-a, expected secret-b'
                model = 'm'
            "};
            let err = toml::from_str::<Config>(content).expect_err("an unknown variant fails");

            assert_eq!(
                redacted_message(&err).as_deref(),
                Some("expected one of `low`, `medium`, `high`"),
            );
        }

        #[test]
        fn keeps_unknown_field_names_and_their_expected_list() {
            // cspell:ignore modle

            let content = indoc! {"
                [client]
                modle = 'secret-marker'
                effort = 'low'
                model = 'm'
            "};
            let err = toml::from_str::<Config>(content).expect_err("an unknown field must fail");

            let message = redacted_message(&err).unwrap();

            assert!(
                message.starts_with("unknown field `modle`, expected"),
                "{message}"
            );
            assert!(!message.contains("secret-marker"), "{message}");
        }
    }
}
