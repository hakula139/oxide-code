//! TOML parse errors reported without the parsed source.

/// Describes a parse error in `content` as ``line L, column C, in `key`: message``, leaving out
/// the position or key `err` does not record. `toml`'s own `Display` quotes the offending source
/// line, which would print a line of an unrelated file when the input is a mistyped path or a
/// symlink.
pub fn describe_error(content: &str, err: &toml::de::Error) -> String {
    let message = err.message();
    match locate_error(content, err) {
        Some(location) => format!("{location}: {message}"),
        None => message.to_owned(),
    }
}

/// Where a parse error in `content` points, as ``line L, column C, in `key` ``, leaving out the
/// position or key `err` does not record. `None` when it records neither.
pub(crate) fn locate_error(content: &str, err: &toml::de::Error) -> Option<String> {
    let position = err
        .span()
        .and_then(|span| content.get(..span.start))
        .map(|before| {
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            let line = before.matches('\n').count() + 1;
            let column = before[line_start..].chars().count() + 1;
            format!("line {line}, column {column}")
        });
    // Without its input, `Display` follows the message with the key path, as in ``in `a.b` ``.
    let mut bare = err.clone();
    bare.set_input(None);
    let rendered = bare.to_string();
    let key = rendered
        .strip_prefix(err.message())
        .map(str::trim)
        .filter(|key| !key.is_empty());

    let location: Vec<&str> = position.as_deref().into_iter().chain(key).collect();
    (!location.is_empty()).then(|| location.join(", "))
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
}
