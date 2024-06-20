use std::collections::BTreeMap;
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;

use anyhow::{anyhow, Context};
use askama::Template;
use axum::{extract::Query, http::StatusCode, routing::get, Router};
use axum_sqlx_tx::Layer;
use itertools::{EitherOrBoth, Itertools};
use serde::Deserialize;
use serde_json::json;
use sqlx::{sqlite::SqliteRow, Row, Sqlite};
use uuid::Uuid;

use crate::plot::Value as PlotValue;
use crate::{
    CallStats, Invocation, InvocationProgress, InvocationStatus, OuterAppState, Result,
    ResultToResponseError, Tx,
};

pub fn router(state: OuterAppState, layer: Layer<Sqlite, axum_sqlx_tx::Error>) -> Router {
    Router::new()
        .route("/", get(home))
        //.route("/invocations", get(invocations))
        //.route("/workloads", get(workloads))
        //.route("/branches", get(branches))
        //.route("/tags", get(tags))
        //.route("/commits", get(commits))
        .route("/view_spans", get(view_spans))
        .route("/view_span", get(view_span))
        .with_state(state)
        .layer(layer)
}

mod filters {
    use std::fmt::Display;

    use super::TimeWithUnit;

    pub fn to_ns(duration: &f64) -> ::askama::Result<TimeWithUnit> {
        Ok(TimeWithUnit::new(*duration))
    }

    pub fn commit(sha1: impl Display + Clone) -> ::askama::Result<String> {
        let truncated_sha1 = ::askama::filters::truncate(sha1.clone(), 9)?;
        commit_link(sha1, truncated_sha1)
    }

    pub fn commit_link(sha1: impl Display, inner: impl Display) -> ::askama::Result<String> {
        Ok(format!(
            r#"<a href="https://github.com/meilisearch/meilisearch/commit/{}">{}</a>"#,
            sha1, inner,
        ))
    }

    pub fn branch(branch_ref: impl Display) -> ::askama::Result<String> {
        let branch = branch_ref.to_string();
        let branch_name = branch.rsplit('/').next().unwrap_or(&branch);
        Ok(format!(
            r#"<a href="https://github.com/meilisearch/meilisearch/tree/{}">{}</a>"#,
            branch_name, branch_name
        ))
    }

    pub fn tag(tag_ref: impl Display) -> ::askama::Result<String> {
        Ok(format!(
            r#"<a href="https://github.com/meilisearch/meilisearch/tree/{}">{}</a>"#,
            tag_ref, tag_ref
        ))
    }

    pub fn trim_outer_p(text: impl Display) -> ::askama::Result<String> {
        Ok(text
            .to_string()
            .trim_start_matches("<p>")
            .trim_end_matches("</p>")
            .to_string())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TimeUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl Display for TimeUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TimeUnit {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeUnit::Nanoseconds => "ns",
            TimeUnit::Microseconds => "μs",
            TimeUnit::Milliseconds => "ms",
            TimeUnit::Seconds => "s",
            TimeUnit::Minutes => "min",
            TimeUnit::Hours => "h",
            TimeUnit::Days => "d",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TimeWithUnit {
    pub nano: f64,
    pub unit: TimeUnit,
}

impl TimeWithUnit {
    /// Chooses a readable unit by default
    pub fn new(nano: f64) -> TimeWithUnit {
        let unit = if nano.abs() < 10000. {
            TimeUnit::Nanoseconds
        } else if nano.abs() / 1000. < 10000. {
            TimeUnit::Microseconds
        } else if nano.abs() / 1_000_000. < 10000. {
            TimeUnit::Milliseconds
        } else if nano.abs() / 1_000_000_000. < 120. {
            TimeUnit::Seconds
        } else if nano.abs() / 1_000_000_000. / 60.0 < 600.0 {
            TimeUnit::Minutes
        } else if nano.abs() / 1_000_000_000. / 3600.0 < 48.0 {
            TimeUnit::Hours
        } else {
            TimeUnit::Days
        };

        Self { nano, unit }
    }

    pub fn as_own_unit(&self) -> f64 {
        let value = self.as_unit(self.unit);
        value.round()
    }

    pub fn as_unit(&self, unit: TimeUnit) -> f64 {
        match unit {
            TimeUnit::Nanoseconds => self.nano,
            TimeUnit::Microseconds => self.nano / 1_000.,
            TimeUnit::Milliseconds => self.nano / 1_000_000.,
            TimeUnit::Seconds => self.nano / 1_000_000_000.,
            TimeUnit::Minutes => self.nano / 1_000_000_000. / 60.,
            TimeUnit::Hours => self.nano / 1_000_000_000. / 3600.,
            TimeUnit::Days => self.nano / 1_000_000_000. / 3600. / 24.,
        }
    }

    /// Round the value up to the next "nice" value in the current unit
    ///
    /// Use to determine the end of an axis' range
    pub fn nice_axis_end_value(&self) -> f64 {
        let as_own_unit = self.as_own_unit();

        match as_own_unit.abs() {
            x if x <= 0.1 => 0.1,
            x if x <= 1. => 1.,
            x if x <= 5. => 5.,
            x if x <= 10. => 10.,
            x if x <= 50. => 50.,
            x if x <= 100. => 100.,
            x if x <= 500. => 500.,
            x if x <= 1000. => 1000.,
            x if x <= 5000. => 5000.,
            x => x,
        }
    }

    pub fn with_unit(nano: f64, unit: TimeUnit) -> Self {
        Self { nano, unit }
    }
}

impl Display for TimeWithUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.as_own_unit(), self.unit)
    }
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    latest_invocation_progress: Vec<Invocation>,
    workloads_on_main: Vec<WorkloadTemplate>,
    recent_workloads: Vec<WorkloadTemplate>,
}

