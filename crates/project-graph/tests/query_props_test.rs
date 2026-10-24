//! Property-based tests for the query language (MQL).
//!
//! Random workspaces (projects with tags, layers, stacks, and languages, and
//! tasks with tags and types) are queried with random queries (every field and
//! operator, lists of values, and nested groups of `&&` and `||`), for both
//! projects and tasks, and the results are compared against a reference
//! evaluator that follows the documented semantics:
//!
//! - `=` and `~` match when any of the values match (for fields with many
//!   values, like tags, when any of them match), and `!=` and `!~` match
//!   exactly when their positive form doesn't. A negated comparison excludes
//!   an item with a matching value.
//! - `&&` and `||` are an intersection and union of their conditions.
//! - When querying projects, task fields match a project with a matching task.
//!   When querying tasks, project fields match the task's project.
//!
//! Set `PROPTEST_CASES` to run more (or fewer) cases.

use moon_query::build_query;
use moon_test_utils::WorkspaceMocker;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use starbase_sandbox::create_empty_sandbox;
use std::collections::BTreeSet;

const TAGS: [&str; 3] = ["a", "b", "c"];
const LAYERS: [&str; 3] = ["application", "library", "tool"];
const STACKS: [&str; 2] = ["frontend", "backend"];
const LANGUAGES: [&str; 3] = ["rust", "javascript", "python"];
const TASK_TYPES: [&str; 3] = ["build", "run", "test"];
const TASK_IDS: [&str; 3] = ["t0", "t1", "t2"];
const PROJECTS: usize = 4;

#[derive(Clone, Debug)]
struct TaskSpec {
    id: usize,
    tags: Vec<usize>,
    type_of: usize,
}

#[derive(Clone, Debug)]
struct ProjectSpec {
    tags: Vec<usize>,
    layer: usize,
    stack: usize,
    language: usize,
    tasks: Vec<TaskSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Project,
    ProjectId,
    ProjectTag,
    ProjectLayer,
    ProjectStack,
    Language,
    Task,
    TaskTag,
    TaskType,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::ProjectId => "projectId",
            Self::ProjectTag => "projectTag",
            Self::ProjectLayer => "projectLayer",
            Self::ProjectStack => "projectStack",
            Self::Language => "language",
            Self::Task => "task",
            Self::TaskTag => "taskTag",
            Self::TaskType => "taskType",
        }
    }

    fn values(self) -> Vec<String> {
        let values: Vec<&str> = match self {
            Self::Project | Self::ProjectId => vec!["p0", "p1", "p2", "p3"],
            Self::ProjectTag | Self::TaskTag => TAGS.to_vec(),
            Self::ProjectLayer => LAYERS.to_vec(),
            Self::ProjectStack => STACKS.to_vec(),
            Self::Language => LANGUAGES.to_vec(),
            Self::Task => TASK_IDS.to_vec(),
            Self::TaskType => TASK_TYPES.to_vec(),
        };

        values.into_iter().map(String::from).collect()
    }

    /// Like comparisons can only be used on non-enum fields.
    fn supports_like(self) -> bool {
        matches!(
            self,
            Self::Project | Self::ProjectId | Self::ProjectTag | Self::Task | Self::TaskTag
        )
    }

    fn is_task_field(self) -> bool {
        matches!(self, Self::Task | Self::TaskTag | Self::TaskType)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Equal,
    NotEqual,
    Like,
    NotLike,
}

impl Op {
    fn symbol(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Like => "~",
            Self::NotLike => "!~",
        }
    }

    fn is_negated(self) -> bool {
        matches!(self, Self::NotEqual | Self::NotLike)
    }

    fn is_like(self) -> bool {
        matches!(self, Self::Like | Self::NotLike)
    }
}

#[derive(Clone, Debug)]
enum Query {
    Comparison {
        field: Field,
        op: Op,
        values: Vec<String>,
    },
    Group {
        and: bool,
        conditions: Vec<Query>,
    },
}

impl Query {
    fn render(&self, nested: bool) -> String {
        match self {
            Self::Comparison { field, op, values } => {
                let value = if values.len() == 1 {
                    values[0].clone()
                } else {
                    format!("[{}]", values.join(", "))
                };

                format!("{}{}{}", field.name(), op.symbol(), value)
            }
            Self::Group { and, conditions } => {
                let joined = conditions
                    .iter()
                    .map(|condition| condition.render(true))
                    .collect::<Vec<_>>()
                    .join(if *and { " && " } else { " || " });

                if nested {
                    format!("({joined})")
                } else {
                    joined
                }
            }
        }
    }
}

