//! Property-based (model-based) tests for task caching.
//!
//! A task derives every kind of output (a required file, an optional file, and
//! files matched by a glob) from its input. A random sequence of operations is
//! then applied to the workspace: changing the input, deleting or adding
//! outputs, editing workspace configs, and running the task normally, after the
//! pipeline was aborted, or with a failing requirement. After every run, the
//! outcome is checked against a model of what the cache must (and must not)
//! do:
//!
//! - When a run passes (or is cached), the outputs on disk are exactly what
//!   the current input produces. Never stale outputs from another input.
//! - When a run doesn't run its command (skipped or failed before running), the
//!   outputs on disk are untouched.
//! - A run is never a cache hit for inputs that have never ran successfully,
//!   and is always a cache hit for inputs that have, unless the last run of
//!   the task failed (which avoids hydrating by design).
//! - The hash is a function of the task's inputs, which includes the
//!   toolchains config, but not the workspace config.
//!
//! Outputs can also be locked (made read-only), so that hydrating fails part
//! way through, after which any run may fail, but once a run passes again, its
//! outputs must be exactly what the input produces.
//!
//! The contents of outputs are not verified when reusing outputs for the same
//! hash, so modifying an output (instead of deleting it) is not an operation.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

#![cfg(unix)]

mod utils;

use moon_action::ActionStatus;
use moon_action_context::ActionContext;
use moon_config::{TaskCheck, TaskCheckRequirementConfig};
use moon_task_runner::{TaskRunResult, TaskRunner};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use utils::TaskRunnerContainer;

/// Inputs to choose from. Few of them, so that sequences revisit the same
/// hashes, which is where the cache is reused.
const INPUTS: [&str; 5] = ["a", "b", "oa", "ob", "fail"];

#[derive(Clone, Copy, Debug)]
enum Op {
    SetInput(usize),
    Run,
    /// Run after the pipeline was aborted, like when another task failed.
    RunStopped,
    /// Run with a requirement check that fails, which is not hashed.
    RunWithFailingRequirement,
    DeleteRequiredOutput,
    DeleteOptionalOutput,
    DeleteGlobOutputs,
    AddUnrelatedGlobOutput,
    EditWorkspaceConfig,
    EditToolchainsConfig,
    /// Make the glob outputs directory read-only, so that writing to it (when
    /// running or hydrating) fails.
    LockGlobOutputs,
    UnlockGlobOutputs,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..INPUTS.len()).prop_map(Op::SetInput),
        6 => Just(Op::Run),
        2 => Just(Op::RunStopped),
        1 => Just(Op::RunWithFailingRequirement),
        2 => Just(Op::DeleteRequiredOutput),
        2 => Just(Op::DeleteOptionalOutput),
        2 => Just(Op::DeleteGlobOutputs),
        1 => Just(Op::AddUnrelatedGlobOutput),
        1 => Just(Op::EditWorkspaceConfig),
        1 => Just(Op::EditToolchainsConfig),
        1 => Just(Op::LockGlobOutputs),
        1 => Just(Op::UnlockGlobOutputs),
    ]
}

/// Fewer operations that are more likely to interleave into hydrating one
/// input over the outputs of another, and failing part way through.
fn focused_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..2usize).prop_map(Op::SetInput),
        4 => Just(Op::Run),
        1 => Just(Op::LockGlobOutputs),
        1 => Just(Op::UnlockGlobOutputs),
    ]
}

/// Operations that start from any input.
fn ops_strategy(op: impl Strategy<Value = Op>) -> impl Strategy<Value = Vec<Op>> {
    (0..INPUTS.len(), prop::collection::vec(op, 1..24))
        .prop_map(|(input, ops)| std::iter::once(Op::SetInput(input)).chain(ops).collect())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Executed,
    Cached,
    Skipped,
    Failed,
}

