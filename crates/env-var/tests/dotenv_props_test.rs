//! Property-based tests for parsing `.env` files.
//!
//! Random variables (with values that contain spaces, `#`, `=`, quotes, and
//! backslashes) are written to a `.env` file with valid quoting (single quotes
//! for literals, double quotes with escapes, or no quotes for simple values),
//! sometimes followed by a ` # comment`, and must be parsed back into exactly
//! the same values.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_env_var::DotEnv;
use proptest::prelude::*;
use std::path::Path;

#[derive(Clone, Debug)]
enum Quoting {
    Single,
    Double,
    None,
}

fn value_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => prop::char::range('a', 'z'),
            1 => prop::char::range('0', '9'),
            2 => Just(' '),
            1 => prop::sample::select(vec!['#', '=', '\'', '"', '\\', '-', '.', '/', ':']),
        ],
        0..12,
    )
    .prop_map(|chars| chars.into_iter().collect::<String>())
    // `null` and `nil` are special values (unset), see the bugs below
    .prop_filter("special values", |value| value != "null" && value != "nil")
}

fn comment_strategy() -> impl Strategy<Value = Option<String>> {
    prop::option::weighted(
        0.4,
        prop::collection::vec(
            prop_oneof![
                4 => prop::char::range('a', 'z'),
                1 => prop::sample::select(vec![' ', '#', '\'', '"', '=']),
            ],
            0..10,
        )
        .prop_map(|chars| chars.into_iter().collect::<String>()),
    )
}

/// How the value can be written, if at all.
fn quote(value: &str, quoting: &Quoting) -> Option<String> {
    match quoting {
        // Literal, can't contain a single quote
        Quoting::Single => (!value.contains('\'')).then(|| format!("'{value}'")),
        Quoting::Double => Some(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        )),
        // Only simple values
        Quoting::None => (!value.is_empty()
            && value
                .chars()
                .all(|char| char.is_ascii_alphanumeric() || "-./:".contains(char)))
        .then(|| value.to_owned()),
    }
}

proptest! {
    #[test]
    fn parses_written_values_back(
        vars in prop::collection::vec(
            (
                value_strategy(),
                prop_oneof![Just(Quoting::Single), Just(Quoting::Double), Just(Quoting::None)],
                comment_strategy(),
            ),
            1..6,
        )
    ) {
        let mut content = String::new();
        let mut expected = vec![];

        for (index, (value, quoting, comment)) in vars.iter().enumerate() {
            let Some(quoted) = quote(value, quoting) else {
                continue;
            };

            let key = format!("KEY_{index}");

            content.push_str(&format!("{key}={quoted}"));

            if let Some(comment) = comment {
                content.push_str(&format!(" # {comment}"));
            }

            content.push('\n');
            expected.push((key, Some(value.clone())));
        }

        let parsed = DotEnv::default()
            .load(&content, Path::new(".env"))
            .map_err(|error| TestCaseError::fail(format!("failed to parse:\n{content}\n{error}")))?;

        prop_assert_eq!(
            parsed.into_iter().collect::<Vec<_>>(),
            expected,
            "parsed:\n{}",
            content
        );
    }
}

/// Bugs found by the property above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;

    fn parse(content: &str) -> Vec<(String, Option<String>)> {
        DotEnv::default()
            .load(content, Path::new(".env"))
            .unwrap()
            .into_iter()
            .collect()
    }

    fn var(key: &str, value: &str) -> Vec<(String, Option<String>)> {
        vec![(key.to_owned(), Some(value.to_owned()))]
    }

    // The trailing comment is found with `rfind(" #")`, so only the last of
    // many ` #` is removed, and what remains is no longer a quoted value
    #[test]
    fn removes_comments_that_contain_a_hash() {
        assert_eq!(parse("KEY='value' # a # b\n"), var("KEY", "value"));
        assert_eq!(parse("KEY=\"value\" # a # b\n"), var("KEY", "value"));
    }

    // `get_quote_indices` looks for a pair of single quotes before double
    // quotes, so for a double quoted value that contains a pair of single
    // quotes, a ` #` after them (but within the double quotes) is mistaken for
    // a comment, and the value is cut off (with its opening quote)
    #[test]
    fn keeps_hashes_within_double_quotes_that_contain_single_quotes() {
        assert_eq!(parse("KEY=\"a 'b' #c\"\n"), var("KEY", "a 'b' #c"));
    }

    // Arguable: `null` and `nil` unset the variable, even when quoted, so a
    // variable can't be set to the literal string `null`
    #[test]
    fn quoted_null_is_a_literal_value() {
        assert_eq!(parse("KEY='null'\n"), var("KEY", "null"));
    }
}