fn comparison_strategy() -> impl Strategy<Value = Query> {
    prop_oneof![
        Just(Field::Project),
        Just(Field::ProjectId),
        Just(Field::ProjectTag),
        Just(Field::ProjectLayer),
        Just(Field::ProjectStack),
        Just(Field::Language),
        Just(Field::Task),
        Just(Field::TaskTag),
        Just(Field::TaskType),
    ]
    .prop_flat_map(|field| {
        let ops = if field.supports_like() {
            vec![Op::Equal, Op::NotEqual, Op::Like, Op::NotLike]
        } else {
            vec![Op::Equal, Op::NotEqual]
        };
        let mut values = field.values();

        // Wildcards for like comparisons
        if field.supports_like() {
            let prefix = values[0][..1].to_owned();
            values.push(format!("{prefix}*"));
            values.push("*".into());
        }

        (
            Just(field),
            prop::sample::select(ops),
            prop::sample::subsequence(values.clone(), 1..3),
        )
    })
    .prop_filter_map("wildcards are only for like", |(field, op, mut values)| {
        // Like comparisons take a single glob, not a list
        if op.is_like() {
            values.truncate(1);
        }

        (op.is_like() || values.iter().all(|value| !value.contains('*')))
            .then_some(Query::Comparison { field, op, values })
    })
}

fn query_strategy() -> impl Strategy<Value = Query> {
    comparison_strategy().prop_recursive(2, 8, 3, |inner| {
        // A group must start with a comparison, not another group
        (
            any::<bool>(),
            comparison_strategy(),
            prop::collection::vec(inner, 1..3),
        )
            .prop_map(|(and, first, rest)| Query::Group {
                and,
                conditions: std::iter::once(first).chain(rest).collect(),
            })
    })
}

fn project_strategy() -> impl Strategy<Value = ProjectSpec> {
    (
        prop::sample::subsequence((0..TAGS.len()).collect::<Vec<_>>(), 0..3),
        0..LAYERS.len(),
        0..STACKS.len(),
        0..LANGUAGES.len(),
        prop::sample::subsequence((0..TASK_IDS.len()).collect::<Vec<_>>(), 0..4),
        prop::collection::vec(
            (
                prop::sample::subsequence((0..TAGS.len()).collect::<Vec<_>>(), 0..3),
                0..TASK_TYPES.len(),
            ),
            3,
        ),
    )
        .prop_map(
            |(tags, layer, stack, language, task_ids, task_attrs)| ProjectSpec {
                tags,
                layer,
                stack,
                language,
                tasks: task_ids
                    .into_iter()
                    .map(|id| TaskSpec {
                        id,
                        tags: task_attrs[id].0.clone(),
                        type_of: task_attrs[id].1,
                    })
                    .collect(),
            },
        )
}