#[derive(Template)]
#[template(path = "workload.html")]
struct WorkloadTemplate {
    name: String,
    commit_sha1: String,
    compared_commit_sha1: Option<String>,
    commit_message: String,
    summary: WorkloadSummaryTemplate,
    reason: Option<String>,
    target_branch: Option<String>,
    plot: String,
}

#[derive(Template, Default)]
#[template(path = "workload_summary.html")]
struct WorkloadSummaryTemplate {
    improvements: usize,
    regressions: usize,
    unchanged: usize,
    unstable: usize,
}

async fn home(mut tx: Tx) -> Result<HomeTemplate> {
    const WORKLOAD_ON_MAIN: usize = 5;
    const RECENT_WORKLOADS: usize = 15;
    let latest = latest_completed_invocations(&mut tx, 40).await?;

    let mut workloads_on_main = Vec::new();
    let mut recent_workloads = Vec::new();
    for WorkloadDescription {
        workload_name,
        commit_sha1,
        commit_message,
        branch,
        reason,
    } in latest
    {
        if workloads_on_main.len() > WORKLOAD_ON_MAIN && recent_workloads.len() > RECENT_WORKLOADS {
            break;
        }
        match branch {
            Some(branch) if branch == "main" => {
                if workloads_on_main.len() > WORKLOAD_ON_MAIN {
                    continue;
                }
                let baseline_commit = latest_commits_for_branch(
                    &mut tx,
                    "main",
                    1,
                    Some(&commit_sha1),
                    &workload_name,
                )
                .await?;
                let target_data = data_for_commit(&mut tx, &commit_sha1, &workload_name).await?;

                let (summary, plot) = if let Some(baseline_commit) = baseline_commit.first() {
                    let baseline_data =
                        data_for_commit(&mut tx, &baseline_commit.sha1, &workload_name).await?;
                    let comparison = compare_commits(target_data.clone(), &baseline_data);

                    (
                        comparison_summary(comparison),
                        summary_gauge(Some(baseline_data), target_data),
                    )
                } else {
                    (
                        WorkloadSummaryTemplate::default(),
                        summary_gauge(None, target_data),
                    )
                };
                workloads_on_main.push(WorkloadTemplate {
                    name: workload_name,
                    target_branch: Some(branch),
                    commit_sha1,
                    commit_message,
                    compared_commit_sha1: baseline_commit.first().map(|commit| commit.sha1.clone()),
                    summary,
                    reason,
                    plot,
                });
                continue;
            }
            branch => {
                if recent_workloads.len() > RECENT_WORKLOADS {
                    continue;
                }
                // FIXME: determine baseline_branch with heuristics:
                // 1. compare with main
                // 2. compare with target_branch
                // 3. compare with previous commits (current implementation)
                let baseline_commit =
                    latest_commits_for_worfklow(&mut tx, 1, &commit_sha1, &workload_name).await?;
                let target_data = data_for_commit(&mut tx, &commit_sha1, &workload_name).await?;

                let (summary, plot) = if let Some(baseline_commit) = baseline_commit.first() {
                    let baseline_data =
                        data_for_commit(&mut tx, &baseline_commit.sha1, &workload_name).await?;
                    let comparison = compare_commits(target_data.clone(), &baseline_data);
                    (
                        comparison_summary(comparison),
                        summary_gauge(Some(baseline_data), target_data),
                    )
                } else {
                    (Default::default(), summary_gauge(None, target_data))
                };
                recent_workloads.push(WorkloadTemplate {
                    name: workload_name,
                    commit_sha1,
                    commit_message,
                    compared_commit_sha1: baseline_commit.first().map(|commit| commit.sha1.clone()),
                    summary,
                    reason,
                    target_branch: branch,
                    plot,
                })
            }
        }
    }

    let latest_invocation_progress = latest_invocation_progress(&mut tx, 8).await?;

    Ok(HomeTemplate {
        latest_invocation_progress,
        workloads_on_main,
        recent_workloads,
    })
}

