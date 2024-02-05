use std::{collections::BTreeMap, str::FromStr};

use anyhow::{anyhow, bail, Context};
use axum::{
    extract::{FromRequestParts, Path, Query},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use axum_extra::response::Css;
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
    ConnectOptions, Executor, Row, SqlitePool,
};
use time::format_description::well_known::Iso8601;
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, Layer};
use uuid::Uuid;

type Tx = axum_sqlx_tx::Tx<sqlx::Sqlite>;

type Result<T> = std::result::Result<T, ResponseError>;

mod plot;
mod view;

pub fn default_log_filter() -> String {
    "info".into()
}

pub fn default_db_path() -> String {
    "benchboard.sqlite".into()
}

pub fn default_http_addr() -> String {
    "127.0.0.1:9001".into()
}

/// Records workload invocations into a database and expose a dashboard of result
#[derive(Parser)]
struct Args {
    /// Path to sqlite database
    #[arg(long, default_value_t = default_db_path())]
    db_path: String,

    /// Log directives
    #[arg(short, long, default_value_t = default_log_filter())]
    log_filter: String,

    /// Authentication key for API routes
    #[arg(long)]
    api_key: Option<String>,

    /// Sets the HTTP address and port that the server will listen to.
    #[arg(long, default_value_t = default_http_addr())]
    http_addr: String,
}

pub struct InvocationTimeouts {
    timeout_receiver: tokio::sync::mpsc::Receiver<(Uuid, time::Duration)>,
    pool: sqlx::Pool<sqlx::Sqlite>,
}

#[derive(Clone)]
pub struct TimeoutSender(pub tokio::sync::mpsc::Sender<(Uuid, time::Duration)>);

impl InvocationTimeouts {
    pub fn new(
        pool: sqlx::Pool<sqlx::Sqlite>,
    ) -> (Self, tokio::sync::mpsc::Sender<(Uuid, time::Duration)>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        (
            Self {
                timeout_receiver: receiver,
                pool,
            },
            sender,
        )
    }

