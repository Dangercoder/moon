//! Property-based tests for the action graph builder.
//!
//! Random workspaces are generated, where tasks depend on tasks in "earlier"
//! projects (directly, or through `^:` and `dependsOn`), with every dependency
//! type, `args` and `env` (in any order), serial dependencies, and persistent
//! tasks. Configs are always acyclic, so the builder must never fail, and the
//! graph it builds is checked against what the configs imply, for random
//! targets ran in a random order:
//!
//! - The graph is acyclic.
//! - With deep dependencies, the graph contains exactly one node for every
//!   task (with its args and env) that is reachable from the targets, and an
//!   edge (of the right type and direction) for every configured dependency,
//!   regardless of the order of targets.
//! - With direct dependencies, every dependency of a node is either linked in
//!   the graph, or ignored by the action context (which the task runner
//!   checks, as it would otherwise fail with a missing dependency hash), and
//!   never ignored for the targets. Cleanups are always linked.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_action::ActionNode;
use moon_action_context::ActionContext;
use moon_action_graph::{
    ActionGraph, ActionGraphBuilder, ActionGraphBuilderOptions, RunRequirements,
};
use moon_affected::UpstreamScope;
use moon_config::TaskDependencyType;
use moon_graph_utils::GraphToDot;
use moon_test_utils::WorkspaceMocker;
use moon_workspace_graph::WorkspaceGraph;
use petgraph::graph::NodeIndex;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::{FxHashMap, FxHashSet};
use starbase_sandbox::create_empty_sandbox;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const ENV_KEYS: [&str; 2] = ["A", "B"];

#[derive(Clone, Debug)]
enum RawTarget {
    /// An index into the tasks that come before the depending task.
    Earlier(usize),
    /// `^:t0`, of every project that the project depends on.
    Upstream,
}

#[derive(Clone, Debug)]
struct RawDep {
    target: RawTarget,
    type_of: TaskDependencyType,
    args: bool,
    /// Keys into `ENV_KEYS`, in the order they are configured.
    env: Vec<usize>,
}

#[derive(Clone, Debug)]
struct RawTask {
    deps: Vec<RawDep>,
    serial: bool,
    persistent: bool,
}

#[derive(Clone, Debug)]
struct RawProject {
    depends_on: Vec<usize>,
    tasks: Vec<RawTask>,
}

/// A task dependency, as configured, but with its target resolved.
#[derive(Clone, Debug)]
struct DepSpec {
    /// The resolved targets (more than one for `^:t0`).
    targets: Vec<(usize, usize)>,
    upstream: bool,
    type_of: TaskDependencyType,
    args: bool,
    env: Vec<usize>,
}

#[derive(Clone, Debug)]
struct TaskSpec {
    deps: Vec<DepSpec>,
    serial: bool,
    persistent: bool,
}

#[derive(Clone, Debug)]
struct WorkspaceSpec {
    /// The projects each project depends on, and its tasks.
    projects: Vec<(Vec<usize>, Vec<TaskSpec>)>,
}

impl WorkspaceSpec {
    fn task(&self, (project, task): (usize, usize)) -> &TaskSpec {
        &self.projects[project].1[task]
    }

