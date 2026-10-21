//! Error bodies from HTTP servers, reduced to the details a one-line diagnostic can carry.

use std::sync::LazyLock;

use regex::Regex;

use crate::util::text::truncate_to_width;

/// Most terminal columns [`condense`] keeps, so a verbose body cannot bury the advice after it.
pub(crate) const MAX_DETAILS_WIDTH: usize = 300;

static HTML_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("the title pattern is a valid regex")
});

/// What a non-2xx `body` says, [condensed](condense): the title of a markup page, such as a
/// gateway's or CDN's HTML error, or else the text itself. `None` when that is blank or a page has
/// no title.
pub(crate) fn body_details(body: &str) -> Option<String> {
    let text = if body.trim_start().starts_with('<') {
        HTML_TITLE.captures(body)?.get(1)?.as_str()
    } else {
        body
    };
    condense(text)
}

/// Server text on one line of at most [`MAX_DETAILS_WIDTH`] columns, with its punctuation as sent.
/// `None` when the text is blank.
pub(crate) fn condense(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| truncate_to_width(&text, MAX_DETAILS_WIDTH))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    mod body_details {
        use super::*;

        #[test]
        fn shows_the_title_of_a_markup_page() {
            let nginx = indoc! {"
                <html>
                <head><title>502 Bad Gateway</title></head>
                <body><center><h1>502 Bad Gateway</h1></center></body>
                </html>
            "};

            assert_eq!(body_details(nginx).as_deref(), Some("502 Bad Gateway"));
            assert_eq!(body_details("<html><body>busy</body></html>"), None);
        }

        #[test]
        fn shows_plain_text_on_one_capped_line() {
            let long = "x".repeat(MAX_DETAILS_WIDTH + 1);
            for (body, expected) in [
                (
                    "\n  upstream connect error\r\n\treset reason: overflow\n",
                    Some("upstream connect error reset reason: overflow".to_owned()),
                ),
                (
                    long.as_str(),
                    Some(format!("{}...", "x".repeat(MAX_DETAILS_WIDTH - 3))),
                ),
                (" \r\n", None),
            ] {
                assert_eq!(body_details(body), expected, "{body:?}");
            }
        }
    }
}