    pub async fn run(mut self) {
        while let Some((uuid, duration)) = self.timeout_receiver.recv().await {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                let deadline = time::Instant::now() + duration;
                tokio::time::sleep_until(deadline.into_inner().into()).await;
                match Self::on_deadline(pool, uuid).await {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::error!(
                            "failure while executing `InvocationTimeout::on_deadline`: {}",
                            error
                        )
                    }
                }
            });
        }
    }

    async fn on_deadline(pool: sqlx::Pool<sqlx::Sqlite>, uuid: Uuid) -> anyhow::Result<()> {
        let mut tx = pool.acquire().await.context("acquiring tx")?;

        let query = sqlx::query("SELECT status, reason FROM invocations WHERE uuid = ?")
            .bind(uuid.to_string());

        let row = tx
            .fetch_one(query)
            .await
            .with_context(|| format!("fetching invocation with uuid {}", uuid))?;

        let status: String = row.get(0);
        let reason: Option<String> = row.get(1);

        let status = InvocationStatus::from_db_status(&status, reason)
            .context("could not deserialize invocation")
            .with_context(|| format!("getting status for invocation with uuid {}", uuid))?;

        if status.is_running() {
            let update = sqlx::query(r#"UPDATE invocations SET status = ? WHERE uuid = ?"#)
                .bind(InvocationStatus::Timeout.to_db_status())
                .bind(uuid.to_string());
            tx.execute(update).await.context("update failed")?;
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct OuterAppState {
    pub sql_state: axum_sqlx_tx::State<sqlx::Sqlite>,
    pub timeout_sender: TimeoutSender,
}

pub struct AppState {
    pub tx: Tx,
    pub timeout_sender: TimeoutSender,
}

impl FromRequestParts<OuterAppState> for AppState {
    type Rejection = axum_sqlx_tx::Error;

    fn from_request_parts<'req, 'state, 'ctx>(
        parts: &'req mut Parts,
        state: &'state OuterAppState,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Self, Self::Rejection>>
                + Send
                + 'ctx,
        >,
    >
    where
        Self: 'ctx,
        'req: 'ctx,
        'state: 'ctx,
    {
        Box::pin(async {
            let tx = Tx::from_request_parts(parts, &state.sql_state).await?;
            let timeout_sender = state.timeout_sender.clone();
            Ok(AppState { tx, timeout_sender })
        })
    }
}

impl FromRequestParts<OuterAppState> for Tx {
    type Rejection = axum_sqlx_tx::Error;

    fn from_request_parts<'req, 'state, 'ctx>(
        parts: &'req mut Parts,
        state: &'state OuterAppState,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Self, Self::Rejection>>
                + Send
                + 'ctx,
        >,
    >
    where
        Self: 'ctx,
        'req: 'ctx,
        'state: 'ctx,
    {
        Box::pin(async { Tx::from_request_parts(parts, &state.sql_state).await })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter: tracing_subscriber::filter::Targets =
        args.log_filter.parse().context("invalid --log-filter")?;

    // logging
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_span_events(FmtSpan::ENTER)
            .with_filter(filter),
    );
    tracing::subscriber::set_global_default(subscriber).context("could not setup logging")?;

    let pool = new_pool(&args.db_path).await?;

    let (invocation_timeouts, timeout_sender) = InvocationTimeouts::new(pool.clone());

    tracing::info!(db_path = args.db_path, "Loaded database");

    let (state, layer) = Tx::setup(pool.clone());

    // this must be done AFTER creating the tables
    reap_zombie_invocations(pool.clone()).await?;

    let state = OuterAppState {
        sql_state: state,
        timeout_sender: TimeoutSender(timeout_sender),
    };

    let api_routes: Router<OuterAppState> = Router::new();

    let api_routes: Router<()> = api_routes
        .route("/invocation", put(new_invocation))
        .route("/machine", put(new_machine))
        .route("/machine", get(list_machines))
        .route("/machine/:machine_id", get(get_machine))
        .route("/invocation", get(list_invocations))
        .route("/cancel-invocation", post(cancel_invocation))
        .route("/invocation/:invocation_uuid", get(get_invocation))
        .route("/workload", put(new_workload))
        .route("/workload", get(list_workloads))
        .route("/run", put(new_run))
        .layer(layer.clone())
        .with_state(state.clone());

    let api_routes = match args.api_key {
        Some(api_key) => {
            let auth_layer = ValidateRequestHeaderLayer::bearer(&api_key);
            api_routes.layer(auth_layer)
        }
        None => {
            tracing::warn!(
                "⚠️  Started without `--api-key`: API routes will allow unauthenticated access ⚠️"
            );
            api_routes
        }
    };

    let view_routes = view::router(state.clone(), layer.clone());

    let app = Router::new()
        .nest("/api/v1", api_routes)
        .nest("", view_routes)
        .route_service("/stylesheet.css", get(get_style))
        .route_service("/custom.css", get(get_custom_style))
        .route_service("/scouter.png", get(get_icon));

    tokio::spawn(async move {
        invocation_timeouts.run().await;
    });

    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .with_context(|| format!("Could not bind to {}", args.http_addr))?;
    tracing::info!(http_addr = &args.http_addr, "Server listening");
    axum::serve(listener, app).await.unwrap();
    Ok(())
}

async fn reap_zombie_invocations(pool: sqlx::Pool<sqlx::Sqlite>) -> anyhow::Result<()> {
    let mut tx = pool.acquire().await.context("could not acquire tx")?;
    let query = sqlx::query(r#"SELECT uuid FROM invocations WHERE status = ?"#)
        .bind(InvocationStatus::Running.to_db_status());

    use futures_util::StreamExt as _;

    let mut zombie_uuids = Vec::new();

    {
        let mut rows = tx.fetch(query);

        while let Some(next) = rows.next().await {
            let row = next
                .context("fetching next row")
                .context("reaping zombie invocations")?;
            let uuid: String = row.get(0);
            zombie_uuids.push(uuid);
        }
    }

    let zombie_status = InvocationStatus::Failed("reaped zombie".to_string());

    for zombie_uuid in zombie_uuids.iter() {
        let update_query =
            sqlx::query(r#"UPDATE invocations SET status = ?, failure_reason = ? WHERE uuid = ?"#)
                .bind(zombie_status.to_db_status())
                .bind(zombie_status.reason())
                .bind(zombie_uuid);
        tx.execute(update_query).await.with_context(|| {
            format!("while reaping zombie invocation with uuid={}", zombie_uuid)
        })?;
        tracing::warn!(uuid = zombie_uuid, "Reaping zombie invocation");
    }
    Ok(())
}

async fn new_pool(db_path: &str) -> anyhow::Result<SqlitePool> {
    let db_options = SqliteConnectOptions::from_str(db_path)?
        .create_if_missing(true)
        .disable_statement_logging()
        .busy_timeout(std::time::Duration::from_secs(60))
        .to_owned();

    let pool = SqlitePoolOptions::new().connect_with(db_options).await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS machines (
            hostname TEXT PRIMARY KEY
        );",
    )
    .execute(&pool)
    .await
    .context("creating machines table")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS commits (
            sha1 TEXT PRIMARY KEY,
            date TEXT NOT NULL,
            message TEXT NOT NULL
        );",
    )
    .execute(&pool)
    .await
    .context("creating commits table")?;

    // "invocations" contains finished invocations
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS invocations (
            uuid TEXT PRIMARY KEY,
            commit_sha1 TEXT NOT NULL,
            machine_hostname TEXT NOT NULL,
            reason TEXT,
            tag TEXT,
            branch TEXT,
            status TEXT NOT NULL,
            failure_reason TEXT,
            max_workloads INTEGER NOT NULL,
            FOREIGN KEY (commit_sha1) REFERENCES commits(sha1),
            FOREIGN KEY (machine_hostname) REFERENCES machines(hostname)
        );",
    )
    .execute(&pool)
    .await
    .context("creating invocations table")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS workloads (
            uuid TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            invocation_uuid TEXT NOT NULL,
            max_runs INTEGER NOT NULL,
            FOREIGN KEY (invocation_uuid) REFERENCES invocations(uuid)
        );",
    )
    .execute(&pool)
    .await
    .context("creating workloads table")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS runs (
            uuid TEXT PRIMARY KEY,
            workload_invocation_uuid TEXT NOT NULL,
            FOREIGN KEY (workload_invocation_uuid) REFERENCES workloads(uuid)
        );",
    )
    .execute(&pool)
    .await
    .context("creating runs table")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS data (
            span TEXT NOT NULL,
            self_time INTEGER NOT NULL,
            time INTEGER NOT NULL,
            call_count INTEGER NOT NULL,
            run_uuid TEXT NOT NULL,
            FOREIGN KEY (run_uuid) REFERENCES runs(uuid)
        );",
    )
    .execute(&pool)
    .await
    .context("creating data table")?;

    Ok(pool)
}