    fn targets(&self) -> Vec<(usize, usize)> {
        self.projects
            .iter()
            .enumerate()
            .flat_map(|(project, (_, tasks))| (0..tasks.len()).map(move |task| (project, task)))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct Scenario {
    workspace: WorkspaceSpec,
    /// Targets to run, in order.
    run: Vec<(usize, usize)>,
}

fn dep_strategy() -> impl Strategy<Value = RawDep> {
    (
        prop_oneof![
            5 => any::<usize>().prop_map(RawTarget::Earlier),
            1 => Just(RawTarget::Upstream),
        ],
        prop_oneof![
            5 => Just(TaskDependencyType::Required),
            1 => Just(TaskDependencyType::Optional),
            2 => Just(TaskDependencyType::Wait),
            2 => Just(TaskDependencyType::Cleanup),
        ],
        prop::bool::weighted(0.3),
        prop::collection::vec(0..ENV_KEYS.len(), 0..3),
    )
        .prop_map(|(target, type_of, args, env)| RawDep {
            target,
            type_of,
            args,
            env,
        })
}

/// How dense the generated workspaces are.
#[derive(Clone, Copy)]
struct Shape {
    projects: (usize, usize),
    tasks: (usize, usize),
    deps: (usize, usize),
    serial: f64,
}

/// Many projects, with sparse dependencies between them.
const WIDE: Shape = Shape {
    projects: (1, 6),
    tasks: (1, 4),
    deps: (0, 4),
    serial: 0.4,
};

/// Few projects, where many (mostly serial) dependencies share tasks, so that
/// serial orderings are more likely to contradict a dependency.
const DENSE: Shape = Shape {
    projects: (1, 3),
    tasks: (2, 5),
    deps: (1, 4),
    serial: 0.8,
};

fn task_strategy(shape: Shape) -> impl Strategy<Value = RawTask> {
    (
        prop::collection::vec(dep_strategy(), shape.deps.0..shape.deps.1),
        prop::bool::weighted(shape.serial),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(deps, serial, persistent)| RawTask {
            deps,
            serial,
            persistent,
        })
}

fn project_strategy(shape: Shape) -> impl Strategy<Value = RawProject> {
    (
        prop::collection::vec(any::<usize>(), 0..3),
        prop::collection::vec(task_strategy(shape), shape.tasks.0..shape.tasks.1),
    )
        .prop_map(|(depends_on, tasks)| RawProject { depends_on, tasks })
}

fn scenario_strategy_with(shape: Shape) -> impl Strategy<Value = Scenario> {
    (
        prop::collection::vec(project_strategy(shape), shape.projects.0..shape.projects.1),
        prop::collection::vec(any::<usize>(), 1..6),
    )
        .prop_map(|(projects, run)| {
            let workspace = normalize(projects);
            let targets = workspace.targets();
            let mut seen = FxHashSet::default();

            Scenario {
                run: run
                    .into_iter()
                    .map(|index| targets[index % targets.len()])
                    .filter(|target| seen.insert(*target))
                    .collect(),
                workspace,
            }
        })
}

fn scenario_strategy() -> impl Strategy<Value = Scenario> {
    prop_oneof![scenario_strategy_with(WIDE), scenario_strategy_with(DENSE)]
}

/// Resolve the raw specs into a valid workspace: dependencies only point to
/// earlier tasks (so they're acyclic), only persistent tasks require other
/// persistent tasks (others must wait on them), a task depends on a target
/// with one type only, and cleanup dependencies are dropped when they
/// contradict another dependency (the task would have to run both before and
/// after them).
fn normalize(raw: Vec<RawProject>) -> WorkspaceSpec {
    let mut projects: Vec<(Vec<usize>, Vec<TaskSpec>)> = vec![];

    // Task level graph, from a task to the tasks that must run before it
    let mut before = FxHashMap::<(usize, usize), FxHashSet<(usize, usize)>>::default();

    let reaches = |before: &FxHashMap<(usize, usize), FxHashSet<(usize, usize)>>,
                   from: (usize, usize),
                   to: (usize, usize)| {
        let mut stack = vec![from];
        let mut visited = FxHashSet::default();

        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }

            if visited.insert(node) {
                stack.extend(before.get(&node).into_iter().flatten().copied());
            }
        }

        false
    };