fn write_workspace(projects: &[ProjectSpec], root: &std::path::Path) {
    let list = |values: &[usize], names: &[&str]| {
        values
            .iter()
            .map(|value| format!("'{}'", names[*value]))
            .collect::<Vec<_>>()
            .join(", ")
    };

    for (index, project) in projects.iter().enumerate() {
        let mut yaml = format!(
            "language: '{}'\nlayer: '{}'\nstack: '{}'\ntags: [{}]\n",
            LANGUAGES[project.language],
            LAYERS[project.layer],
            STACKS[project.stack],
            list(&project.tags, &TAGS),
        );

        if !project.tasks.is_empty() {
            yaml.push_str("tasks:\n");

            for task in &project.tasks {
                yaml.push_str(&format!(
                    "  {}:\n    command: 'echo'\n    type: '{}'\n    tags: [{}]\n",
                    TASK_IDS[task.id],
                    TASK_TYPES[task.type_of],
                    list(&task.tags, &TAGS),
                ));
            }
        }

        let path = root.join(format!("p{index}/moon.yml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, yaml).unwrap();
    }
}

fn value_matches(op: Op, pattern: &str, value: &str) -> bool {
    if op.is_like() {
        match pattern.strip_suffix('*') {
            Some(prefix) => value.starts_with(prefix),
            None => pattern == value,
        }
    } else {
        pattern == value
    }
}

/// Whether any of the item's values match any of the comparison's values.
fn any_matches(op: Op, patterns: &[String], values: &[&str]) -> bool {
    values.iter().any(|value| {
        patterns
            .iter()
            .any(|pattern| value_matches(op, pattern, value))
    })
}

fn project_field_values(project_index: usize, project: &ProjectSpec, field: Field) -> Vec<String> {
    match field {
        Field::Project | Field::ProjectId => vec![format!("p{project_index}")],
        Field::ProjectTag => project.tags.iter().map(|tag| TAGS[*tag].into()).collect(),
        Field::ProjectLayer => vec![LAYERS[project.layer].into()],
        Field::ProjectStack => vec![STACKS[project.stack].into()],
        Field::Language => vec![LANGUAGES[project.language].into()],
        _ => unreachable!(),
    }
}

fn task_field_values(task: &TaskSpec, field: Field) -> Vec<String> {
    match field {
        Field::Task => vec![TASK_IDS[task.id].into()],
        Field::TaskTag => task.tags.iter().map(|tag| TAGS[*tag].into()).collect(),
        Field::TaskType => vec![TASK_TYPES[task.type_of].into()],
        _ => unreachable!(),
    }
}

/// Evaluate the query against a project, or a task of the project.
fn evaluate(
    query: &Query,
    project_index: usize,
    project: &ProjectSpec,
    task: Option<&TaskSpec>,
) -> bool {
    match query {
        Query::Comparison { field, op, values } => {
            let positive = if field.is_task_field() {
                match task {
                    Some(task) => {
                        let item = task_field_values(task, *field);
                        any_matches(
                            *op,
                            values,
                            &item.iter().map(String::as_str).collect::<Vec<_>>(),
                        )
                    }
                    // A project matches when one of its tasks matches
                    None => project.tasks.iter().any(|task| {
                        let item = task_field_values(task, *field);
                        any_matches(
                            *op,
                            values,
                            &item.iter().map(String::as_str).collect::<Vec<_>>(),
                        )
                    }),
                }
            } else {
                let item = project_field_values(project_index, project, *field);
                any_matches(
                    *op,
                    values,
                    &item.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            };

            positive != op.is_negated()
        }
        Query::Group { and, conditions } => {
            let mut results = conditions
                .iter()
                .map(|condition| evaluate(condition, project_index, project, task));

            if *and {
                results.all(|result| result)
            } else {
                results.any(|result| result)
            }
        }
    }
}

async fn run_queries(projects: Vec<ProjectSpec>, queries: Vec<Query>) -> Result<(), TestCaseError> {
    let sandbox = create_empty_sandbox();

    write_workspace(&projects, sandbox.path());

    let workspace_graph = WorkspaceMocker::new(sandbox.path())
        .load_default_configs()
        .with_default_projects()
        .mock_workspace_graph()
        .await;

    for query in &queries {
        let input = query.render(false);
        let criteria = build_query(&input)
            .map_err(|error| TestCaseError::fail(format!("failed to parse {input}: {error}")))?;

        let actual = workspace_graph
            .query_projects(&criteria)
            .unwrap()
            .iter()
            .map(|project| project.id.to_string())
            .collect::<BTreeSet<_>>();
        let expected = projects
            .iter()
            .enumerate()
            .filter(|(index, project)| evaluate(query, *index, project, None))
            .map(|(index, _)| format!("p{index}"))
            .collect::<BTreeSet<_>>();

        prop_assert_eq!(actual, expected, "projects for `{}`", input);

        let actual = workspace_graph
            .query_tasks(&criteria)
            .unwrap()
            .iter()
            .map(|task| task.target.to_string())
            .collect::<BTreeSet<_>>();
        let expected = projects
            .iter()
            .enumerate()
            .flat_map(|(index, project)| {
                project
                    .tasks
                    .iter()
                    .filter(move |task| evaluate(query, index, project, Some(task)))
                    .map(move |task| format!("p{index}:{}", TASK_IDS[task.id]))
            })
            .collect::<BTreeSet<_>>();

        prop_assert_eq!(actual, expected, "tasks for `{}`", input);
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
fn queries_match_the_documented_semantics() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut runner = TestRunner::new(Config {
        cases: cases(128),
        failure_persistence: None,
        ..Config::default()
    });

    let strategy = (
        prop::collection::vec(project_strategy(), PROJECTS),
        prop::collection::vec(query_strategy(), 1..8),
    );

    let result = runner.run(&strategy, |(projects, queries)| {
        runtime.block_on(run_queries(projects, queries))
    });

    if let Err(error) = result {
        panic!("{error}");
    }
}

/// Bugs found by the property above, as minimal tests that fail until fixed.
mod bugs {
    use super::*;
    use moon_test_utils::create_moon_sandbox;

    fn task(id: usize, tags: Vec<usize>) -> TaskSpec {
        TaskSpec {
            id,
            tags,
            type_of: 0,
        }
    }

    fn project(tags: Vec<usize>, tasks: Vec<TaskSpec>) -> ProjectSpec {
        ProjectSpec {
            tags,
            layer: 0,
            stack: 0,
            language: 0,
            tasks,
        }
    }

    async fn query_projects(projects: &[ProjectSpec], query: &str) -> BTreeSet<String> {
        let sandbox = create_empty_sandbox();
        write_workspace(projects, sandbox.path());

        WorkspaceMocker::new(sandbox.path())
            .load_default_configs()
            .with_default_projects()
            .mock_workspace_graph()
            .await
            .query_projects(build_query(query).unwrap())
            .unwrap()
            .iter()
            .map(|project| project.id.to_string())
            .collect()
    }

    async fn query_tasks(projects: &[ProjectSpec], query: &str) -> BTreeSet<String> {
        let sandbox = create_empty_sandbox();
        write_workspace(projects, sandbox.path());

        WorkspaceMocker::new(sandbox.path())
            .load_default_configs()
            .with_default_projects()
            .mock_workspace_graph()
            .await
            .query_tasks(build_query(query).unwrap())
            .unwrap()
            .iter()
            .map(|task| task.target.to_string())
            .collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    // When querying projects, task fields match a project when *any* of its
    // tasks matches the comparison, and that includes negated comparisons. So
    // `task!=t1` matches a project that has `t1` (as it has another task), and
    // never matches a project without tasks. Like `taskTag!=` before 2.6.1,
    // a negated comparison must exclude projects with a matching task.
    // See `does_project_match_criteria` in `crates/workspace-graph`.
    #[tokio::test(flavor = "multi_thread")]
    async fn negated_task_fields_exclude_projects_with_a_matching_task() {
        let projects = [
            project(vec![], vec![task(0, vec![0]), task(1, vec![])]),
            project(vec![], vec![task(0, vec![])]),
            project(vec![], vec![]),
        ];

        assert_eq!(
            query_projects(&projects, "task!=t1").await,
            set(&["p1", "p2"]),
            "`task!=t1` must not match p0, which has a `t1` task"
        );
        assert_eq!(
            query_projects(&projects, "taskTag!=a").await,
            set(&["p1", "p2"]),
            "`taskTag!=a` must not match p0, which has a task tagged `a`"
        );
    }

    // When querying tasks, project fields (other than `project`) fall through
    // to `_ => Ok(false)` in `does_task_match_criteria`, below the comment
    // "These fields match against the task's parent project". So
    // `moon query tasks "projectTag=a"` (or `language=rust`, `projectLayer=..`)
    // never matches anything, and neither does its negation.
    #[tokio::test(flavor = "multi_thread")]
    async fn task_queries_match_project_fields_against_the_parent_project() {
        let projects = [
            project(vec![0], vec![task(0, vec![])]),
            project(vec![], vec![task(0, vec![])]),
        ];

        assert_eq!(
            query_tasks(&projects, "projectTag=a").await,
            set(&["p0:t0"])
        );
        assert_eq!(
            query_tasks(&projects, "projectTag!=a").await,
            set(&["p1:t0"])
        );
        assert_eq!(
            query_tasks(&projects, "projectId=p1").await,
            set(&["p1:t0"])
        );
        assert_eq!(
            query_tasks(&projects, "language=rust").await,
            set(&["p0:t0", "p1:t0"])
        );
    }

    // The grammar (`crates/query/src/mql.pest`) requires an expression to
    // start with a comparison, so a query can't start with a group, although
    // grouping is documented, and `c && (a || b)` works.
    #[test]
    fn queries_can_start_with_a_group() {
        assert!(build_query("task=t0 && (project=p0 || project=p1)").is_ok());
        assert!(
            build_query("(project=p0 || project=p1) && task=t0").is_ok(),
            "a query that starts with a group fails to parse"
        );
    }

    // `project` matches against the ID, and otherwise against the aliases. As
    // the negation is applied to each separately, `project!=x` matches the
    // project `x` (because one of its aliases isn't `x`), and the project
    // aliased `x` (because its ID isn't `x`). Uses the aliases fixture, where
    // the `alias-one` project has the `one` alias.
    #[tokio::test(flavor = "multi_thread")]
    async fn negated_project_excludes_aliased_projects() {
        let sandbox = create_moon_sandbox("aliases");
        let workspace_graph = WorkspaceMocker::new(sandbox.path())
            .load_default_configs()
            .with_default_projects()
            .with_all_toolchains()
            .with_inherited_tasks()
            .mock_workspace_graph()
            .await;

        let ids = |query: &str| {
            workspace_graph
                .query_projects(build_query(query).unwrap())
                .unwrap()
                .iter()
                .map(|project| project.id.to_string())
                .collect::<BTreeSet<_>>()
        };

        assert!(ids("project=alias-one").contains("alias-one"));
        assert!(ids("project=one").contains("alias-one"));

        assert!(
            !ids("project!=alias-one").contains("alias-one"),
            "`project!=alias-one` matches the alias-one project"
        );
        assert!(
            !ids("project!=one").contains("alias-one"),
            "`project!=one` matches the project aliased `one`"
        );
    }
}