fn comparison_summary(comparison: BTreeMap<String, Change>) -> WorkloadSummaryTemplate {
    let mut improvements = 0;
    let mut regressions = 0;
    let mut unchanged = 0;
    let mut unstable = 0;

    for (_, change) in comparison {
        match change {
            Change::New { .. } => (),
            Change::Lost { .. } => (),
            Change::Improvement { .. } => {
                improvements += 1;
            }
            Change::Regression { .. } => {
                regressions += 1;
            }
            Change::Stable { .. } => {
                unchanged += 1;
            }
            Change::Unstable { .. } => {
                unstable += 1;
            }
        }
    }

    WorkloadSummaryTemplate {
        improvements,
        regressions,
        unchanged,
        unstable,
    }
}

async fn latest_invocation_progress(tx: &mut Tx, count: u32) -> Result<Vec<Invocation>> {
    let rows = sqlx::query(r#"SELECT uuid, commit_sha1, machine_hostname, reason, tag, branch, status, failure_reason, max_workloads FROM invocations
    ORDER BY invocations.uuid DESC
    LIMIT ?"#)
    .bind(count)
    .fetch_all(&mut *tx).await.context("fetching invocation progress").with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut invocation_status_progress = Vec::with_capacity(rows.len());
    for row in rows {
        let uuid: String = row.get(0);
        let commit_sha1: String = row.get(1);
        let machine_hostname: String = row.get(2);
        let reason: Option<String> = row.get(3);
        let tag: Option<String> = row.get(4);
        let branch: Option<String> = row.get(5);
        let status: String = row.get(6);
        let failure_reason: Option<String> = row.get(7);
        let max_workloads: u32 = row.get(8);

        let status = InvocationStatus::from_db_status(&status, failure_reason)
            .context("incorrect status in DB")
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        let progress = InvocationProgress::from_db(tx, &uuid, &status, max_workloads)
            .await
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        let uuid = Uuid::from_str(&uuid).expect("Could not deserialize as a UUID");

        invocation_status_progress.push(Invocation {
            uuid,
            commit_sha1,
            machine_hostname,
            reason,
            branch,
            tag,
            status,
            progress,
        });
    }

    Ok(invocation_status_progress)
}

async fn latest_completed_invocations(tx: &mut Tx, count: u32) -> Result<Vec<WorkloadDescription>> {
    sqlx::query(
        r#"SELECT workloads.name, commits.sha1, commits.message, invocations.reason, invocations.branch FROM invocations
    INNER JOIN workloads ON invocations.uuid = workloads.invocation_uuid
    INNER JOIN commits ON invocations.commit_sha1 = commits.sha1
    WHERE invocations.status = ?
    ORDER BY invocations.uuid DESC
    LIMIT ?"#,
    )
    .bind(InvocationStatus::Completed.to_db_status())
    .bind(count)
    .map(|row: SqliteRow| WorkloadDescription {
        workload_name: row.get(0),
        commit_sha1: row.get(1),
        commit_message: row.get(2),
        branch: row.get(4),
        reason: row.get(3),
    })
    .fetch_all(tx)
    .await
    .context("fetching latest invocations")
    .with_code(StatusCode::INTERNAL_SERVER_ERROR)
}

struct WorkloadDescription {
    workload_name: String,
    commit_sha1: String,
    commit_message: String,
    branch: Option<String>,
    reason: Option<String>,
}

#[derive(Template)]
#[template(path = "view_spans.html")]
struct ViewSpansTemplate {
    workload_name: String,
    target_commit_sha1: String,
    target_branch: Option<String>,
    baseline_branch: Option<String>,
    target_tag: Option<String>,
    baseline_tag: Option<String>,
    span_stats: SpanStats,
}

impl ViewSpansTemplate {
    pub fn view_span_url(&self, span: impl Display) -> String {
        let mut url = format!(
            "view_span?span={span}&workload_name={workload_name}&target={target}",
            workload_name = self.workload_name,
            target = self.target_commit_sha1
        );
        if let Some(baseline_commit) = self.span_stats.baseline_commit() {
            url.push_str(&format!("&baseline={baseline_commit}"));
        }
        if let Some(baseline_branch) = &self.baseline_branch {
            url.push_str(&format!("&baseline_branch={baseline_branch}"));
        }
        if let Some(target_branch) = &self.target_branch {
            url.push_str(&format!("&target_branch={target_branch}"));
        }
        url
    }
}

struct SpanComparison {
    baseline_commit_sha1: Rc<String>,
    total_time: Option<SpanChange>,
    improvements: Vec<SpanChange>,
    regressions: Vec<SpanChange>,
    stables: Vec<SpanChange>,
    unstables: Vec<SpanChange>,
    news: Vec<SpanChange>,
    olds: Vec<SpanChange>,
}

enum SpanStats {
    NoComparison(BTreeMap<String, Stats>),
    Comparison(Box<SpanComparison>),
}

impl SpanStats {
    pub fn baseline_commit(&self) -> Option<&str> {
        match self {
            SpanStats::NoComparison(_) => None,
            SpanStats::Comparison(comparison) => Some(comparison.baseline_commit_sha1.as_str()),
        }
    }
}

struct SpanChange {
    span: String,
    change: Change,
}

#[derive(Deserialize)]
struct ViewSpansQuery {
    workload_name: String,
    // target
    commit_sha1: Option<String>,
    #[serde(default)]
    target_branch: Option<String>,
    #[serde(default)]
    target_tag: Option<String>,

    // baseline
    #[serde(default)]
    baseline_commit: Option<String>,
    #[serde(default)]
    baseline_tag: Option<String>,
    #[serde(default)]
    baseline_branch: Option<String>,
}

async fn view_spans(
    mut tx: Tx,
    Query(ViewSpansQuery {
        workload_name,
        commit_sha1,
        target_branch,
        target_tag,
        baseline_commit,
        baseline_tag,
        baseline_branch,
    }): Query<ViewSpansQuery>,
) -> Result<ViewSpansTemplate> {
    // determine target commit_sha1
    let commit_sha1 = 'commit: {
        if let Some(commit_sha1) = commit_sha1 {
            break 'commit commit_sha1;
        }
        if let Some(tag) = target_tag.as_deref() {
            let commit = commit_for_tag(&mut tx, tag, &workload_name).await?;
            if let Some(commit) = commit {
                break 'commit commit.sha1;
            } else {
                return Err(anyhow!("Could not find commit for tag {}", tag))
                    .with_code(StatusCode::NOT_FOUND);
            }
        }
        if let Some(branch) = target_branch.as_deref() {
            let mut latest_commits =
                latest_commits_for_branch(&mut tx, branch, 1, None, &workload_name).await?;
            if let Some(commit) = latest_commits.pop() {
                break 'commit commit.sha1;
            } else {
                return Err(anyhow!("Could not find commmit on branch {}", branch))
                    .with_code(StatusCode::NOT_FOUND);
            }
        }
        return Err(anyhow!(
            "Specify one of commit_sha1, target_branch or target_tag"
        ))
        .with_code(StatusCode::BAD_REQUEST);
    };

    let baseline_commit = 'commit: {
        if let Some(baseline_commit) = baseline_commit {
            break 'commit Some(baseline_commit);
        }
        if let Some(baseline_tag) = baseline_tag.as_deref() {
            if let Some(commit) = commit_for_tag(&mut tx, baseline_tag, &workload_name).await? {
                break 'commit Some(commit.sha1);
            }
        }
        if let Some(baseline_branch) = baseline_branch.as_deref() {
            let mut latest_commits = latest_commits_for_branch(
                &mut tx,
                baseline_branch,
                1,
                Some(&commit_sha1),
                &workload_name,
            )
            .await?;
            if let Some(commit) = latest_commits.pop() {
                break 'commit Some(commit.sha1);
            }
        }
        let mut latest_commits =
            latest_commits_for_worfklow(&mut tx, 1, &commit_sha1, &workload_name).await?;
        latest_commits.pop().map(|commit| commit.sha1)
    };

    let commit_data = data_for_commit(&mut tx, &commit_sha1, &workload_name).await?;

    if let Some(baseline_commit) = baseline_commit {
        let baseline_data = data_for_commit(&mut tx, &baseline_commit, &workload_name).await?;
        let report = compare_commits(commit_data, &baseline_data);

        let mut total_time = None;
        let mut improvements = Vec::new();
        let mut regressions = Vec::new();
        let mut stables = Vec::new();
        let mut unstables = Vec::new();
        let mut news = Vec::new();
        let mut olds = Vec::new();

        let baseline_sha1 = Rc::new(baseline_commit.clone());

        for (span, change) in report.into_iter() {
            if span == "::meta::total" {
                total_time = Some(SpanChange { span, change });
                continue;
            }
            let target = match &change {
                Change::New { .. } => &mut news,
                Change::Lost { .. } => &mut olds,
                Change::Improvement { .. } => &mut improvements,
                Change::Regression { .. } => &mut regressions,
                Change::Stable { .. } => &mut stables,
                Change::Unstable { .. } => &mut unstables,
            };
            target.push(SpanChange { span, change })
        }
        improvements.sort_by_key(|x| x.change.abs_difference());
        regressions.sort_by_key(|x| x.change.abs_difference());
        let span_stats = SpanStats::Comparison(Box::new(SpanComparison {
            baseline_commit_sha1: baseline_sha1,
            total_time,
            improvements,
            regressions,
            stables,
            unstables,
            news,
            olds,
        }));
        Ok(ViewSpansTemplate {
            workload_name,
            target_commit_sha1: commit_sha1,
            span_stats,
            target_branch,
            baseline_branch,
            target_tag,
            baseline_tag,
        })
    } else {
        let span_stats = commit_data
            .into_iter()
            .map(|(k, v)| (k, compute_stats(&v)))
            .collect();
        Ok(ViewSpansTemplate {
            workload_name,
            target_commit_sha1: commit_sha1,
            target_branch,
            baseline_branch,
            target_tag,
            baseline_tag,
            span_stats: SpanStats::NoComparison(span_stats),
        })
    }
}