async fn new_machine(mut tx: Tx, Json(machine): Json<NewMachine>) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO machines (hostname) VALUES (?)")
        .bind(machine.hostname)
        .execute(&mut tx)
        .await
        .context("creating a new machine")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(())
}

async fn list_machines(
    mut tx: Tx,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PaginatedResults<Machine>>> {
    let limit = pagination.limit.unwrap_or(20);
    let offset = pagination.offset.unwrap_or(0);

    let query = sqlx::query("SELECT * from machines LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset);
    let machines = query
        .map(|row: SqliteRow| {
            let hostname: String = row.get(0);
            Machine { hostname }
        })
        .fetch_all(&mut tx)
        .await
        .context("listing machines")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let machine_count: i64 = sqlx::query("SELECT COUNT(hostname) from machines")
        .fetch_one(&mut tx)
        .await
        .context("fetching machine count")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?
        .get(0);

    Ok(Json(PaginatedResults {
        results: machines,
        total_count: machine_count as u64,
    }))
}

async fn get_machine(mut tx: Tx, Path(machine_id): Path<String>) -> Result<Json<Machine>> {
    let row = sqlx::query("SELECT * FROM machines where hostname = ?")
        .bind(&machine_id)
        .fetch_optional(&mut tx)
        .await;
    match row {
        Ok(Some(row)) => Ok(Json(Machine {
            hostname: row.get(0),
        })),
        Ok(None) => Err(ResponseError {
            error: anyhow!("machine '{machine_id}' not found"),
            code: StatusCode::NOT_FOUND,
        }),
        Err(error) => Err(ResponseError {
            error: anyhow::Error::new(error).context("while fetching machine"),
            code: StatusCode::INTERNAL_SERVER_ERROR,
        }),
    }
}

async fn new_invocation(
    AppState {
        mut tx,
        timeout_sender: TimeoutSender(timeout_sender),
    }: AppState,
    Json(invocation): Json<NewInvocation>,
) -> Result<Json<Uuid>> {
    if invocation.max_workloads == 0 {
        return Err(anyhow!("max_workloads must be positive")).with_code(StatusCode::BAD_REQUEST);
    }

    let uuid = Uuid::now_v7();
    insert_commit(&mut tx, &invocation.commit).await?;
    sqlx::query(
        r#"INSERT INTO
        invocations (uuid, commit_sha1, branch, tag, status, max_workloads, machine_hostname, reason)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(uuid.to_string())
    .bind(invocation.commit.sha1)
    .bind(invocation.commit.branch)
    .bind(invocation.commit.tag)
    .bind(InvocationStatus::Running.to_db_status())
    .bind(invocation.max_workloads)
    .bind(invocation.machine_hostname)
    .bind(invocation.reason)
    .execute(&mut tx)
    .await
    .context("inserting new invocation")
    .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let duration = invocation.timeout.unwrap_or(time::Duration::hours(3));

    timeout_sender
        .send((uuid, duration))
        .await
        .context("could not send timeout")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(uuid))
}

#[derive(Serialize)]
pub enum InvocationStatus {
    Running,
    Failed(String),
    Completed,
    Canceled,
    Timeout,
}

impl InvocationStatus {
    const RUNNING: &'static str = "running";
    const FAILED: &'static str = "failed";
    const COMPLETED: &'static str = "completed";
    const CANCELED: &'static str = "canceled";
    const TIMEOUT: &'static str = "timeout";

    pub fn to_db_status(&self) -> &'static str {
        match self {
            InvocationStatus::Running => Self::RUNNING,
            InvocationStatus::Failed(_) => Self::FAILED,
            InvocationStatus::Completed => Self::COMPLETED,
            InvocationStatus::Canceled => Self::CANCELED,
            InvocationStatus::Timeout => Self::TIMEOUT,
        }
    }

    pub fn from_db_status(status: &str, reason: Option<String>) -> anyhow::Result<Self> {
        Ok(match status {
            Self::RUNNING => InvocationStatus::Running,
            Self::FAILED => InvocationStatus::Failed(reason.unwrap_or_default()),
            Self::COMPLETED => InvocationStatus::Completed,
            Self::CANCELED => InvocationStatus::Canceled,
            Self::TIMEOUT => InvocationStatus::Timeout,
            _unknown => bail!("received unknown status from DB: {}", status),
        })
    }

    pub fn reason(&self) -> Option<&str> {
        let InvocationStatus::Failed(reason) = self else {
            return None;
        };
        Some(reason)
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, InvocationStatus::Completed)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, InvocationStatus::Running)
    }

    async fn get(tx: &mut Tx, invocation_uuid: &str) -> anyhow::Result<Option<Self>> {
        Ok(
            match sqlx::query("SELECT status, failure_reason FROM invocations WHERE uuid = ?")
                .bind(invocation_uuid)
                .fetch_optional(tx)
                .await
                .context("fetching invocation status")?
            {
                Some(row) => {
                    let this = Self::from_db_status(row.get(0), row.get(1))
                        .context("deserializing invocation status")?;
                    Some(this)
                }
                None => None,
            },
        )
    }
}