fn get_outcome(result: &TaskRunResult) -> Outcome {
    if result.error.is_some() {
        return Outcome::Failed;
    }

    if result.operations.iter().any(|operation| {
        operation.meta.is_output_hydration()
            && matches!(
                operation.status,
                ActionStatus::Cached | ActionStatus::CachedFromRemote
            )
    }) {
        return Outcome::Cached;
    }

    match result
        .operations
        .get_last_execution()
        .map(|operation| operation.status)
    {
        Some(ActionStatus::Passed) => Outcome::Executed,
        Some(ActionStatus::Skipped) => Outcome::Skipped,
        _ => Outcome::Failed,
    }
}

/// The outputs of the task on disk.
#[derive(Debug, PartialEq)]
struct Outputs {
    required: Option<String>,
    optional: Option<String>,
    globs: BTreeMap<String, String>,
}

impl Outputs {
    fn expected(input: &str) -> Self {
        Self {
            required: Some(input.into()),
            optional: input.starts_with('o').then(|| input.into()),
            globs: BTreeMap::from_iter([(format!("{input}.txt"), input.into())]),
        }
    }

    /// Whether every expected output exists with the expected contents,
    /// ignoring any other outputs.
    fn contains(&self, expected: &Self) -> bool {
        self.required == expected.required
            && (expected.optional.is_none() || self.optional == expected.optional)
            && expected
                .globs
                .iter()
                .all(|(name, content)| self.globs.get(name) == Some(content))
    }

    fn read(out_dir: &Path) -> Self {
        let mut globs = BTreeMap::default();

        if let Ok(entries) = fs::read_dir(out_dir.join("glob")) {
            for entry in entries.flatten() {
                globs.insert(
                    entry.file_name().to_string_lossy().to_string(),
                    fs::read_to_string(entry.path()).unwrap_or_default(),
                );
            }
        }

        Self {
            required: fs::read_to_string(out_dir.join("required.txt")).ok(),
            optional: fs::read_to_string(out_dir.join("optional.txt")).ok(),
            globs,
        }
    }
}

/// What the hash of the task must be a function of.
type Key = (usize, usize);

struct Model {
    input: usize,
    toolchains_rev: usize,
    workspace_rev: usize,

    /// Keys that the task has ran successfully for (or was hydrated for).
    succeeded: FxHashSet<Key>,

    /// Whether the last command that ran failed, which avoids hydrating.
    last_failed: bool,

    /// Whether the glob outputs directory is read-only.
    locked: bool,

    /// Whether a run failed while locked, so it's unknown whether the command
    /// ran (and failed), or hydrating failed before it could run.
    uncertain: bool,

    hashes: FxHashMap<Key, String>,
}

impl Model {
    fn key(&self) -> Key {
        (self.input, self.toolchains_rev)
    }

    fn input(&self) -> &'static str {
        INPUTS[self.input]
    }

    fn check_hash(&mut self, result: &TaskRunResult) -> Result<(), TestCaseError> {
        let Some(hash) = &result.hash else {
            return Ok(());
        };
        let key = self.key();

        if let Some(previous) = self.hashes.get(&key) {
            prop_assert_eq!(
                previous,
                &hash.to_string(),
                "hash changed although the inputs (and toolchains config) did not"
            );
        }

        for (other_key, other_hash) in &self.hashes {
            prop_assert!(
                *other_key == key || *other_hash != hash.to_string(),
                "hash for {:?} is the same as for {:?}",
                key,
                other_key
            );
        }

        self.hashes.insert(key, hash.to_string());

        Ok(())
    }
}