fn summary_gauge(
    baseline: Option<BTreeMap<String, Vec<CallStats>>>,
    target: BTreeMap<String, Vec<CallStats>>,
) -> String {
    // what should the unit be?
    let target_total_time = if let Some(stats) = target.get("::meta::total") {
        CallStats::avg_time(stats)
    } else {
        None
    };
    let baseline_total_time = baseline.as_ref().and_then(|stats| {
        stats
            .get("::meta::total")
            .map(|stats| CallStats::avg_time(stats))
    });

    let target_total_time = target_total_time.unwrap_or_default();
    let baseline_total_time = baseline_total_time.flatten().unwrap_or_default();

    let max_total_time = target_total_time.max(baseline_total_time);

    let max_total_time = TimeWithUnit::new(max_total_time as f64);
    let target_total_time = TimeWithUnit::with_unit(target_total_time as f64, max_total_time.unit);

    let blue = plotly::color::NamedColor::Blue;

    let mut indicator = json!({
        "type": "indicator",
        "value": target_total_time.as_own_unit(),
        "number": {"suffix": max_total_time.unit.as_str()},
        "gauge": {
            "axis": {"visible": true, "range": [0., max_total_time.nice_axis_end_value()]},
            "bar": {"color": blue },
        },
        "title": { "text": "Total time"},
        "mode": "number+delta+gauge",

    });

    let red = plotly::color::NamedColor::Red;
    let green = plotly::color::NamedColor::Green;

    if baseline.is_some() {
        let baseline_total_time =
            TimeWithUnit::with_unit(baseline_total_time as f64, max_total_time.unit);
        indicator["delta"] = json!({
            "reference" : baseline_total_time.as_own_unit(),
            "suffix": max_total_time.unit.as_str(),
            "increasing": { "color": red },
            "decreasing": { "color": green },
        });

        indicator["gauge"]["threshold"] = json!({
            "value": baseline_total_time.as_own_unit(),
            "line": { "color": red },
        });

        let bar_color = if baseline_total_time.nano < target_total_time.nano {
            red
        } else {
            green
        };

        indicator["gauge"]["bar"]["color"] = json!(bar_color);
    }

    let mut plot = plotly::Plot::new();
    plot.add_trace(PlotValue(indicator).boxed());
    plot.set_layout(
        plotly::Layout::new()
            .width(450)
            .height(250)
            .auto_size(false)
            .margin(
                plotly::layout::Margin::new()
                    .pad(4)
                    .top(20)
                    .bottom(10)
                    .left(50)
                    .right(50),
            ),
    );
    plot.to_inline_html(None)
}