async fn cancel_invocation(
    mut tx: Tx,
    Json(InvocationCancelRequest {
        invocation_uuid,
        failure_reason,
    }): Json<InvocationCancelRequest>,
) -> Result<()> {
    let invocation_uuid = invocation_uuid.to_string();
    let invocation_status = match failure_reason {
        Some(failure_reason) => InvocationStatus::Failed(failure_reason),
        None => InvocationStatus::Canceled,
    };

    let current_invocation_status = InvocationStatus::get(&mut tx, &invocation_uuid)
        .await
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(current_invocation_status) = current_invocation_status else {
        return Err(anyhow!(
            "Invocation with uuid {} does not exist",
            invocation_uuid
        ))
        .with_code(StatusCode::BAD_REQUEST);
    };

    if !current_invocation_status.is_running() {
        return Ok(());
    }

    let result =
        sqlx::query(r#"UPDATE invocations SET status = ?, failure_reason = ? WHERE uuid = ?"#)
            .bind(invocation_status.to_db_status())
            .bind(invocation_status.reason())
            .bind(&invocation_uuid)
            .execute(&mut tx)
            .await
            .with_context(|| {
                format!(
                    "Marking invocations {} as failed or canceled",
                    invocation_uuid
                )
            })
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "Could not find invocation with uuid {}",
            invocation_uuid
        ))
        .with_code(StatusCode::BAD_REQUEST);
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct InvocationCancelRequest {
    pub invocation_uuid: Uuid,
    #[serde(default)]
    pub failure_reason: Option<String>,
}

