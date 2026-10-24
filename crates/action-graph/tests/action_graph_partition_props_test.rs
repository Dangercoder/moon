//! Property-based tests for partitioning targets across CI jobs (`--job` and
//! `--jobTotal`). For any amount of tasks and jobs, every task must be ran by
//! exactly one job.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_action_graph::{ActionGraphBuilder, ActionGraphBuilderOptions, RunRequirements};
use moon_task::TargetLocator;
use moon_test_utils::WorkspaceMocker;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::FxHashMap;
use starbase_sandbox::create_empty_sandbox;
use std::collections::BTreeSet;
use std::sync::Arc;

async fn run_partitions(tasks: usize, job_total: usize) -> Result<(), TestCaseError> {
    let sandbox = create_empty_sandbox();
    let mut yaml = String::from("tasks:\n");

    for task in 0..tasks {
        yaml.push_str(&format!("  t{task}:\n    command: 'echo'\n"));
    }

    sandbox.create_file("p0/moon.yml", yaml);

    let mocker = WorkspaceMocker::new(sandbox.path())
        .load_default_configs()
        .with_default_projects();
    let workspace_graph = Arc::new(mocker.mock_workspace_graph().await);
    let locators = (0..tasks)
        .map(|task| TargetLocator::parse(&format!("p0:t{task}")).unwrap())
        .collect::<Vec<_>>();

    // Which jobs ran each target
    let mut ran_by = FxHashMap::<String, Vec<usize>>::default();

    for job in 0..job_total {
        let mut builder = ActionGraphBuilder::new(
            Arc::new(mocker.mock_app_context()),
            Arc::clone(&workspace_graph),
            ActionGraphBuilderOptions::new(false),
        )
        .unwrap();

        let partition = builder
            .run_tasks(
                &locators,
                RunRequirements {
                    ci: false,
                    job: Some(job),
                    job_total: Some(job_total),
                    skip_affected: true,
                    ..RunRequirements::default()
                },
            )
            .await
            .unwrap();

        for target in partition.targets.values() {
            ran_by.entry(target.to_string()).or_default().push(job);
        }
    }

    let expected = (0..tasks)
        .map(|task| format!("p0:t{task}"))
        .collect::<BTreeSet<_>>();

    prop_assert_eq!(
        ran_by.keys().cloned().collect::<BTreeSet<_>>(),
        expected,
        "not every task was ran by a job"
    );

    for (target, jobs) in &ran_by {
        prop_assert!(
            jobs.len() == 1,
            "{} tasks across {} jobs: {} was ran by jobs {:?}",
            tasks,
            job_total,
            target,
            jobs
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
fn every_task_is_ran_by_exactly_one_job() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(64),
        failure_persistence: None,
        ..Config::default()
    });

    let result = runner.run(&(1..16usize, 1..8usize), |(tasks, job_total)| {
        runtime.block_on(run_partitions(tasks, job_total))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}

/// Bugs found by the property above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;

    // `ActionGraphBuilder::run_tasks` slices the tasks of each job with
    // `size = ceil(tasks / jobs)`, but when a job's slice starts past the end
    // of the tasks (`tasks.get(start)` is `None`), the tasks are not sliced at
    // all, so that job runs every task. With 4 jobs and 5 tasks, the slices
    // are [0, 2), [2, 4), [4, 5), and [6, ..), so the last job runs all 5.
    #[test]
    fn last_job_doesnt_run_every_task_when_its_slice_is_empty() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        if let Err(error) = runtime.block_on(run_partitions(5, 4)) {
            panic!("{error}");
        }
    }
}