fn compare_commits(
    target_data: BTreeMap<String, Vec<CallStats>>,
    baseline_data: &BTreeMap<String, Vec<CallStats>>,
) -> BTreeMap<String, Change> {
    let mut changes = BTreeMap::new();
    for merged in target_data
        .into_iter()
        .merge_join_by(baseline_data.iter(), |target, baseline| {
            target.0.cmp(baseline.0)
        })
    {
        match merged {
            EitherOrBoth::Both((span, target), (_, baseline)) => {
                let target_stats = compute_stats(&target);
                let baseline_stats = compute_stats(baseline);

                let (target_mean, baseline_mean, baseline_stddev) = if span == "::meta::total" {
                    (
                        target_stats.mean_time,
                        baseline_stats.mean_time,
                        baseline_stats.stddev_time,
                    )
                } else {
                    (
                        target_stats.mean_self_time,
                        baseline_stats.mean_self_time,
                        baseline_stats.stddev_self_time,
                    )
                };

                let difference = target_mean - baseline_mean;
                let proportion = difference.abs() / baseline_mean;

                let stddev_proportion = baseline_stddev / baseline_mean;
                if stddev_proportion > 1.5 {
                    let proportion = proportion * 100.;
                    changes.insert(
                        span,
                        Change::Unstable {
                            target_stats,
                            baseline_stats,
                            difference,
                            proportion,
                        },
                    );
                    continue;
                }

                if proportion * 10. >= stddev_proportion {
                    let proportion = proportion * 100.;

                    if difference < 0. {
                        changes.insert(
                            span,
                            Change::Improvement {
                                target_stats,
                                baseline_stats,
                                difference,
                                proportion,
                            },
                        );
                    } else {
                        changes.insert(
                            span,
                            Change::Regression {
                                target_stats,
                                baseline_stats,
                                difference,
                                proportion,
                            },
                        );
                    }
                } else {
                    let proportion = proportion * 100.;

                    changes.insert(
                        span,
                        Change::Stable {
                            target_stats,
                            baseline_stats,
                            difference,
                            proportion,
                        },
                    );
                }
            }
            EitherOrBoth::Left((span, target)) => {
                let stats = compute_stats(&target);
                changes.insert(span, Change::New { stats });
            }
            EitherOrBoth::Right((span, baseline)) => {
                let stats = compute_stats(baseline);
                changes.insert(span.clone(), Change::Lost { stats });
            }
        }
    }
    changes
}