async fn run_ops(ops: Vec<Op>, cas_enabled: bool, strict: bool) -> Result<(), TestCaseError> {
    let container = TaskRunnerContainer::new("props", "derive").await;
    container.sandbox.enable_git();

    let root = container.sandbox.path().to_path_buf();
    let out_dir = root.join("project/out");
    let node = container.create_action_node();

    fs::create_dir_all(root.join(".moon")).unwrap();

    // Requirements are not part of the hash, so this is the same task
    let task_with_failing_requirement = {
        let mut task = container.task.as_ref().to_owned();
        task.checks
            .push(TaskCheck::Requirement(TaskCheckRequirementConfig {
                script: "exit 1".into(),
            }));
        Arc::new(task)
    };

    let mut model = Model {
        input: 0,
        toolchains_rev: 0,
        workspace_rev: 0,
        succeeded: FxHashSet::default(),
        last_failed: false,
        locked: false,
        uncertain: false,
        hashes: FxHashMap::default(),
    };

    // Unlocked when dropped (including when a property fails), otherwise the
    // sandbox can't be removed
    let _unlock = Unlock(out_dir.join("glob"));

    for (step, op) in ops.into_iter().enumerate() {
        let before = Outputs::read(&out_dir);

        let result = match op {
            Op::SetInput(input) => {
                model.input = input;
                fs::write(root.join("project/input.txt"), INPUTS[input]).unwrap();
                continue;
            }
            Op::DeleteRequiredOutput => {
                let _ = fs::remove_file(out_dir.join("required.txt"));
                continue;
            }
            Op::DeleteOptionalOutput => {
                let _ = fs::remove_file(out_dir.join("optional.txt"));
                continue;
            }
            Op::DeleteGlobOutputs => {
                let _ = fs::remove_dir_all(out_dir.join("glob"));
                continue;
            }
            Op::AddUnrelatedGlobOutput => {
                let _ = fs::create_dir_all(out_dir.join("glob"));
                let _ = fs::write(out_dir.join("glob/unrelated.txt"), "unrelated");
                continue;
            }
            Op::LockGlobOutputs => {
                fs::create_dir_all(out_dir.join("glob")).unwrap();
                set_mode(&out_dir.join("glob"), 0o555);
                model.locked = true;
                continue;
            }
            Op::UnlockGlobOutputs => {
                set_mode(&out_dir.join("glob"), 0o755);
                model.locked = false;
                continue;
            }
            // Workspace settings don't affect how tasks run, so they must not
            // invalidate the cache, but toolchain settings do. These are
            // created after the workspace has been loaded, so that they don't
            // change how projects are located
            Op::EditWorkspaceConfig => {
                model.workspace_rev += 1;
                fs::write(
                    root.join(".moon/workspace.yml"),
                    format!("# revision {}\n", model.workspace_rev),
                )
                .unwrap();
                continue;
            }
            Op::EditToolchainsConfig => {
                model.toolchains_rev += 1;
                fs::write(
                    root.join(".moon/toolchains.yml"),
                    format!("# revision {}\n", model.toolchains_rev),
                )
                .unwrap();
                continue;
            }
            Op::Run => {
                let mut runner = container.create_runner();
                runner.state.local_cas_enabled = cas_enabled;
                runner.run(&ActionContext::default(), &node).await.unwrap()
            }
            Op::RunStopped => {
                let context = ActionContext::default();
                context.abort_token.cancel();

                let mut runner = container.create_runner();
                runner.state.local_cas_enabled = cas_enabled;
                runner.run(&context, &node).await.unwrap()
            }
            Op::RunWithFailingRequirement => {
                let mut runner = TaskRunner::new(
                    &container.app_context,
                    &container.project,
                    &task_with_failing_requirement,
                    None,
                )
                .unwrap();
                runner.state.local_cas_enabled = cas_enabled;
                runner.run(&ActionContext::default(), &node).await.unwrap()
            }
        };

        // Archiving is fire-and-forget, so wait for it like a following run would
        container.flush_storage().await;

        let outcome = get_outcome(&result);
        let key = model.key();
        let succeeded = model.succeeded.contains(&key);
        let must_be_cached = succeeded && !model.last_failed && !model.uncertain && !model.locked;
        let context = format!(
            "step {step}, {op:?} with input {:?}: {outcome:?} ({:?})",
            model.input(),
            result.error.as_ref().map(|error| error.to_string())
        );

        // Which outcomes are possible depends on how it ran. In every case,
        // hydration happens before the task stops or checks requirements
        let allowed: &[Outcome] = match op {
            // Writing outputs fails, while running or hydrating
            _ if model.locked => &[
                Outcome::Executed,
                Outcome::Cached,
                Outcome::Skipped,
                Outcome::Failed,
            ],
            Op::Run if model.input().starts_with("fail") => &[Outcome::Failed],
            Op::Run => &[Outcome::Executed, Outcome::Cached],
            Op::RunStopped => &[Outcome::Skipped, Outcome::Cached],
            _ => &[Outcome::Failed, Outcome::Cached],
        };

        prop_assert!(
            allowed.contains(&outcome),
            "unexpected outcome at {context}"
        );

        if outcome == Outcome::Cached {
            prop_assert!(
                succeeded,
                "cache hit for inputs that never ran successfully at {context}"
            );
        } else if must_be_cached {
            prop_assert!(
                false,
                "not cached, although the inputs ran successfully before at {context}"
            );
        }

        match outcome {
            Outcome::Executed | Outcome::Cached => {
                let actual = Outputs::read(&out_dir);
                let expected = Outputs::expected(model.input());

                // Known issue: hydrating from a local archive doesn't delete the
                // existing outputs first, so stale outputs that are not in the
                // archive are left behind (see `known_issues`). While locked,
                // existing outputs can't be deleted either
                if model.locked || (outcome == Outcome::Cached && !cas_enabled && !strict) {
                    prop_assert!(
                        actual.contains(&expected),
                        "outputs on disk are not from the current input at {}: {:?}",
                        context,
                        actual
                    );
                } else {
                    prop_assert_eq!(
                        actual,
                        expected,
                        "outputs on disk are not from the current input at {}",
                        context
                    );
                }

                // Either ran successfully, or the cache was hydrated, which
                // requires the previous run to have been successful
                model.succeeded.insert(key);
                model.last_failed = false;
                model.uncertain = false;
            }
            // Hydrating may have failed part way through
            Outcome::Skipped | Outcome::Failed if model.locked => {
                model.uncertain |= outcome == Outcome::Failed;
            }
            // The command ran but failed
            Outcome::Failed if op_runs_command(op) => {
                model.last_failed = true;
            }
            // The command never ran, so the outputs are as they were
            Outcome::Skipped | Outcome::Failed => {
                prop_assert_eq!(
                    Outputs::read(&out_dir),
                    before,
                    "outputs changed although the command never ran at {}",
                    context
                );
            }
        };

        model.check_hash(&result)?;
    }

    Ok(())
}