async fn insert_commit(tx: &mut Tx, commit: &NewCommit) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO commits (sha1, date, message) VALUES (?, ?, ?)")
        .bind(&commit.sha1)
        .bind(&commit.commit_date.format(&Iso8601::DEFAULT).unwrap())
        .bind(&commit.message)
        .execute(&mut *tx)
        .await
        .context("inserting new commit")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(())
}

async fn new_workload(mut tx: Tx, Json(workload): Json<NewWorkload>) -> Result<Json<Uuid>> {
    let invocation_uuid = workload.invocation_uuid.to_string();
    check_running(&mut tx, &invocation_uuid).await?;

    let uuid = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workloads (uuid, name, max_runs, invocation_uuid) VALUES (?, ?, ?, ?)",
    )
    .bind(uuid.to_string())
    .bind(workload.name)
    .bind(workload.max_runs)
    .bind(invocation_uuid)
    .execute(&mut tx)
    .await
    .context("inserting workload invocation")
    .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(uuid))
}

async fn new_run(mut tx: Tx, Json(run): Json<NewRun>) -> Result<Json<Uuid>> {
    let workload_uuid = run.workload_uuid.to_string();

    let row = sqlx::query("SELECT max_runs, invocation_uuid FROM workloads WHERE uuid = ?")
        .bind(&workload_uuid)
        .fetch_one(&mut tx)
        .await
        .context("fetching max number of runs")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let max_number_of_runs: u32 = row.get(0);
    let invocation_uuid: String = row.get(1);

    check_running(&mut tx, &invocation_uuid).await?;

    let uuid = Uuid::now_v7();

    let run_uuid = uuid.to_string();

    sqlx::query("INSERT INTO runs (uuid, workload_invocation_uuid) VALUES (?, ?)")
        .bind(&run_uuid)
        .bind(&workload_uuid)
        .execute(&mut tx)
        .await
        .context("inserting run")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    for (span, point) in run.data {
        sqlx::query(
            "INSERT INTO data (span, run_uuid, self_time, time, call_count) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(span)
        .bind(&run_uuid)
        .bind(
            i64::try_from(point.self_time)
                .with_context(|| {
                    format!(
                        "'self_time' ({}ns) bigger than the allowed maximum ({}ns)",
                        point.self_time,
                        i64::MAX
                    )
                })
                .with_code(StatusCode::BAD_REQUEST)?,
        )
        .bind(
            i64::try_from(point.time)
                .with_context(|| {
                    format!(
                        "'time' ({}ns) bigger than the allowed maximum ({}ns)",
                        point.time,
                        i64::MAX
                    )
                })
                .with_code(StatusCode::BAD_REQUEST)?,
        )
        .bind(
            i64::try_from(point.call_count)
                .with_context(|| {
                    format!(
                        "'call_count' ({}) bigger than the allowed maximum ({})",
                        point.call_count,
                        i64::MAX
                    )
                })
                .with_code(StatusCode::BAD_REQUEST)?,
        )
        .execute(&mut tx)
        .await
        .context("inserting data point")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    // check number of runs for this workload
    let number_of_runs = number_of_runs(&workload_uuid, &mut tx)
        .await
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    if number_of_runs == max_number_of_runs {
        tracing::info!(invocation_uuid, workload_uuid, "Workload completed");

        // workload over
        // check number of workloads for this invocation
        let number_of_workloads = number_of_workloads(&invocation_uuid, &mut tx)
            .await
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        let max_number_of_workloads: u32 =
            sqlx::query("SELECT max_workloads FROM invocations WHERE uuid = ?")
                .bind(&invocation_uuid)
                .fetch_one(&mut tx)
                .await
                .context("fetching max number of invocations")
                .with_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .get(0);

        if number_of_workloads == max_number_of_workloads {
            // if the last workload is over, mark this invocation as completed
            sqlx::query("UPDATE invocations SET status = ? WHERE uuid = ?")
                .bind(InvocationStatus::Completed.to_db_status())
                .bind(&invocation_uuid)
                .execute(&mut tx)
                .await
                .context("marking invocation as completed")
                .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::info!(invocation_uuid, "Invocation completed");
        }
    }

    Ok(Json(uuid))
}

async fn check_running(tx: &mut Tx, invocation_uuid: &str) -> Result<()> {
    let invocation_status = InvocationStatus::get(tx, invocation_uuid)
        .await
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(invocation_status) = invocation_status else {
        return Err(ResponseError {
            error: anyhow!("invocation '{invocation_uuid}' not found"),
            code: StatusCode::NOT_FOUND,
        });
    };
    if !invocation_status.is_running() {
        return Err(ResponseError {
            error: anyhow!("invocation '{invocation_uuid}' is not running"),
            code: StatusCode::BAD_REQUEST,
        });
    }

    Ok(())
}

pub async fn number_of_workloads(invocation_uuid: &str, tx: &mut Tx) -> anyhow::Result<u32> {
    Ok(
        sqlx::query("SELECT COUNT(uuid) FROM workloads WHERE invocation_uuid = ?")
            .bind(invocation_uuid)
            .fetch_one(tx)
            .await
            .context("fetching number of workloads")?
            .get(0),
    )
}

pub async fn latest_workload(
    invocation_uuid: &str,
    tx: &mut Tx,
) -> anyhow::Result<Option<(String, String, u32)>> {
    Ok(sqlx::query(
        "SELECT uuid, name, max_runs FROM workloads WHERE invocation_uuid = ? ORDER BY uuid DESC LIMIT ?",
    )
    .bind(invocation_uuid)
    .bind(1)
    .fetch_optional(tx)
    .await
    .context("fetching latest workload")?
    .map(|row| (row.get(0), row.get(1), row.get(2))))
}

pub async fn number_of_runs(workload_uuid: &str, tx: &mut Tx) -> anyhow::Result<u32> {
    Ok(
        sqlx::query("SELECT COUNT(uuid) FROM runs WHERE workload_invocation_uuid = ?")
            .bind(workload_uuid)
            .fetch_one(tx)
            .await
            .context("fetching number of runs")?
            .get(0),
    )
}

async fn list_invocations(
    mut tx: Tx,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PaginatedResults<Invocation>>> {
    let limit = pagination.limit.unwrap_or(20);
    let offset = pagination.offset.unwrap_or(0);

    let query = sqlx::query(
        r#"SELECT
    uuid,
    commit_sha1,
    machine_hostname,
    reason,
    tag,
    branch,
    status,
    failure_reason,
    max_workloads
    FROM invocations
    LIMIT ?
    OFFSET ?"#,
    )
    .bind(limit)
    .bind(offset);

    let rows = query
        .fetch_all(&mut tx)
        .await
        .context("listing invocations")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut invocations = Vec::with_capacity(rows.len());
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
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        let progress = InvocationProgress::from_db(&mut tx, &uuid, &status, max_workloads)
            .await
            .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        invocations.push(Invocation {
            uuid: Uuid::from_str(&uuid).expect("could not deserialize UUID"),
            commit_sha1,
            machine_hostname,
            reason,
            branch,
            tag,
            status,
            progress,
        })
    }

    let invocation_count: i64 = sqlx::query("SELECT COUNT(uuid) from invocations")
        .fetch_one(&mut tx)
        .await
        .context("fetching invocation count")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?
        .get(0);

    Ok(Json(PaginatedResults {
        results: invocations,
        total_count: invocation_count as u64,
    }))
}

async fn get_invocation(mut tx: Tx, Path(invocation_uuid): Path<Uuid>) -> Result<Json<Invocation>> {
    let uuid_str = invocation_uuid.to_string();
    let row =
        sqlx::query("SELECT commit_sha1, machine_hostname, reason, branch, tag, status, failure_reason, max_workloads
        FROM invocation
        WHERE uuid = ?")
            .bind(&uuid_str)
            .fetch_optional(&mut tx)
            .await;
    match row {
        Ok(Some(row)) => {
            let commit_sha1: String = row.get(0);
            let machine_hostname: String = row.get(1);
            let reason: Option<String> = row.get(2);
            let branch: Option<String> = row.get(3);
            let tag: Option<String> = row.get(4);
            let status: String = row.get(5);
            let failure_reason: Option<String> = row.get(6);
            let max_workloads: u32 = row.get(7);

            let status = InvocationStatus::from_db_status(&status, failure_reason)
                .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            let progress = InvocationProgress::from_db(&mut tx, &uuid_str, &status, max_workloads)
                .await
                .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            Ok(Json(Invocation {
                uuid: invocation_uuid,
                commit_sha1,
                machine_hostname,
                reason,
                branch,
                tag,
                status,
                progress,
            }))
        }
        Ok(None) => Err(ResponseError {
            error: anyhow!("invocation '{invocation_uuid}' not found"),
            code: StatusCode::NOT_FOUND,
        }),
        Err(error) => Err(ResponseError {
            error: anyhow::Error::new(error).context("while fetching invocation"),
            code: StatusCode::INTERNAL_SERVER_ERROR,
        }),
    }
}

async fn list_workloads(
    mut tx: Tx,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PaginatedResults<Workload>>> {
    let limit = pagination.limit.unwrap_or(20);
    let offset = pagination.offset.unwrap_or(0);

    let query = sqlx::query("SELECT * from workloads LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset);

    let workloads = query
        .map(|row: SqliteRow| {
            let uuid: String = row.get(0);
            let uuid = Uuid::from_str(&uuid).expect("could not deserialize UUID");
            let invocation_uuid: String = row.get(1);
            let invocation_uuid: Uuid =
                Uuid::from_str(&invocation_uuid).expect("could not deserialize invocation UUID");
            let name: String = row.get(2);
            Workload {
                uuid,
                invocation_uuid,
                name,
            }
        })
        .fetch_all(&mut tx)
        .await
        .context("listing workloads")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?;

    let workload_count: i64 = sqlx::query("SELECT COUNT(uuid) from workloads")
        .fetch_one(&mut tx)
        .await
        .context("fetching workload count")
        .with_code(StatusCode::INTERNAL_SERVER_ERROR)?
        .get(0);

    Ok(Json(PaginatedResults {
        results: workloads,
        total_count: workload_count as u64,
    }))
}

#[derive(Deserialize)]
pub struct NewInvocation {
    pub commit: NewCommit,
    pub machine_hostname: String,
    pub max_workloads: u32,
    pub reason: Option<String>,
    pub timeout: Option<time::Duration>,
}

#[derive(Serialize)]
pub struct Invocation {
    pub uuid: Uuid,
    pub commit_sha1: String,
    pub machine_hostname: String,
    pub reason: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub status: InvocationStatus,
    pub progress: Option<InvocationProgress>,
}

#[derive(Serialize)]
pub struct InvocationProgress {
    pub current_workload: Option<String>,
    pub completed_runs: u32,
    pub max_runs: u32,
    pub completed_workloads: u32,
    pub max_workloads: u32,
}

impl InvocationProgress {
    pub fn as_percent(&self) -> u32 {
        if self.max_workloads == 0 {
            return 0;
        }
        if self.max_runs == 0 {
            (self.completed_workloads * 100) / self.max_workloads
        } else {
            let completed_runs = self.completed_workloads * self.max_runs + self.completed_runs;
            let total_runs = self.max_workloads * self.max_runs;
            (completed_runs * 100) / total_runs
        }
    }
}

impl InvocationProgress {
    async fn from_db(
        tx: &mut Tx,
        uuid: &str,
        status: &InvocationStatus,
        max_workloads: u32,
    ) -> anyhow::Result<Option<Self>> {
        Ok(if status.is_running() {
            // get progress
            let number_of_workloads = crate::number_of_workloads(uuid, tx).await?;
            Some(
                if let Some((workload_uuid, workload_name, max_runs)) =
                    crate::latest_workload(uuid, tx).await?
                {
                    let number_of_runs = crate::number_of_runs(&workload_uuid, tx).await?;
                    crate::InvocationProgress {
                        current_workload: Some(workload_name),
                        completed_runs: number_of_runs,
                        max_runs,
                        completed_workloads: number_of_workloads.saturating_sub(1),
                        max_workloads,
                    }
                } else {
                    crate::InvocationProgress {
                        current_workload: None,
                        completed_runs: 0,
                        max_runs: 0,
                        completed_workloads: 0,
                        max_workloads,
                    }
                },
            )
        } else {
            None
        })
    }
}

#[derive(Deserialize)]
pub struct NewCommit {
    pub sha1: String,
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub commit_date: time::OffsetDateTime,
    pub message: String,
}

#[derive(Deserialize)]
pub struct NewMachine {
    pub hostname: String,
}

#[derive(Deserialize)]
pub struct NewWorkload {
    pub invocation_uuid: Uuid,
    pub name: String,
    pub max_runs: u32,
}

#[derive(Serialize)]
pub struct Workload {
    pub uuid: Uuid,
    pub invocation_uuid: Uuid,
    pub name: String,
}

#[derive(Deserialize)]
pub struct NewRun {
    pub workload_uuid: Uuid,
    pub data: BTreeMap<String, CallStats>,
}

#[derive(Deserialize, Clone, Copy)]
pub struct CallStats {
    pub time: u64,
    pub call_count: u64,
    pub self_time: u64,
}

impl CallStats {
    pub fn avg_time(stats: &[CallStats]) -> Option<u64> {
        if stats.is_empty() {
            return None;
        }
        let sum: u64 = stats.iter().map(|CallStats { time, .. }| time).sum();
        Some(sum / stats.len() as u64)
    }
}

#[derive(Serialize)]
pub struct Machine {
    pub hostname: String,
}

#[derive(Debug)]
struct ResponseError {
    error: anyhow::Error,
    code: StatusCode,
}

trait ResultToResponseError<T> {
    fn with_code(self, code: StatusCode) -> std::result::Result<T, ResponseError>;
}

impl<T> ResultToResponseError<T> for anyhow::Result<T> {
    fn with_code(self, code: StatusCode) -> std::result::Result<T, ResponseError> {
        self.map_err(|error| ResponseError { error, code })
    }
}

impl IntoResponse for ResponseError {
    fn into_response(self) -> axum::response::Response {
        let sources: Vec<_> = std::iter::successors(self.error.source(), |error| error.source())
            .map(|error| error.to_string())
            .collect();
        (
            self.code,
            Json(json!({
                "error": self.error.to_string(),
                "sources": sources,
            }))
            .into_response(),
        )
            .into_response()
    }
}

async fn get_style() -> Css<&'static str> {
    Css(include_str!("../assets/stylesheet.css"))
}

async fn get_custom_style() -> Css<&'static str> {
    Css(include_str!("../assets/custom.css"))
}

async fn get_icon() -> impl axum::response::IntoResponse {
    let scouter = include_bytes!("../assets/scouter.png");
    let mut header = axum::http::HeaderMap::new();
    header.append(
        axum::http::header::CONTENT_TYPE,
        "image/png".parse().unwrap(),
    );
    (header, scouter)
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Serialize)]
struct PaginatedResults<T> {
    pub results: Vec<T>,
    pub total_count: u64,
}