fn compute_stats(data: &[CallStats]) -> Stats {
    if data.is_empty() {
        return Stats {
            mean_self_time: 0.,
            stddev_self_time: 0.,
            mean_time: 0.,
            stddev_time: 0.,
            mean_call_count: 0.,
            stddev_call_count: 0.,
        };
    }
    let (mean_self_time, stddev_self_time) = mean_stddev(
        data.iter().map(|x| x.self_time as f64),
        data.iter().map(|x| x.self_time as f64),
        data.len() as f64,
    );
    let (mean_time, stddev_time) = mean_stddev(
        data.iter().map(|x| x.time as f64),
        data.iter().map(|x| x.time as f64),
        data.len() as f64,
    );

    let (mean_call_count, stddev_call_count) = mean_stddev(
        data.iter().map(|x| x.call_count as f64),
        data.iter().map(|x| x.call_count as f64),
        data.len() as f64,
    );

    Stats {
        mean_self_time,
        stddev_self_time,
        mean_time,
        stddev_time,
        mean_call_count,
        stddev_call_count,
    }
}

fn mean_stddev(
    mean_values: impl Iterator<Item = f64>,
    variance_values: impl Iterator<Item = f64>,
    len: f64,
) -> (f64, f64) {
    let sum: f64 = mean_values.sum();
    let mean: f64 = sum / len;
    let variance: f64 = if len <= 1. {
        0.0
    } else {
        variance_values
            .map(|x| (x - mean).abs().powi(2))
            .sum::<f64>()
            / (len - 1.)
    };
    let stddev = variance.sqrt();
    (mean, stddev)
}

#[derive(Clone, Copy)]
pub struct Stats {
    mean_self_time: f64,
    stddev_self_time: f64,
    mean_time: f64,
    stddev_time: f64,
    mean_call_count: f64,
    stddev_call_count: f64,
}

#[derive(Clone, Copy)]
pub enum Change {
    New {
        stats: Stats,
    },
    Lost {
        stats: Stats,
    },
    Improvement {
        target_stats: Stats,
        baseline_stats: Stats,
        difference: f64,
        proportion: f64,
    },
    Regression {
        target_stats: Stats,
        baseline_stats: Stats,
        difference: f64,
        proportion: f64,
    },
    Stable {
        target_stats: Stats,
        baseline_stats: Stats,
        difference: f64,
        proportion: f64,
    },
    Unstable {
        target_stats: Stats,
        baseline_stats: Stats,
        difference: f64,
        proportion: f64,
    },
}
impl Change {
    fn abs_difference(&self) -> u64 {
        match self {
            Change::New { .. } => 0,
            Change::Lost { .. } => 0,
            Change::Improvement { difference, .. } => difference.abs() as u64,
            Change::Regression { difference, .. } => difference.abs() as u64,
            Change::Stable { difference, .. } => difference.abs() as u64,
            Change::Unstable { difference, .. } => difference.abs() as u64,
        }
    }
}

async fn data_for_commit(
    tx: &mut Tx,
    commit_sha1: &str,
    workload_name: &str,
) -> Result<BTreeMap<String, Vec<CallStats>>> {
    let mut spans: BTreeMap<String, Vec<CallStats>> = BTreeMap::new();
    sqlx::query(
        r#"SELECT data.span, data.self_time, data.time, data.call_count FROM data
    INNER JOIN runs ON data.run_uuid = runs.uuid
    INNER JOIN workloads ON runs.workload_invocation_uuid = workloads.uuid
    INNER JOIN invocations ON workloads.invocation_uuid = invocations.uuid
    WHERE workloads.name = ?
    AND invocations.commit_sha1 = ?
    AND invocations.status = ?"#,
    )
    .bind(workload_name)
    .bind(commit_sha1)
    .bind(InvocationStatus::Completed.to_db_status())
    .map(|row: SqliteRow| {
        let span: String = row.get(0);
        let self_time: i64 = row.get(1);
        let time: i64 = row.get(2);
        let call_count: i64 = row.get(3);
        spans.entry(span).or_default().push(CallStats {
            self_time: self_time as u64,
            time: time as u64,
            call_count: call_count as u64,
        });
    })
    .fetch_all(&mut *tx)
    .await
    .context("fetching data for commit")
    .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(spans)
}