struct Unlock(std::path::PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        set_mode(&self.0, 0o755);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

fn op_runs_command(op: Op) -> bool {
    matches!(op, Op::Run)
}

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(default)
}

fn run_property(cas_enabled: bool, op: impl Strategy<Value = Op>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(64),
        failure_persistence: None,
        ..Config::default()
    });

    let result = runner.run(&ops_strategy(op), |ops| {
        runtime.block_on(run_ops(ops, cas_enabled, false))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}

#[test]
fn outputs_always_match_inputs_with_archives() {
    run_property(false, op_strategy());
}

#[test]
fn outputs_always_match_inputs_with_cas() {
    run_property(true, op_strategy());
}

// Hydrating from an archive never fails part way through, as it's a cache
// miss when unpacking fails, which deletes the outputs
#[test]
fn recovers_from_failed_hydrations_with_cas() {
    run_property(true, focused_op_strategy());
}

/// Counterexamples found by the properties above, that are excluded from them.
/// Run them with `--run-ignored only`.
mod known_issues {
    use super::*;

    fn run_strict(ops: Vec<Op>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        if let Err(error) = runtime.block_on(run_ops(ops, false, true)) {
            panic!("{error}");
        }
    }

    // `OutputHydrater::unpack_local_archive` only deletes the existing outputs
    // when unpacking fails, while hydrating from storage deletes them before
    // unpacking. Outputs on disk that are not in the archive are left behind
    #[test]
    #[ignore = "known issue: hydrating from a local archive leaves stale outputs"]
    fn hydrating_from_an_archive_removes_outputs_from_another_input() {
        run_strict(vec![
            Op::Run,
            Op::SetInput(1),
            Op::Run,
            Op::SetInput(0),
            Op::Run,
        ]);
    }

    #[test]
    #[ignore = "known issue: hydrating from a local archive leaves stale outputs"]
    fn hydrating_from_an_archive_removes_unrelated_outputs() {
        run_strict(vec![Op::Run, Op::AddUnrelatedGlobOutput, Op::Run]);
    }
}