    for (project_index, raw_project) in raw.into_iter().enumerate() {
        let mut depends_on = raw_project
            .depends_on
            .into_iter()
            .filter(|_| project_index > 0)
            .map(|index| index % project_index)
            .collect::<Vec<_>>();
        depends_on.sort();
        depends_on.dedup();

        let persistent = raw_project
            .tasks
            .iter()
            .map(|task| task.persistent)
            .collect::<Vec<_>>();

        projects.push((depends_on.clone(), vec![]));

        for (task_index, raw_task) in raw_project.tasks.into_iter().enumerate() {
            let target = (project_index, task_index);

            // Every task that comes before this one. Depending on a task in
            // another project also makes it a dependency of the project, which
            // changes what `^:` resolves to, so projects that depend on others
            // only depend on their tasks through `^:`
            let earlier = (0..project_index)
                .filter(|_| depends_on.is_empty())
                .flat_map(|project| (0..projects[project].1.len()).map(move |task| (project, task)))
                .chain((0..task_index).map(|task| (project_index, task)))
                .collect::<Vec<_>>();

            let is_persistent = |dep: (usize, usize)| {
                if dep.0 == project_index {
                    persistent[dep.1]
                } else {
                    projects[dep.0].1[dep.1].persistent
                }
            };

            let mut deps = vec![];

            // A task can only depend on a target with one type
            let mut types = FxHashMap::<(usize, usize), TaskDependencyType>::default();

            // Requirements first, then cleanups, so that a cleanup is only
            // added when it doesn't contradict a requirement
            let (cleanups, requirements): (Vec<_>, Vec<_>) = raw_task
                .deps
                .into_iter()
                .partition(|dep| matches!(dep.type_of, TaskDependencyType::Cleanup));

            for raw_dep in requirements.into_iter().chain(cleanups) {
                let (targets, upstream) = match raw_dep.target {
                    RawTarget::Earlier(index) if !earlier.is_empty() => {
                        (vec![earlier[index % earlier.len()]], false)
                    }
                    RawTarget::Upstream if !depends_on.is_empty() => (
                        depends_on.iter().map(|project| (*project, 0)).collect(),
                        true,
                    ),
                    _ => continue,
                };

                let mut type_of = raw_dep.type_of;
                let mut env = raw_dep.env;
                let mut seen = FxHashSet::default();
                env.retain(|key| seen.insert(*key));

                // Other tasks can only wait on persistent tasks, and they can't
                // clean up, as they never complete
                if raw_task.persistent && matches!(type_of, TaskDependencyType::Cleanup) {
                    continue;
                }

                if targets.iter().any(|dep| is_persistent(*dep)) {
                    if matches!(type_of, TaskDependencyType::Cleanup) {
                        continue;
                    }

                    if !raw_task.persistent {
                        type_of = TaskDependencyType::Wait;
                    }
                }

                let base_type = edge_type(type_of);

                if targets
                    .iter()
                    .any(|dep| types.get(dep).is_some_and(|other| *other != base_type))
                {
                    continue;
                }

                if matches!(type_of, TaskDependencyType::Cleanup) {
                    if targets.iter().any(|dep| reaches(&before, target, *dep)) {
                        continue;
                    }

                    for dep in &targets {
                        before.entry(*dep).or_default().insert(target);
                    }
                } else {
                    before
                        .entry(target)
                        .or_default()
                        .extend(targets.iter().copied());
                }

                for dep in &targets {
                    types.insert(*dep, base_type);
                }

                deps.push(DepSpec {
                    targets,
                    upstream,
                    type_of,
                    args: raw_dep.args,
                    env,
                });
            }

            projects[project_index].1.push(TaskSpec {
                deps,
                serial: raw_task.serial,
                persistent: raw_task.persistent,
            });
        }
    }

    WorkspaceSpec { projects }
}

fn target_label((project, task): (usize, usize)) -> String {
    format!("p{project}:t{task}")
}

fn dep_args(dep: &DepSpec) -> Vec<String> {
    if dep.args {
        vec!["--arg".into()]
    } else {
        vec![]
    }
}

fn dep_env(dep: &DepSpec) -> BTreeMap<String, String> {
    dep.env
        .iter()
        .map(|key| (ENV_KEYS[*key].to_owned(), "1".to_owned()))
        .collect()
}

fn write_workspace(spec: &WorkspaceSpec, root: &std::path::Path) {
    for (project, (depends_on, tasks)) in spec.projects.iter().enumerate() {
        let mut yaml = String::new();

        if !depends_on.is_empty() {
            yaml.push_str("dependsOn:\n");

            for dep in depends_on {
                yaml.push_str(&format!("  - 'p{dep}'\n"));
            }
        }

        yaml.push_str("tasks:\n");

        for (index, task) in tasks.iter().enumerate() {
            yaml.push_str(&format!("  t{index}:\n    command: 'echo'\n"));

            if !task.deps.is_empty() {
                yaml.push_str("    deps:\n");

                for dep in &task.deps {
                    let target = if dep.upstream {
                        "^:t0".to_owned()
                    } else {
                        target_label(dep.targets[0])
                    };

                    yaml.push_str(&format!("      - target: '{target}'\n"));
                    // Optional is a marker (for inherited deps), not a type
                    if matches!(dep.type_of, TaskDependencyType::Optional) {
                        yaml.push_str("        optional: true\n");
                    } else {
                        yaml.push_str(&format!("        type: '{}'\n", dep.type_of));
                    }

                    if dep.args {
                        yaml.push_str("        args: ['--arg']\n");
                    }

                    if !dep.env.is_empty() {
                        yaml.push_str("        env:\n");

                        // In the configured order, which must not matter
                        for key in &dep.env {
                            yaml.push_str(&format!("          {}: '1'\n", ENV_KEYS[*key]));
                        }
                    }
                }
            }

            if task.serial || task.persistent {
                yaml.push_str("    options:\n");

                if task.serial {
                    yaml.push_str("      runDepsInParallel: false\n");
                }

                // Other tasks can't depend on a task that doesn't run in CI
                if task.persistent {
                    yaml.push_str("      persistent: true\n      runInCI: true\n");
                }
            }
        }

        let path = root.join(format!("p{project}/moon.yml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, yaml).unwrap();
    }
}

/// A `RunTask` node, by its target, args, and env (which is unordered).
type NodeKey = (String, Vec<String>, BTreeMap<String, String>);

fn node_key(node: &ActionNode) -> Option<NodeKey> {
    match node {
        ActionNode::RunTask(inner) => Some((
            inner.target.to_string(),
            inner.args.clone(),
            inner
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone().unwrap_or_default()))
                .collect(),
        )),
        _ => None,
    }
}