async fn data_for_span(
    tx: &mut Tx,
    commit_sha1: &str,
    workload_name: &str,
    span: &str,
) -> Result<Vec<CallStats>> {
    sqlx::query(
        r#"SELECT data.self_time, data.time, data.call_count FROM data
    INNER JOIN runs ON data.run_uuid = runs.uuid
    INNER JOIN workloads ON runs.workload_invocation_uuid = workloads.uuid
    INNER JOIN invocations ON workloads.invocation_uuid = invocations.uuid
    WHERE data.span = ?
    AND workloads.name = ?
    AND invocations.commit_sha1 = ?
    AND invocations.status = ?"#,
    )
    .bind(span)
    .bind(workload_name)
    .bind(commit_sha1)
    .bind(InvocationStatus::Completed.to_db_status())
    .map(|row: SqliteRow| {
        let self_time: i64 = row.get(0);
        let time: i64 = row.get(1);
        let call_count: i64 = row.get(2);
        CallStats {
            time: time as u64,
            call_count: call_count as u64,
            self_time: self_time as u64,
        }
    })
    .fetch_all(&mut *tx)
    .await
    .context("fetching data for span")
    .with_code(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn latest_commits_for_worfklow(
    tx: &mut Tx,
    count: u32,
    before: &str,
    workload_name: &str,
) -> Result<Vec<Commit>> {
    let query = sqlx::query(
        r#"SELECT commits.sha1, commits.date, commits.message FROM workloads
            INNER JOIN invocations ON workloads.invocation_uuid = invocations.uuid
            INNER JOIN commits ON commits.sha1 = invocations.commit_sha1
            WHERE workloads.name = ?
            AND commits.sha1 != ?
            AND invocations.status = ?
            ORDER BY commits.date DESC
            LIMIT ?"#,
    )
    .bind(workload_name)
    .bind(before)
    .bind(InvocationStatus::Completed.to_db_status())
    .bind(count);

    let commits = query
        .map(|row: SqliteRow| Commit { sha1: row.get(0) })
        .fetch_all(tx)
        .await
        .context("fetching latest commits")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(commits)
}

async fn commit_for_tag(tx: &mut Tx, tag: &str, workload_name: &str) -> Result<Option<Commit>> {
    let query = sqlx::query(
        r#"SELECT commits.sha1 FROM commits
        INNER JOIN workloads ON invocations.uuid = workloads.invocation_uuid
        INNER JOIN invocations ON commits.sha1 = invocations.commit_sha1
    WHERE workloads.name = ? AND invocations.tag = ? AND invocations.status = ?
    GROUP BY commits.sha1
    ORDER BY commits.date DESC
    LIMIT 1"#,
    )
    .bind(workload_name)
    .bind(tag)
    .bind(InvocationStatus::Completed.to_db_status());

    let commit = query
        .map(|row: SqliteRow| Commit { sha1: row.get(0) })
        .fetch_optional(tx)
        .await
        .context("fetching commit for tag")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(commit)
}

async fn latest_commits_for_branch(
    tx: &mut Tx,
    branch: &str,
    count: u32,
    before: Option<&str>,
    workload_name: &str,
) -> Result<Vec<Commit>> {
    let commit_date: Option<String> = if let Some(before) = before {
        sqlx::query(
            r#"SELECT commits.date FROM commits
        INNER JOIN invocations ON commits.sha1 = invocations.commit_sha1
        WHERE invocations.branch = ?
        AND commits.sha1 = ?
        AND invocations.status = ?"#,
        )
        .bind(branch)
        .bind(before)
        .bind(InvocationStatus::Completed.to_db_status())
        .map(|row: SqliteRow| row.get(0))
        .fetch_optional(&mut *tx)
        .await
        .context("fetching commit date")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        None
    };

    let query = if let Some(commit_date) = commit_date {
        sqlx::query(
            r#"SELECT commits.sha1, commits.date, commits.message FROM invocations
        INNER JOIN workloads ON invocations.uuid = workloads.invocation_uuid
        INNER JOIN commits ON invocations.commit_sha1 = commits.sha1
        WHERE workloads.name = ? AND invocations.branch = ? AND commits.date < ? AND invocations.status = ?
        GROUP BY commits.sha1
        ORDER BY commits.date DESC
        LIMIT ?"#,
        )
        .bind(workload_name)
        .bind(branch)
        .bind(commit_date)
        .bind(InvocationStatus::Completed.to_db_status())
        .bind(count)
    } else {
        sqlx::query(
            r#"SELECT commits.sha1 FROM commits
        INNER JOIN workloads ON invocations.uuid = workloads.invocation_uuid
        INNER JOIN invocations ON commits.sha1 = invocations.commit_sha1
        WHERE workloads.name = ? AND invocations.branch = ? AND invocations.status = ?
        GROUP BY commits.sha1
        ORDER BY commits.date DESC
        LIMIT ?"#,
        )
        .bind(workload_name)
        .bind(branch)
        .bind(InvocationStatus::Completed.to_db_status())
        .bind(count)
    };

    let commits = query
        .map(|row: SqliteRow| Commit { sha1: row.get(0) })
        .fetch_all(tx)
        .await
        .context("fetching latest commits on branch")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(commits)
}

struct Commit {
    sha1: String,
}

async fn view_span(
    mut tx: Tx,
    Query(ViewSpanQuery {
        span,
        workload_name,
        target,
        baseline,
        target_branch,
        baseline_branch,
        target_tag,
        baseline_tag,
    }): Query<ViewSpanQuery>,
) -> Result<ViewSpanTemplate> {
    let target_data = data_for_span(&mut tx, &target, &workload_name, &span).await?;

    let baseline_commits = if let Some(baseline_branch) = baseline_branch.as_deref() {
        latest_commits_for_branch(
            &mut tx,
            baseline_branch,
            4,
            baseline.as_deref(),
            &workload_name,
        )
        .await?
    } else {
        Vec::new()
    };

    let mut baselines = Vec::with_capacity(baseline_commits.len() + 1);

    for baseline_commit in baseline_commits
        .into_iter()
        .map(|commit| commit.sha1)
        .rev()
        .chain(baseline.clone().into_iter())
    {
        let baseline_data = data_for_span(&mut tx, &baseline_commit, &workload_name, &span).await?;
        baselines.push((baseline_commit, baseline_data));
    }

    let plot = comparison_boxplot(&span, (&target, &target_data), baselines.as_slice());

    Ok(ViewSpanTemplate {
        workload_name,
        span,
        target_commit_sha1: target,
        baseline_commit_sha1: baseline,
        baseline_branch,
        target_branch,
        target_tag,
        baseline_tag,
        plot,
    })
}

fn comparison_boxplot(
    span: &str,
    (target, target_stats): (&str, &[CallStats]),
    baseline: &[(String, Vec<CallStats>)],
) -> String {
    // take time of interest depending on span
    let time = if span == "::meta::total" {
        |x: &CallStats| x.time
    } else {
        |x: &CallStats| x.self_time
    };

    // find unit
    let target_max = target_stats.iter().map(time).max().unwrap_or_default();
    let baseline_max = baseline
        .iter()
        .map(|(_, baseline_stats)| baseline_stats.iter().map(time).max().unwrap_or_default())
        .max()
        .unwrap_or_default();
    let max = TimeWithUnit::new(target_max.max(baseline_max) as f64);

    let to_proper_unit =
        |x: &CallStats| TimeWithUnit::with_unit(time(x) as f64, max.unit).as_own_unit();

    let mut plot = plotly::Plot::new();

    let target_plot = plotly::BoxPlot::new(target_stats.iter().map(to_proper_unit).collect())
        .name(format!("This commit: {}", target))
        .box_points(plotly::box_plot::BoxPoints::Outliers);

    for (baseline, baseline_stats) in baseline.iter() {
        let baseline_plot =
            plotly::BoxPlot::new(baseline_stats.iter().map(to_proper_unit).collect())
                .name(baseline)
                .box_points(plotly::box_plot::BoxPoints::Outliers);

        plot.add_trace(baseline_plot);
    }

    plot.add_trace(target_plot);

    let layout = plotly::Layout::new()
        .title(plotly::common::Title::new(span))
        .y_axis(
            plotly::layout::Axis::new()
                .range_mode(plotly::layout::RangeMode::ToZero)
                .show_tick_suffix(plotly::layout::ArrayShow::All)
                .tick_suffix(max.unit.as_str()),
        );

    plot.set_layout(layout);

    plot.to_inline_html(None)
}

#[derive(Template)]
#[template(path = "view_span.html")]
struct ViewSpanTemplate {
    workload_name: String,
    span: String,
    target_commit_sha1: String,
    baseline_commit_sha1: Option<String>,
    target_branch: Option<String>,
    baseline_branch: Option<String>,
    target_tag: Option<String>,
    baseline_tag: Option<String>,
    plot: String,
}

#[derive(Deserialize)]
struct ViewSpanQuery {
    workload_name: String,
    span: String,
    target: String,
    #[serde(default)]
    baseline: Option<String>,
    #[serde(default)]
    target_branch: Option<String>,
    #[serde(default)]
    baseline_branch: Option<String>,
    #[serde(default)]
    target_tag: Option<String>,
    #[serde(default)]
    baseline_tag: Option<String>,
}
