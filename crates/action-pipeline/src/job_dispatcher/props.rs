//! Property-based tests for the job dispatcher.
//!
//! Random action graphs (with every dependency type, parallel edges of
//! different types, duplicate targets, persistent tasks, and non-task nodes)
//! are dispatched like the pipeline does, while jobs complete in a random
//! order (sometimes more than once). Every dispatch is checked against a model
//! of the graph's edges, instead of the dispatcher's own bookkeeping, so that
//! a mistake in how relations are extracted or unblocked is caught as well.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use super::JobDispatcher;
use crate::abort_state::AbortState;
use crate::event_emitter::EventEmitter;
use crate::job_context::{CompletedJob, JobContext};
use moon_action::{Action, ActionNode, RunTaskNode, SyncProjectNode};
use moon_action_graph::{ActionGraph, ActionGraphType};
use moon_common::Id;
use moon_config::TaskDependencyType;
use moon_task::Target;
use moon_workspace_graph::WorkspaceGraph;
use petgraph::graph::NodeIndex;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
enum NodeSpec {
    /// A `RunTask` node. Tasks that share a `name` share a target (and
    /// must not run in parallel), but are distinct nodes through `args`.
    Task {
        name: u8,
        args: u8,
        persistent: bool,
        priority: u8,
    },
    /// A non-task node (critical priority), which is walked through by the
    /// dispatcher when looking for something to run.
    Sync { project: u8 },
}

#[derive(Clone, Debug)]
struct GraphSpec {
    nodes: Vec<NodeSpec>,
    /// `(from, to, type)` where `from > to`, so that the graph is acyclic.
    edges: Vec<(usize, usize, TaskDependencyType)>,
}

/// How a job completes when the driver lets it.
#[derive(Clone, Copy, Debug)]
struct Completion {
    pick: usize,
    started: bool,
    executed: bool,
    failed: bool,
    /// Also report an already completed job as completed again, like a
    /// persistent job that is marked when dispatched, and again on exit.
    duplicate: Option<usize>,
}

#[derive(Clone, Debug)]
struct Scenario {
    graph: GraphSpec,
    completions: Vec<Completion>,
    /// Abort the pipeline after this many jobs have completed.
    abort_after: Option<usize>,
    /// Give up on running jobs after this many iterations of the cleanup loop.
    give_up_after: Option<usize>,
}

fn dep_type_strategy() -> impl Strategy<Value = TaskDependencyType> {
    prop_oneof![
        4 => Just(TaskDependencyType::Required),
        1 => Just(TaskDependencyType::Optional),
        2 => Just(TaskDependencyType::Wait),
        2 => Just(TaskDependencyType::Cleanup),
    ]
}

fn node_strategy() -> impl Strategy<Value = NodeSpec> {
    prop_oneof![
        6 => (0..4u8, 0..2u8, prop::bool::weighted(0.1), 0..4u8).prop_map(
            |(name, args, persistent, priority)| NodeSpec::Task {
                name,
                args,
                persistent,
                priority,
            }
        ),
        1 => (0..2u8).prop_map(|project| NodeSpec::Sync { project }),
    ]
}

fn graph_strategy() -> impl Strategy<Value = GraphSpec> {
    prop::collection::vec(node_strategy(), 2..12)
        .prop_flat_map(|nodes| {
            let count = nodes.len();

            (
                Just(nodes),
                prop::collection::vec(
                    (any::<Index>(), any::<Index>(), dep_type_strategy()),
                    0..count * 3,
                )
                .prop_map(move |edges| {
                    edges
                        .into_iter()
                        .filter_map(|(a, b, edge)| {
                            let (a, b) = (a.index(count), b.index(count));

                            (a != b).then(|| (a.max(b), a.min(b), edge))
                        })
                        .collect::<Vec<_>>()
                }),
            )
        })
        .prop_map(|(nodes, edges)| {
            // Duplicate nodes are merged by the action graph builder, so
            // the same node can't be inserted twice
            let mut seen = FxHashSet::default();
            let nodes = nodes
                .into_iter()
                .enumerate()
                .map(|(index, node)| {
                    let key = match node {
                        NodeSpec::Task { name, args, .. } => (0, name, args),
                        NodeSpec::Sync { project } => (1, project, 0),
                    };

                    if seen.insert(key) {
                        node
                    } else {
                        // Unique by construction
                        NodeSpec::Task {
                            name: 100 + index as u8,
                            args: 0,
                            persistent: false,
                            priority: 2,
                        }
                    }
                })
                .collect();

            GraphSpec { nodes, edges }
        })
}