struct Built {
    context: ActionContext,
    graph: ActionGraph,
    keys: FxHashMap<NodeKey, NodeIndex>,
}

impl Built {
    fn has_edge(&self, from: NodeIndex, to: NodeIndex, type_of: TaskDependencyType) -> bool {
        self.graph
            .get_inner_graph()
            .graph()
            .edges_connecting(from, to)
            .any(|edge| *edge.weight() == type_of)
    }
}

/// The action graph has always used required edges for optional dependencies.
fn edge_type(type_of: TaskDependencyType) -> TaskDependencyType {
    if type_of.is_required_type() {
        TaskDependencyType::Required
    } else {
        type_of
    }
}

async fn build(
    mocker: &WorkspaceMocker,
    workspace_graph: &Arc<WorkspaceGraph>,
    run: &[(usize, usize)],
    dependencies: UpstreamScope,
) -> Result<Built, TestCaseError> {
    let mut builder = ActionGraphBuilder::new(
        Arc::new(mocker.mock_app_context()),
        Arc::clone(workspace_graph),
        ActionGraphBuilderOptions::new(false),
    )
    .unwrap();

    let reqs = RunRequirements {
        ci: false,
        dependencies,
        ..RunRequirements::default()
    };

    for target in run {
        let task = workspace_graph
            .get_task_from_project(&format!("p{}", target.0), &format!("t{}", target.1))
            .unwrap();

        if let Err(error) = builder.run_task(&task, &reqs).await {
            return Err(TestCaseError::fail(format!(
                "failed to run {} ({dependencies:?}): {error:?}",
                target_label(*target)
            )));
        }
    }

    let (context, graph) = builder.build();

    if let Err(error) = graph.sort_topological() {
        return Err(TestCaseError::fail(format!(
            "graph has a cycle ({dependencies:?}): {error:?}\n{}",
            graph.to_dot()
        )));
    }

    let mut keys = FxHashMap::default();

    for (index, node) in graph.get_inner_nodes() {
        if let Some(key) = node_key(node) {
            prop_assert!(
                keys.insert(key.clone(), *index).is_none(),
                "{key:?} is in the graph more than once ({dependencies:?})"
            );
        }
    }

    Ok(Built {
        context,
        graph,
        keys,
    })
}

/// Every node that must be in the graph when running the targets with deep
/// dependencies, and the configured dependencies of each node.
fn expected_closure(
    spec: &WorkspaceSpec,
    run: &[(usize, usize)],
) -> FxHashMap<NodeKey, (usize, usize)> {
    let mut nodes = FxHashMap::default();
    let mut stack = run
        .iter()
        .map(|target| {
            (
                (target_label(*target), vec![], BTreeMap::default()),
                *target,
            )
        })
        .collect::<Vec<_>>();

    while let Some((key, target)) = stack.pop() {
        if nodes.insert(key, target).is_some() {
            continue;
        }

        for dep in &spec.task(target).deps {
            for dep_target in &dep.targets {
                stack.push((
                    (target_label(*dep_target), dep_args(dep), dep_env(dep)),
                    *dep_target,
                ));
            }
        }
    }

    nodes
}

