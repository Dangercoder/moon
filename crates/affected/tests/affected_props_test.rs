//! Property-based tests for affected tracking.
//!
//! Random workspaces (projects that depend on earlier projects, with tasks
//! that depend on earlier tasks, and inputs of every kind) are tracked against
//! random changed files, for every combination of upstream and downstream
//! scopes, and:
//!
//! - The sync and async trackers (the `asyncAffectedTracking` experiment)
//!   mark exactly the same projects and tasks, for the same reasons.
//! - Tracking is monotonic: more changed files never affect fewer projects or
//!   tasks.
//! - The affected tasks are exactly those with an input that matches a changed
//!   file, and their dependencies (upstream) and dependents (downstream),
//!   within scope.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_affected::{Affected, AffectedTracker, DownstreamScope, UpstreamScope};
use moon_common::path::WorkspaceRelativePathBuf;
use moon_test_utils::{WorkspaceGraph, WorkspaceMocker};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::FxHashSet;
use starbase_sandbox::create_empty_sandbox;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Files that may change, where `{}` is the project's folder.
const PROJECT_FILES: [&str; 3] = ["{}/a.txt", "{}/src/x.ts", "{}/b.md"];
const OTHER_FILES: [&str; 2] = ["shared/x.txt", "root.txt"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum InputSpec {
    /// `a.txt`, a file in the project.
    File,
    /// `src/**/*`, a glob in the project.
    Glob,
    /// `/shared/*.txt`, a glob relative to the workspace root.
    WorkspaceGlob,
}

#[derive(Clone, Debug)]
struct TaskSpec {
    /// `None` uses the default inputs (every file in the project).
    inputs: Option<Vec<InputSpec>>,
    /// Indexes into the tasks that come before this one.
    deps: Vec<usize>,
}

#[derive(Clone, Debug)]
struct WorkspaceSpec {
    /// The projects each project depends on.
    depends_on: Vec<Vec<usize>>,
    /// `(project, task)` for every task, in order.
    tasks: Vec<(usize, TaskSpec)>,
}

impl WorkspaceSpec {
    fn target(&self, index: usize) -> String {
        let project = self.tasks[index].0;
        let task = self.tasks[..index]
            .iter()
            .filter(|(other, _)| *other == project)
            .count();

        format!("p{project}:t{task}")
    }

    fn files(&self) -> Vec<String> {
        (0..self.depends_on.len())
            .flat_map(|project| {
                PROJECT_FILES
                    .iter()
                    .map(move |file| file.replace("{}", &format!("p{project}")))
            })
            .chain(OTHER_FILES.iter().map(|file| file.to_string()))
            .collect()
    }

    fn is_input(&self, index: usize, file: &str) -> bool {
        let (project, task) = &self.tasks[index];
        let dir = format!("p{project}/");

        match &task.inputs {
            None => file.starts_with(&dir),
            Some(inputs) => inputs.iter().any(|input| match input {
                InputSpec::File => file == format!("{dir}a.txt"),
                InputSpec::Glob => file.starts_with(&format!("{dir}src/")),
                InputSpec::WorkspaceGlob => file.starts_with("shared/") && file.ends_with(".txt"),
            }),
        }
    }

    fn dependents(&self, index: usize) -> Vec<usize> {
        (0..self.tasks.len())
            .filter(|other| self.tasks[*other].1.deps.contains(&index))
            .collect()
    }

    /// The tasks reachable from the task (excluding itself) within scope.
    fn reach(&self, index: usize, deep: bool, next: impl Fn(usize) -> Vec<usize>) -> Vec<usize> {
        let mut found = vec![];
        let mut stack = next(index);

        while let Some(other) = stack.pop() {
            if !found.contains(&other) {
                found.push(other);

                if deep {
                    stack.extend(next(other));
                }
            }
        }

        found
    }

    fn expected_tasks(
        &self,
        changed: &[String],
        upstream: UpstreamScope,
        downstream: DownstreamScope,
    ) -> BTreeSet<String> {
        let mut affected = BTreeSet::default();

        for index in 0..self.tasks.len() {
            if !changed.iter().any(|file| self.is_input(index, file)) {
                continue;
            }

            affected.insert(self.target(index));

            if upstream != UpstreamScope::None {
                for dep in self.reach(index, upstream == UpstreamScope::Deep, |task| {
                    self.tasks[task].1.deps.clone()
                }) {
                    affected.insert(self.target(dep));
                }
            }

            if downstream != DownstreamScope::None {
                for dep in self.reach(index, downstream == DownstreamScope::Deep, |task| {
                    self.dependents(task)
                }) {
                    affected.insert(self.target(dep));
                }
            }
        }

        affected
    }
}

fn input_strategy() -> impl Strategy<Value = InputSpec> {
    prop_oneof![
        Just(InputSpec::File),
        Just(InputSpec::Glob),
        Just(InputSpec::WorkspaceGlob),
    ]
}

fn workspace_strategy() -> impl Strategy<Value = WorkspaceSpec> {
    (1..4usize).prop_flat_map(|projects| {
        (
            prop::collection::vec(prop::collection::vec(any::<usize>(), 0..2), projects),
            prop::collection::vec(
                (
                    0..projects,
                    prop::option::weighted(0.8, prop::collection::vec(input_strategy(), 0..3)),
                    prop::collection::vec(any::<usize>(), 0..3),
                ),
                1..7,
            ),
        )
            .prop_map(move |(depends_on, tasks)| {
                let depends_on = depends_on
                    .into_iter()
                    .enumerate()
                    .map(|(project, deps)| {
                        let mut deps = deps
                            .into_iter()
                            .filter(|_| project > 0)
                            .map(|dep| dep % project)
                            .collect::<Vec<_>>();
                        deps.sort();
                        deps.dedup();
                        deps
                    })
                    .collect();

                // Tasks are ordered by project, so that their IDs are stable
                let mut tasks = tasks;
                tasks.sort_by_key(|(project, _, _)| *project);

                let tasks = tasks
                    .into_iter()
                    .enumerate()
                    .map(|(index, (project, inputs, deps))| {
                        let mut deps = deps
                            .into_iter()
                            .filter(|_| index > 0)
                            .map(|dep| dep % index)
                            .collect::<Vec<_>>();
                        deps.sort();
                        deps.dedup();

                        (project, TaskSpec { inputs, deps })
                    })
                    .collect();

                WorkspaceSpec { depends_on, tasks }
            })
    })
}

fn write_workspace(spec: &WorkspaceSpec, root: &std::path::Path) {
    for (project, depends_on) in spec.depends_on.iter().enumerate() {
        let mut yaml = String::new();

        if !depends_on.is_empty() {
            yaml.push_str("dependsOn:\n");

            for dep in depends_on {
                yaml.push_str(&format!("  - 'p{dep}'\n"));
            }
        }

        yaml.push_str("tasks:\n");

        for (index, (task_project, task)) in spec.tasks.iter().enumerate() {
            if *task_project != project {
                continue;
            }

            let target = spec.target(index);
            let id = target.split(':').nth(1).unwrap();

            yaml.push_str(&format!("  {id}:\n    command: 'echo'\n"));

            if let Some(inputs) = &task.inputs {
                yaml.push_str("    inputs:\n");

                if inputs.is_empty() {
                    yaml.replace_range(yaml.len() - 1.., " []\n");
                }

                for input in inputs {
                    yaml.push_str(match input {
                        InputSpec::File => "      - 'a.txt'\n",
                        InputSpec::Glob => "      - 'src/**/*'\n",
                        InputSpec::WorkspaceGlob => "      - '/shared/*.txt'\n",
                    });
                }
            }

            if !task.deps.is_empty() {
                yaml.push_str("    deps:\n");

                for dep in &task.deps {
                    yaml.push_str(&format!("      - '{}'\n", spec.target(*dep)));
                }
            }
        }

        let path = root.join(format!("p{project}/moon.yml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, yaml).unwrap();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Sync,
    Async,
    SyncTasksOnly,
}

async fn track(
    workspace_graph: &Arc<WorkspaceGraph>,
    changed: &[String],
    upstream: UpstreamScope,
    downstream: DownstreamScope,
    mode: Mode,
) -> Affected {
    let changed_files = changed
        .iter()
        .map(WorkspaceRelativePathBuf::from)
        .collect::<FxHashSet<_>>();

    let mut tracker = AffectedTracker::new(Arc::clone(workspace_graph), changed_files);
    tracker.set_scopes(upstream, downstream);

    match mode {
        Mode::Sync => {
            tracker.track_projects().unwrap();
            tracker.track_tasks().unwrap();
        }
        Mode::Async => {
            tracker.track_projects_async().await.unwrap();
            tracker.track_tasks_async().await.unwrap();
        }
        Mode::SyncTasksOnly => {
            tracker.track_tasks().unwrap();
        }
    };

    tracker.build()
}

fn task_keys(affected: &Affected) -> BTreeSet<String> {
    affected
        .tasks
        .keys()
        .map(|target| target.to_string())
        .collect()
}

fn project_keys(affected: &Affected) -> BTreeSet<String> {
    affected.projects.keys().map(|id| id.to_string()).collect()
}

const UPSTREAM: [UpstreamScope; 3] = [
    UpstreamScope::None,
    UpstreamScope::Direct,
    UpstreamScope::Deep,
];
const DOWNSTREAM: [DownstreamScope; 3] = [
    DownstreamScope::None,
    DownstreamScope::Direct,
    DownstreamScope::Deep,
];

async fn run_scenario(spec: WorkspaceSpec, changed: Vec<bool>) -> Result<(), TestCaseError> {
    let sandbox = create_empty_sandbox();
    write_workspace(&spec, sandbox.path());

    let workspace_graph = Arc::new(
        WorkspaceMocker::new(sandbox.path())
            .load_default_configs()
            .with_default_projects()
            .mock_workspace_graph()
            .await,
    );

    let files = spec.files();
    let changed = files
        .iter()
        .zip(changed.iter().cycle())
        .filter(|(_, changed)| **changed)
        .map(|(file, _)| file.clone())
        .collect::<Vec<_>>();

    for upstream in UPSTREAM {
        for downstream in DOWNSTREAM {
            let scopes =
                format!("upstream {upstream:?}, downstream {downstream:?}, changed {changed:?}");

            let sync = track(&workspace_graph, &changed, upstream, downstream, Mode::Sync).await;
            let async_ = track(
                &workspace_graph,
                &changed,
                upstream,
                downstream,
                Mode::Async,
            )
            .await;

            prop_assert_eq!(
                &sync.tasks,
                &async_.tasks,
                "sync and async trackers marked different tasks ({})",
                scopes
            );
            prop_assert_eq!(
                &sync.projects,
                &async_.projects,
                "sync and async trackers marked different projects ({})",
                scopes
            );

            let tasks_only = track(
                &workspace_graph,
                &changed,
                upstream,
                downstream,
                Mode::SyncTasksOnly,
            )
            .await;

            prop_assert_eq!(
                task_keys(&tasks_only),
                spec.expected_tasks(&changed, upstream, downstream),
                "affected tasks ({})",
                scopes
            );

            // Every subset of the changed files affects a subset
            for skip in 0..changed.len() {
                let mut fewer = changed.clone();
                fewer.remove(skip);

                for mode in [Mode::Sync, Mode::Async] {
                    let less = track(&workspace_graph, &fewer, upstream, downstream, mode).await;
                    let more = if mode == Mode::Sync { &sync } else { &async_ };

                    prop_assert!(
                        task_keys(&less).is_subset(&task_keys(more)),
                        "{:?} tracking isn't monotonic for tasks: {:?} without {} ({})",
                        mode,
                        task_keys(&less)
                            .difference(&task_keys(more))
                            .collect::<Vec<_>>(),
                        changed[skip],
                        scopes
                    );
                    prop_assert!(
                        project_keys(&less).is_subset(&project_keys(more)),
                        "{:?} tracking isn't monotonic for projects: {:?} without {} ({})",
                        mode,
                        project_keys(&less)
                            .difference(&project_keys(more))
                            .collect::<Vec<_>>(),
                        changed[skip],
                        scopes
                    );
                }
            }
        }
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
fn affected_tracking_is_consistent() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(64),
        failure_persistence: None,
        ..Config::default()
    });

    let strategy = (
        workspace_strategy(),
        prop::collection::vec(prop::bool::weighted(0.3), 1..12),
    );

    let result = runner.run(&strategy, |(spec, changed)| {
        runtime.block_on(run_scenario(spec, changed))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}
