//! Property-based tests for environment variable substitution.
//!
//! `$NAME` and `${NAME}` are two syntaxes for the same thing, so for any
//! value of a variable (including values that themselves look like variable
//! references), both must substitute it the same way, and the value must be
//! substituted as is, instead of being substituted again.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use indexmap::IndexMap;
use moon_env_var::{DotEnv, EnvSubstitutor};
use proptest::prelude::*;
use std::path::Path;

/// Values made of fragments that look like (parts of) variable references.
fn value_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "x", "y", " ", "$", "{", "}", "$B", "${B}", "${B:-z}", "${C}", "-",
        ]),
        0..6,
    )
    .prop_map(|fragments| fragments.concat())
}

fn substitute(vars: &IndexMap<String, Option<String>>, value: &str) -> String {
    EnvSubstitutor::default()
        .with_local_vars(vars)
        .substitute(value)
}

proptest! {
    #[test]
    fn both_syntaxes_substitute_the_same(a in value_strategy(), b in value_strategy()) {
        let vars = IndexMap::from_iter([
            ("A".to_owned(), Some(a.clone())),
            ("B".to_owned(), Some(b)),
        ]);

        prop_assert_eq!(
            substitute(&vars, "$A"),
            substitute(&vars, "${A}"),
            "A = {:?}",
            a
        );
        prop_assert_eq!(
            substitute(&vars, "x $A y"),
            substitute(&vars, "x ${A} y"),
            "A = {:?}",
            a
        );
    }

    #[test]
    fn values_are_substituted_as_is(a in value_strategy(), b in value_strategy()) {
        let vars = IndexMap::from_iter([
            ("A".to_owned(), Some(a.clone())),
            ("B".to_owned(), Some(b)),
        ]);

        prop_assert_eq!(substitute(&vars, "${A}"), a.clone());
        prop_assert_eq!(substitute(&vars, "$A"), a);
    }
}

/// Bugs found by the properties above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;

    // `$NAME` references are substituted first, and the result is then scanned
    // again for `${NAME}` references, so a value that contains `${...}` is
    // substituted twice when referenced as `$NAME`, but not as `${NAME}`.
    // In a `.env` file, a single quoted value isn't substituted, so `A` is
    // the literal `${B}`, but referencing it as `$A` substitutes `B` anyway
    #[test]
    fn doesnt_substitute_the_value_of_a_variable_again() {
        let vars = DotEnv::default()
            .load(
                "B=secret\nA='${B}'\nWITH_BRACKETS=\"${A}\"\nWITHOUT_BRACKETS=\"$A\"\n",
                Path::new(".env"),
            )
            .unwrap();

        assert_eq!(vars["A"], Some("${B}".into()));
        assert_eq!(vars["WITH_BRACKETS"], Some("${B}".into()));
        assert_eq!(
            vars["WITHOUT_BRACKETS"],
            Some("${B}".into()),
            "`$A` substituted the literal `${{B}}` within A's value"
        );
    }
}
