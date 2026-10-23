//! Property-based (differential) tests for input matching.
//!
//! Whether a file is an input of a task is decided twice: by the task hasher
//! (which files are hashed), and by affected tracking (which changed files
//! affect a task). For random inputs (files, globs, negated globs, workspace
//! relative globs, and folders) and outputs (files, folders, and globs), every
//! file in the workspace is changed one at a time, and the task's hash must
//! change exactly when the file affects the task.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

#![cfg(unix)]

use moon_action::{ActionNode, RunTaskNode};
use moon_action_context::ActionContext;
use moon_affected::AffectedTracker;
use moon_common::path::WorkspaceRelativePathBuf;
use moon_task_runner::TaskRunner;
use moon_test_utils::WorkspaceMocker;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::FxHashSet;
use starbase_sandbox::create_empty_sandbox;
use std::sync::Arc;

/// Every file in the workspace, relative to the root.
const FILES: [&str; 10] = [
    "project/a.txt",
    "project/b.txt",
    "project/src/x.ts",
    "project/src/y.md",
    "project/docs/d.txt",
    "project/dist/out.js",
    "project/build/z.txt",
    "project/gen/g.ts",
    "shared/x.txt",
    "shared/skip.txt",
];

const INPUTS: [&str; 7] = [
    "a.txt",
    "src/**/*",
    "!src/**/*.md",
    "/shared/*.txt",
    "!/shared/skip.txt",
    // A folder, which exists when the workspace is loaded
    "docs",
    "**/*.ts",
];

const OUTPUTS: [&str; 3] = [
    "dist/out.js",
    // A folder
    "build",
    "gen/**/*",
];

async fn run_case(inputs: Option<Vec<usize>>, outputs: Vec<usize>) -> Result<(), TestCaseError> {
    let sandbox = create_empty_sandbox();
    let quote = |values: &[&str]| {
        values
            .iter()
            .map(|value| format!("'{value}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut yaml = "tasks:\n  build:\n    command: 'noop'\n    toolchain: system\n".to_owned();

    if let Some(inputs) = &inputs {
        yaml.push_str(&format!(
            "    inputs: [{}]\n",
            quote(
                &inputs
                    .iter()
                    .map(|index| INPUTS[*index])
                    .collect::<Vec<_>>()
            )
        ));
    }

    if !outputs.is_empty() {
        yaml.push_str(&format!(
            "    outputs: [{}]\n",
            quote(
                &outputs
                    .iter()
                    .map(|index| OUTPUTS[*index])
                    .collect::<Vec<_>>()
            )
        ));
    }

    sandbox.create_file("project/moon.yml", &yaml);

    for file in FILES {
        sandbox.create_file(file, "0");
    }

    sandbox.enable_git();

    let mocker = WorkspaceMocker::new(sandbox.path())
        .load_default_configs()
        .with_global_envs()
        .with_default_projects();
    let app_context = Arc::new(mocker.mock_app_context());
    let workspace_graph = Arc::new(mocker.mock_workspace_graph().await);
    let project = workspace_graph.get_project("project").unwrap();
    let task = workspace_graph
        .get_task_from_project("project", "build")
        .unwrap();
    let node = ActionNode::RunTask(Box::new(RunTaskNode::new(task.target.clone())));

    let hash = || async {
        TaskRunner::new(&app_context, &project, &task, None)
            .unwrap()
            .hash(&ActionContext::default(), &node)
            .await
            .unwrap()
            .to_string()
    };

    let mut previous = hash().await;

    for (revision, file) in FILES.iter().enumerate() {
        sandbox.create_file(file, format!("{}", revision + 1));

        let next = hash().await;
        let hashed = next != previous;
        previous = next;

        let tracker = AffectedTracker::new(
            Arc::clone(&workspace_graph),
            FxHashSet::from_iter([WorkspaceRelativePathBuf::from(*file)]),
        );
        let affected = tracker.is_task_affected(&task).unwrap().is_some();

        prop_assert_eq!(
            hashed,
            affected,
            "changing {} {} the hash, but {} the task (inputs {:?}, outputs {:?})",
            file,
            if hashed { "changed" } else { "didn't change" },
            if affected {
                "affects"
            } else {
                "doesn't affect"
            },
            task.input_files
                .keys()
                .chain(task.input_globs.keys())
                .collect::<Vec<_>>(),
            task.output_files
                .keys()
                .chain(task.output_globs.keys())
                .collect::<Vec<_>>(),
        );
    }

    Ok(())
}

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(default)
}

#[test]
fn hashed_files_are_the_files_that_affect_the_task() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(48),
        failure_persistence: None,
        ..Config::default()
    });

    let strategy = (
        prop::option::weighted(
            0.85,
            prop::sample::subsequence((0..INPUTS.len()).collect::<Vec<_>>(), 1..4),
        ),
        prop::sample::subsequence((0..OUTPUTS.len()).collect::<Vec<_>>(), 0..3),
    );

    let result = runner.run(&strategy, |(inputs, outputs)| {
        runtime.block_on(run_case(inputs, outputs))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}

/// Bugs found by the property above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;

    fn run(inputs: Option<Vec<usize>>, outputs: Vec<usize>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        if let Err(error) = runtime.block_on(run_case(inputs, outputs)) {
            panic!("{error}");
        }
    }

    // The hasher never hashes output files (or files within output folders),
    // but `Task::create_globset`, which affected tracking uses, only excludes
    // output globs. So with the default inputs (`**/*`), changing an output
    // file (like a committed `dist` build) affects the task (and its
    // dependents), although its hash is the same, so it's a cache hit
    #[test]
    fn output_files_dont_affect_the_task() {
        run(None, vec![0]);
    }

    #[test]
    fn files_in_output_folders_dont_affect_the_task() {
        run(None, vec![1]);
    }
}