fn check_deep(
    spec: &WorkspaceSpec,
    run: &[(usize, usize)],
    built: &Built,
) -> Result<(), TestCaseError> {
    let expected = expected_closure(spec, run);

    let actual = built.keys.keys().cloned().collect::<BTreeSet<_>>();
    let wanted = expected.keys().cloned().collect::<BTreeSet<_>>();

    prop_assert_eq!(
        &actual,
        &wanted,
        "graph doesn't contain exactly the tasks to run (missing: {:?}, extra: {:?})",
        wanted.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&wanted).collect::<Vec<_>>()
    );

    for (key, target) in &expected {
        let index = built.keys[key];

        for dep in &spec.task(*target).deps {
            for dep_target in &dep.targets {
                let dep_key = (target_label(*dep_target), dep_args(dep), dep_env(dep));
                let dep_index = built.keys[&dep_key];

                // Cleanups run after the task, so the edge is reversed
                let (from, to) = if matches!(dep.type_of, TaskDependencyType::Cleanup) {
                    (dep_index, index)
                } else {
                    (index, dep_index)
                };

                prop_assert!(
                    built.has_edge(from, to, edge_type(dep.type_of)),
                    "missing {:?} edge between {:?} and {:?}",
                    dep.type_of,
                    key,
                    dep_key
                );
            }
        }
    }

    Ok(())
}

fn check_direct(
    spec: &WorkspaceSpec,
    run: &[(usize, usize)],
    built: &Built,
) -> Result<(), TestCaseError> {
    for (key, index) in &built.keys {
        let target = spec
            .targets()
            .into_iter()
            .find(|target| target_label(*target) == key.0)
            .unwrap();
        let task_target = moon_task::Target::parse(&key.0).unwrap();

        for dep in &spec.task(target).deps {
            for dep_target in &dep.targets {
                let dep_label = target_label(*dep_target);
                let dep_key = (dep_label.clone(), dep_args(dep), dep_env(dep));
                let dep_index = built.keys.get(&dep_key);

                // Cleanups always run after the task, regardless of scope
                if matches!(dep.type_of, TaskDependencyType::Cleanup) {
                    prop_assert!(
                        dep_index.is_some_and(|dep_index| built.has_edge(
                            *dep_index,
                            *index,
                            TaskDependencyType::Cleanup
                        )),
                        "cleanup {dep_key:?} isn't linked after {key:?} (direct)"
                    );

                    continue;
                }

                let ignored = built.context.is_dependency_ignored(
                    &task_target,
                    moon_task::Target::parse(&dep_label).unwrap(),
                );

                prop_assert!(
                    !ignored || !run.contains(&target),
                    "dependency {dep_label} of {key:?} is ignored, although it was requested (direct)"
                );

                prop_assert!(
                    ignored
                        || dep_index.is_some_and(|dep_index| built.has_edge(
                            *index,
                            *dep_index,
                            edge_type(dep.type_of)
                        )),
                    "dependency {dep_key:?} of {key:?} is neither linked, nor ignored (direct)"
                );
            }
        }
    }

    Ok(())
}

async fn run_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let sandbox = create_empty_sandbox();

    write_workspace(&scenario.workspace, sandbox.path());

    let mocker = WorkspaceMocker::new(sandbox.path())
        .load_default_configs()
        .with_default_projects();
    let workspace_graph = Arc::new(mocker.mock_workspace_graph().await);

    let mut reversed = scenario.run.clone();
    reversed.reverse();

    let deep = build(
        &mocker,
        &workspace_graph,
        &scenario.run,
        UpstreamScope::Deep,
    )
    .await?;
    check_deep(&scenario.workspace, &scenario.run, &deep)?;

    let deep_reversed = build(&mocker, &workspace_graph, &reversed, UpstreamScope::Deep).await?;
    check_deep(&scenario.workspace, &reversed, &deep_reversed)?;

    let direct = build(
        &mocker,
        &workspace_graph,
        &scenario.run,
        UpstreamScope::Direct,
    )
    .await?;
    check_direct(&scenario.workspace, &scenario.run, &direct)?;

    let direct_reversed =
        build(&mocker, &workspace_graph, &reversed, UpstreamScope::Direct).await?;
    check_direct(&scenario.workspace, &reversed, &direct_reversed)?;

    // The targets are the same, so the order they're ran in must not matter
    prop_assert_eq!(
        direct.keys.keys().collect::<BTreeSet<_>>(),
        direct_reversed.keys.keys().collect::<BTreeSet<_>>(),
        "tasks in the graph depend on the order of targets (direct)"
    );

    Ok(())
}

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(default)
}

#[test]
fn builds_the_graph_that_configs_imply() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(128),
        failure_persistence: None,
        ..Config::default()
    });

    let result = runner.run(&scenario_strategy(), |scenario| {
        runtime.block_on(run_scenario(scenario))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}
