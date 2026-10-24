//! Property-based tests for targets.
//!
//! For any valid project and task identifiers (as accepted by `Id::new`,
//! which supports unicode, digits, `/`, `.`, `_`, `-`, and a leading `@`),
//! the target that is created from them must return them as is, and must be
//! parsed back (as a target, and as a target locator, like on the command
//! line) into the same target.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_common::Id;
use moon_target::{Target, TargetLocator};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

fn id_char() -> impl Strategy<Value = char> {
    prop_oneof![
        8 => prop::char::range('a', 'z'),
        2 => prop::char::range('A', 'Z'),
        2 => prop::char::range('0', '9'),
        2 => prop::sample::select(vec!['/', '.', '_', '-']),
        1 => prop::sample::select(vec!['é', 'ü', 'ß', 'ø', '日', '本', 'д']),
    ]
}

fn id_strategy(max_len: usize, allow_scope: bool) -> impl Strategy<Value = String> {
    (
        prop::bool::weighted(if allow_scope { 0.2 } else { 0.0 }),
        prop::collection::vec(id_char(), 1..max_len),
    )
        .prop_map(|(scope, chars)| {
            let id = chars.into_iter().collect::<String>();

            if scope { format!("@{id}") } else { id }
        })
        // Only identifiers that moon itself considers valid
        .prop_filter("valid identifier", |id| Id::new(id).is_ok())
}

fn check_round_trip(project_id: &str, task_id: &str) -> Result<(), TestCaseError> {
    let target = Target::new(project_id, task_id)
        .map_err(|error| TestCaseError::fail(format!("Target::new failed: {error}")))?;

    prop_assert_eq!(target.as_str(), format!("{project_id}:{task_id}"));
    prop_assert_eq!(
        target.get_project_id().ok(),
        Some(project_id),
        "project ID of {}",
        target.as_str()
    );
    prop_assert_eq!(
        target.get_task_id().ok(),
        Some(task_id),
        "task ID of {}",
        target.as_str()
    );

    let parsed = Target::parse(target.as_str()).map_err(|error| {
        TestCaseError::fail(format!(
            "Target::parse({}) failed: {error}",
            target.as_str()
        ))
    })?;

    prop_assert_eq!(&parsed, &target, "parsed {}", target.as_str());

    match TargetLocator::parse(target.as_str()) {
        Ok(TargetLocator::Qualified(located)) => {
            prop_assert_eq!(&located, &target, "located {}", target.as_str())
        }
        other => prop_assert!(
            false,
            "TargetLocator::parse({}) returned {:?}",
            target.as_str(),
            other
        ),
    };

    Ok(())
}

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(default)
}

fn run(strategy: impl Strategy<Value = (String, String)>) {
    let mut runner = TestRunner::new(Config {
        cases: cases(1024),
        failure_persistence: None,
        ..Config::default()
    });

    let result = runner.run(&strategy, |(project_id, task_id)| {
        check_round_trip(&project_id, &task_id)
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}

#[test]
fn targets_round_trip() {
    run((id_strategy(24, true), id_strategy(24, false)));
}

// Starting with a letter, as the first character is checked separately above
#[test]
fn targets_with_long_project_ids_round_trip() {
    run((
        id_strategy(400, false).prop_map(|id| format!("a{id}")),
        id_strategy(24, false),
    ));
}

/// Bugs found by the properties above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;

    fn round_trip(project_id: &str, task_id: &str) {
        if let Err(error) = check_round_trip(project_id, task_id) {
            panic!("{error}");
        }
    }

    // `TARGET_PATTERN` requires a project ID to start with `[A-Za-z@#_]`, but
    // `Id` accepts any unicode letter or digit first, like a project in a
    // `2fa-service` folder, so `moon run 2fa-service:build` fails to parse
    #[test]
    fn parses_project_ids_that_start_with_a_digit() {
        assert!(Id::new("2fa-service").is_ok());

        round_trip("2fa-service", "build");
    }

    #[test]
    fn parses_project_ids_that_start_with_a_non_ascii_letter() {
        assert!(Id::new("équipe").is_ok());

        round_trip("équipe", "build");
    }

    // `Target::index` (the position of the `:`) is a `u8` set with
    // `project_id.len() as u8`, so project IDs of 256 bytes or more wrap it,
    // and the project and task IDs are sliced at the wrong position
    #[test]
    fn supports_project_ids_of_256_bytes() {
        let project_id = "a".repeat(256);

        assert!(Id::new(&project_id).is_ok());

        round_trip(&project_id, "build");
    }

    // Slicing at the wrapped index can land inside a multi-byte character
    #[test]
    fn supports_long_unicode_project_ids() {
        let project_id = "日".repeat(86);

        assert!(Id::new(&project_id).is_ok());

        round_trip(&project_id, "build");
    }
}