fn completion_strategy() -> impl Strategy<Value = Completion> {
    (
        0..16usize,
        prop::bool::weighted(0.8),
        prop::bool::weighted(0.6),
        prop::bool::weighted(0.2),
        prop::option::weighted(0.2, 0..16usize),
    )
        .prop_map(|(pick, started, executed, failed, duplicate)| Completion {
            pick,
            started,
            // Only a job that started can run its command
            executed: started && executed,
            failed,
            duplicate,
        })
}

fn scenario_strategy() -> impl Strategy<Value = Scenario> {
    (
        graph_strategy(),
        prop::collection::vec(completion_strategy(), 1..40),
        prop::option::weighted(0.4, 0..12usize),
        prop::option::weighted(0.2, 0..6usize),
    )
        .prop_map(
            |(graph, completions, abort_after, give_up_after)| Scenario {
                graph,
                completions,
                abort_after,
                give_up_after,
            },
        )
}

fn create_node(spec: &NodeSpec) -> ActionNode {
    match *spec {
        NodeSpec::Task {
            name,
            args,
            persistent,
            priority,
        } => {
            let mut node = RunTaskNode::new(Target::parse(&format!("root:t{name}")).unwrap());
            node.args = (0..args).map(|arg| format!("--arg{arg}")).collect();
            node.persistent = persistent;
            node.priority = priority;

            ActionNode::run_task(node)
        }
        NodeSpec::Sync { project } => ActionNode::sync_project(SyncProjectNode {
            project_id: Id::raw(format!("p{project}")),
        }),
    }
}

fn create_action_graph(spec: &GraphSpec) -> ActionGraph {
    let mut graph = ActionGraphType::new();
    let mut nodes = FxHashMap::default();
    let mut cleanups = FxHashSet::default();

    for node in &spec.nodes {
        let index = graph.add_node(NodeIndex::new(graph.node_count()));

        nodes.insert(index, create_node(node));
    }

    for (from, to, edge) in &spec.edges {
        graph
            .add_edge(NodeIndex::new(*from), NodeIndex::new(*to), *edge)
            .unwrap();

        if matches!(edge, TaskDependencyType::Cleanup) {
            cleanups.insert(NodeIndex::new(*from));
        }
    }

    ActionGraph::new_with_cleanups(graph, nodes, cleanups)
}

fn create_job_context() -> (JobContext, mpsc::UnboundedReceiver<CompletedJob>) {
    let (sender, _receiver) = mpsc::channel::<Action>(8);
    let (completed_sender, completed_receiver) = mpsc::unbounded_channel::<CompletedJob>();

    let context = JobContext {
        abort_token: CancellationToken::new(),
        abort_state: Arc::new(AbortState::default()),
        bail: false,
        cancel_token: CancellationToken::new(),
        completed_queue: completed_sender,
        daemon_client: None,
        emitter: Arc::new(EventEmitter::default()),
        result_sender: sender,
        running_jobs: Arc::new(RwLock::new(FxHashMap::default())),
        semaphore: Arc::new(Semaphore::new(1)),
        workspace_graph: Arc::new(WorkspaceGraph::default()),
    };

    (context, completed_receiver)
}

/// The test's own view of which jobs have been dispatched and completed.
struct Model<'a> {
    spec: &'a GraphSpec,
    graph: &'a ActionGraph,
    context: JobContext,
    cleanups: FxHashSet<usize>,
    dispatched: FxHashSet<usize>,
    /// The first completion of each job.
    completed: FxHashMap<usize, CompletedJob>,
    /// Standard jobs that have been dispatched, and are yet to complete.
    running: Vec<usize>,
    completions: std::iter::Cycle<std::slice::Iter<'a, Completion>>,
}

impl Model<'_> {
    fn node(&self, index: usize) -> &ActionNode {
        self.graph
            .get_node_from_index(&NodeIndex::new(index))
            .unwrap()
    }

    fn exclusive_id(&self, index: usize) -> Option<u64> {
        let node = self.node(index);
        let id = node.get_id();

        (id > 0 && node.is_standard()).then_some(id)
    }

    fn edges_from(&self, index: usize) -> impl Iterator<Item = (usize, TaskDependencyType)> + '_ {
        self.spec
            .edges
            .iter()
            .filter(move |(from, _, _)| *from == index)
            .map(|(_, to, edge)| (*to, *edge))
    }

    fn has_started(&self, index: usize) -> bool {
        self.completed.get(&index).is_some_and(|job| job.started)
    }

    async fn complete(&mut self, job: CompletedJob) {
        // Like `JobContext::mark_completed_job`
        self.context.running_jobs.write().await.remove(&job.index);
        self.context.completed_queue.send(job).unwrap();

        self.completed.entry(job.index.index()).or_insert(job);
        self.running.retain(|index| *index != job.index.index());
    }

    /// Complete one of the running jobs, and possibly report another completed
    /// job again. Returns false if nothing is running.
    async fn complete_next(&mut self) -> bool {
        if self.running.is_empty() {
            return false;
        }

        let step = *self.completions.next().unwrap();
        let index = self.running[step.pick % self.running.len()];

        self.complete(CompletedJob {
            index: NodeIndex::new(index),
            started: step.started,
            failed: step.failed,
            executed: step.executed,
        })
        .await;

        if let Some(duplicate) = step.duplicate {
            let mut completed = self.completed.values().copied().collect::<Vec<_>>();
            completed.sort_by_key(|job| job.index);

            let job = completed[duplicate % completed.len()];

            self.context.completed_queue.send(job).unwrap();
        }

        true
    }

    /// Check a job that the dispatcher returned from `next`.
    fn check_dispatch(&self, index: usize) -> Result<(), TestCaseError> {
        prop_assert!(
            !self.dispatched.contains(&index),
            "{} was dispatched more than once",
            self.node(index).label()
        );

        for (dep, edge) in self.edges_from(index) {
            if matches!(edge, TaskDependencyType::Wait) {
                prop_assert!(
                    self.dispatched.contains(&dep),
                    "{} was dispatched before {} (that it waits on) started",
                    self.node(index).label(),
                    self.node(dep).label()
                );
            } else {
                prop_assert!(
                    self.completed.contains_key(&dep),
                    "{} was dispatched before its {edge:?} dependency {} completed",
                    self.node(index).label(),
                    self.node(dep).label()
                );
            }
        }

        self.check_exclusive(index)
    }

    fn check_exclusive(&self, index: usize) -> Result<(), TestCaseError> {
        self.check_exclusive_among(index, self.running.iter())
    }

    fn check_exclusive_among<'r>(
        &self,
        index: usize,
        running: impl Iterator<Item = &'r usize>,
    ) -> Result<(), TestCaseError> {
        if let Some(id) = self.exclusive_id(index) {
            for running in running {
                prop_assert!(
                    self.exclusive_id(*running) != Some(id),
                    "{} was dispatched while {} (the same action) was running",
                    self.node(index).label(),
                    self.node(*running).label()
                );
            }
        }

        Ok(())
    }

    /// Check a cleanup job that the dispatcher returned after the pipeline
    /// was aborted. The order is checked against completions *so far*, as
    /// the job is running from this point onwards.
    fn check_cleanup_after_abort(&self, index: usize, give_up: bool) -> Result<(), TestCaseError> {
        let label = self.node(index).label();

        prop_assert!(
            self.cleanups.contains(&index),
            "{label} was ran after the pipeline was aborted, but is not a cleanup job"
        );
        prop_assert!(
            !self.dispatched.contains(&index),
            "{label} was dispatched more than once"
        );

        for (dep, edge) in self.edges_from(index) {
            let dep_label = self.node(dep).label();

            match edge {
                TaskDependencyType::Required | TaskDependencyType::Optional => {
                    prop_assert!(
                        self.has_started(dep),
                        "{label} ran after the abort, but its dependency {dep_label} never started"
                    );

                    if !matches!(self.node(dep), ActionNode::RunTask(_)) {
                        prop_assert!(
                            !self.completed[&dep].failed,
                            "{label} ran after the abort, but its dependency {dep_label} failed"
                        );
                    }
                }
                TaskDependencyType::Cleanup => {
                    let still_running = self.running.contains(&dep);

                    prop_assert!(
                        !still_running || (give_up && !self.cleanups.contains(&dep)),
                        "{label} ran after the abort, while {dep_label} (that it cleans up after) was still running"
                    );
                }
                // Known issue: when the cleanup job also cleans up after the job
                // that it waits on, only the cleanup is checked, so it may run
                // although that job never started (see `known_issues`)
                TaskDependencyType::Wait
                    if self.edges_from(index).any(|(to, edge)| {
                        to == dep && matches!(edge, TaskDependencyType::Cleanup)
                    }) => {}
                TaskDependencyType::Wait => {
                    prop_assert!(
                        self.has_started(dep) || self.running.contains(&dep),
                        "{label} ran after the abort, but {dep_label} (that it waits on) never started"
                    );
                }
            };
        }

        // Known issue: only other cleanup jobs are checked for the same action,
        // and not jobs that were running when aborted (see `known_issues`)
        self.check_exclusive_among(
            index,
            self.running
                .iter()
                .filter(|running| self.cleanups.contains(running)),
        )
    }
}

async fn run_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let graph = create_action_graph(&scenario.graph);
    let (context, completed_queue) = create_job_context();
    let groups = graph.group_priorities(graph.sort_topological().unwrap());
    let mut dispatcher = JobDispatcher::new(&graph, context.clone(), groups, completed_queue);
    let total = scenario.graph.nodes.len();

    let mut model = Model {
        spec: &scenario.graph,
        graph: &graph,
        context: context.clone(),
        cleanups: graph
            .get_cleanup_indices()
            .iter()
            .map(|index| index.index())
            .collect(),
        dispatched: FxHashSet::default(),
        completed: FxHashMap::default(),
        running: vec![],
        completions: scenario.completions.iter().cycle(),
    };

    let mut completed_count = 0;
    let mut aborted = false;

    // Dispatch like the pipeline does: until blocked, and only then let a
    // single running job complete
    while dispatcher.has_queued_jobs() {
        if scenario.abort_after.is_some_and(|at| completed_count >= at) {
            context.abort_token.cancel();
            aborted = true;
            break;
        }

        match dispatcher.next().await {
            Some(index) => {
                let index = index.index();

                model.check_dispatch(index)?;
                model.dispatched.insert(index);

                // Persistent jobs are marked as completed once dispatched
                if model.node(index).is_persistent() {
                    model
                        .complete(CompletedJob {
                            index: NodeIndex::new(index),
                            started: true,
                            failed: false,
                            executed: false,
                        })
                        .await;
                } else {
                    model.running.push(index);
                }
            }
            None => {
                prop_assert!(
                    model.complete_next().await,
                    "dispatcher is stuck: nothing is running, but {} of {total} jobs were never dispatched",
                    total - model.dispatched.len()
                );

                completed_count += 1;
            }
        }
    }

    if !aborted {
        prop_assert_eq!(model.dispatched.len(), total);

        return Ok(());
    }

    // Then run cleanup jobs like the pipeline does once aborted: until
    // nothing else can run, and nothing is running
    let mut gave_up = false;

    for iteration in 0.. {
        prop_assert!(iteration <= total * 4, "cleanup jobs never stopped running");

        let give_up = scenario.give_up_after.is_some_and(|at| iteration >= at);
        gave_up |= give_up;

        for index in dispatcher.take_cleanups_after_abort(give_up) {
            let index = index.index();

            model.check_cleanup_after_abort(index, give_up)?;
            model.dispatched.insert(index);
            model.running.push(index);
        }

        if !dispatcher.is_waiting_on_cleanups(give_up) || !model.complete_next().await {
            break;
        }
    }

    // A cleanup job that only cleans up after other jobs must have ran, when
    // one of those jobs ran its command, as there's something to clean up
    if !gave_up {
        for cleanup in &model.cleanups {
            if model.dispatched.contains(cleanup)
                || !model
                    .edges_from(*cleanup)
                    .all(|(_, edge)| matches!(edge, TaskDependencyType::Cleanup))
            {
                continue;
            }

            let ran_parent = model
                .edges_from(*cleanup)
                .find(|(dep, _)| model.completed.get(dep).is_some_and(|job| job.executed));

            prop_assert!(
                ran_parent.is_none(),
                "{} never ran after the abort, but {} (that it cleans up after) ran its command",
                model.node(*cleanup).label(),
                model.node(ran_parent.unwrap().0).label()
            );
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
fn dispatches_every_job_in_dependency_order() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(512),
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

/// Counterexamples found by the properties above, that are excluded from them,
/// as they need a decision on how the dispatcher should behave. Run them with
/// `--run-ignored only`.
mod known_issues {
    use super::*;

    fn complete(context: &JobContext, index: usize, started: bool) {
        context
            .completed_queue
            .send(CompletedJob {
                index: NodeIndex::new(index),
                started,
                failed: false,
                executed: false,
            })
            .unwrap();
    }

    fn task(name: u8, args: u8, priority: u8) -> NodeSpec {
        NodeSpec::Task {
            name,
            args,
            persistent: false,
            priority,
        }
    }

    // A relation that both cleans up after and waits on a job is checked as a
    // cleanup only (`else if` in `can_cleanup_after_abort`), so the wait isn't
    // honored, although the builder links both relationships to be honored
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "known issue: cleanup that also waits on its parent runs when the parent never started"]
    async fn cleanup_that_also_waits_runs_when_parent_never_started() {
        // `c` cleans up after and waits on `p`, which requires (and waits on) `s`
        let spec = GraphSpec {
            nodes: vec![task(0, 0, 0), task(1, 0, 2), task(2, 0, 2)],
            edges: vec![
                (2, 1, TaskDependencyType::Cleanup),
                (2, 1, TaskDependencyType::Wait),
                (1, 0, TaskDependencyType::Required),
                (1, 0, TaskDependencyType::Wait),
            ],
        };
        let graph = create_action_graph(&spec);
        let (context, completed) = create_job_context();
        let groups = graph.group_priorities(graph.sort_topological().unwrap());
        let mut dispatcher = JobDispatcher::new(&graph, context.clone(), groups, completed);

        assert_eq!(dispatcher.next().await, Some(NodeIndex::new(0)));

        // `s` completes, but the pipeline is aborted before `p` is dispatched
        complete(&context, 0, true);
        context.abort_token.cancel();

        assert!(
            dispatcher.take_cleanups_after_abort(false).is_empty(),
            "`c` ran, but `p` (that it waits on) never started"
        );
    }

    // `take_cleanups_after_abort` only collects the IDs of running cleanup
    // jobs, so a cleanup job may run alongside the same action (with other
    // args or env) that was running when the pipeline was aborted
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "known issue: same action runs in parallel once aborted"]
    async fn cleanup_runs_alongside_the_same_action_once_aborted() {
        // `t --a` requires `p`, and `t` cleans up after `p`
        let spec = GraphSpec {
            nodes: vec![task(0, 0, 2), task(1, 1, 2), task(1, 0, 2)],
            edges: vec![
                (1, 0, TaskDependencyType::Required),
                (2, 0, TaskDependencyType::Cleanup),
            ],
        };
        let graph = create_action_graph(&spec);
        let (context, completed) = create_job_context();
        let groups = graph.group_priorities(graph.sort_topological().unwrap());
        let mut dispatcher = JobDispatcher::new(&graph, context.clone(), groups, completed);

        assert_eq!(dispatcher.next().await, Some(NodeIndex::new(0)));

        context
            .running_jobs
            .write()
            .await
            .remove(&NodeIndex::new(0));
        context
            .completed_queue
            .send(CompletedJob {
                index: NodeIndex::new(0),
                started: true,
                failed: false,
                executed: true,
            })
            .unwrap();

        // `t --a` is running when the pipeline is aborted
        assert_eq!(dispatcher.next().await, Some(NodeIndex::new(1)));
        context.abort_token.cancel();

        assert!(
            dispatcher.take_cleanups_after_abort(false).is_empty(),
            "`t` ran while `t --a` (the same action) was still running"
        );
    }
}
