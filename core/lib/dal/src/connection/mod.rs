use std::{
    env, fmt,
    future::Future,
    panic::Location,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Context as _;
use rand::Rng;
use sqlx::{
    pool::PoolConnection,
    postgres::{PgConnectOptions, PgPool, PgPoolOptions, Postgres},
};

pub use self::processor::StorageProcessor;
pub(crate) use self::processor::StorageProcessorTags;
use self::processor::TracedConnections;
use crate::metrics::{PostgresMetrics, CONNECTION_METRICS};

mod processor;

/// Builder for [`ConnectionPool`]s.
#[derive(Clone)]
pub struct ConnectionPoolBuilder {
    database_url: String,
    max_size: u32,
    acquire_timeout: Duration,
    statement_timeout: Option<Duration>,
}

impl fmt::Debug for ConnectionPoolBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Database URL is potentially sensitive, thus we omit it.
        formatter
            .debug_struct("ConnectionPoolBuilder")
            .field("max_size", &self.max_size)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("statement_timeout", &self.statement_timeout)
            .finish()
    }
}

impl ConnectionPoolBuilder {
    /// Overrides the maximum number of connections that can be allocated by the pool.
    pub fn set_max_size(&mut self, max_size: u32) -> &mut Self {
        self.max_size = max_size;
        self
    }

    /// Sets the acquire timeout for a single connection attempt. There are multiple attempts (currently 3)
    /// before `access_storage*` methods return an error. If not specified, the acquire timeout will not be set.
    pub fn set_acquire_timeout(&mut self, timeout: Option<Duration>) -> &mut Self {
        if let Some(timeout) = timeout {
            self.acquire_timeout = timeout;
        }
        self
    }

    /// Sets the statement timeout for the pool. See [Postgres docs] for semantics.
    /// If not specified, the statement timeout will not be set.
    ///
    /// [Postgres docs]: https://www.postgresql.org/docs/14/runtime-config-client.html
    pub fn set_statement_timeout(&mut self, timeout: Option<Duration>) -> &mut Self {
        self.statement_timeout = timeout;
        self
    }

    /// Returns the maximum number of connections that can be allocated by the pool.
    pub fn max_size(&self) -> u32 {
        self.max_size
    }

    /// Builds a connection pool from this builder.
    pub async fn build(&self) -> anyhow::Result<ConnectionPool> {
        let options = PgPoolOptions::new()
            .max_connections(self.max_size)
            .acquire_timeout(self.acquire_timeout);
        let mut connect_options: PgConnectOptions = self
            .database_url
            .parse()
            .context("Failed parsing database URL")?;
        if let Some(timeout) = self.statement_timeout {
            let timeout_string = format!("{}s", timeout.as_secs());
            connect_options = connect_options.options([("statement_timeout", timeout_string)]);
        }
        let pool = options
            .connect_with(connect_options)
            .await
            .context("Failed connecting to database")?;
        tracing::info!("Created DB pool with parameters {self:?}");
        Ok(ConnectionPool {
            database_url: self.database_url.clone(),
            inner: pool,
            max_size: self.max_size,
            traced_connections: None,
        })
    }
}

#[derive(Debug)]
pub struct TestTemplate(url::Url);

impl TestTemplate {
    fn db_name(&self) -> &str {
        self.0.path().strip_prefix('/').unwrap()
    }

    fn url(&self, db_name: &str) -> url::Url {
        let mut url = self.0.clone();
        url.set_path(db_name);
        url
    }

    async fn connect_to(db_url: &url::Url) -> sqlx::Result<sqlx::PgConnection> {
        use sqlx::Connection as _;
        let mut attempts = 10;
        loop {
            match sqlx::PgConnection::connect(db_url.as_ref()).await {
                Ok(conn) => return Ok(conn),
                Err(err) => {
                    attempts -= 1;
                    if attempts == 0 {
                        return Err(err);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Obtains the test database URL from the environment variable.
    pub fn empty() -> anyhow::Result<Self> {
        let db_url = env::var("TEST_DATABASE_URL").context(
            "TEST_DATABASE_URL must be set. Normally, this is done by the 'zk' tool. \
            Make sure that you are running the tests with 'zk test rust' command or equivalent.",
        )?;
        Ok(Self(db_url.parse()?))
    }

    /// Closes the connection pool, disallows connecting to the underlying db,
    /// so that the db can be used as a template.
    pub async fn freeze(pool: ConnectionPool) -> anyhow::Result<Self> {
        use sqlx::Executor as _;
        let mut conn = pool.acquire_connection_retried(None).await?;
        conn.execute(
            "UPDATE pg_database SET datallowconn = false WHERE datname = current_database()",
        )
        .await
        .context("SET dataallowconn = false")?;
        drop(conn);
        pool.inner.close().await;
        Ok(Self(pool.database_url.parse()?))
    }

    /// Constructs a new temporary database (with a randomized name)
    /// by cloning the database template pointed by TEST_DATABASE_URL env var.
    /// The template is expected to have all migrations from dal/migrations applied.
    /// For efficiency, the Postgres container of TEST_DATABASE_URL should be
    /// configured with option "fsync=off" - it disables waiting for disk synchronization
    /// whenever you write to the DBs, therefore making it as fast as an in-memory Postgres instance.
    /// The database is not cleaned up automatically, but rather the whole Postgres
    /// container is recreated whenever you call "zk test rust".
    pub async fn create_db(&self, connections: u32) -> anyhow::Result<ConnectionPoolBuilder> {
        use sqlx::Executor as _;

        let mut conn = Self::connect_to(&self.url(""))
            .await
            .context("connect_to()")?;
        let db_old = self.db_name();
        let db_new = format!("test-{}", rand::thread_rng().gen::<u64>());
        conn.execute(format!("CREATE DATABASE \"{db_new}\" WITH TEMPLATE \"{db_old}\"").as_str())
            .await
            .context("CREATE DATABASE")?;

        Ok(ConnectionPool::builder(
            self.url(&db_new).as_ref(),
            connections,
        ))
    }
}

/// Global DB connection parameters applied to all [`ConnectionPool`] instances.
#[derive(Debug)]
pub struct GlobalConnectionPoolConfig {
    // We consider millisecond precision to be enough for config purposes.
    long_connection_threshold_ms: AtomicU64,
    slow_query_threshold_ms: AtomicU64,
}

impl GlobalConnectionPoolConfig {
    const fn new() -> Self {
        Self {
            long_connection_threshold_ms: AtomicU64::new(5_000), // 5 seconds
            slow_query_threshold_ms: AtomicU64::new(100),        // 0.1 seconds
        }
    }

    pub(crate) fn long_connection_threshold(&self) -> Duration {
        Duration::from_millis(self.long_connection_threshold_ms.load(Ordering::Relaxed))
    }

    pub(crate) fn slow_query_threshold(&self) -> Duration {
        Duration::from_millis(self.slow_query_threshold_ms.load(Ordering::Relaxed))
    }

    /// Sets the threshold for the DB connection lifetime to denote a connection as long-living and log its details.
    pub fn set_long_connection_threshold(&self, threshold: Duration) -> anyhow::Result<&Self> {
        let millis = u64::try_from(threshold.as_millis())
            .context("long_connection_threshold is unreasonably large")?;
        self.long_connection_threshold_ms
            .store(millis, Ordering::Relaxed);
        tracing::info!("Set long connection threshold to {threshold:?}");
        Ok(self)
    }

    /// Sets the threshold to denote a DB query as "slow" and log its details.
    pub fn set_slow_query_threshold(&self, threshold: Duration) -> anyhow::Result<&Self> {
        let millis = u64::try_from(threshold.as_millis())
            .context("slow_query_threshold is unreasonably large")?;
        self.slow_query_threshold_ms
            .store(millis, Ordering::Relaxed);
        tracing::info!("Set slow query threshold to {threshold:?}");
        Ok(self)
    }
}

#[derive(Clone)]
pub struct ConnectionPool {
    pub(crate) inner: PgPool,
    database_url: String,
    max_size: u32,
    traced_connections: Option<Arc<TracedConnections>>,
}

impl fmt::Debug for ConnectionPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We don't print the `database_url`, as is may contain
        // sensitive information (e.g. database password).
        formatter
            .debug_struct("ConnectionPool")
            .field("max_size", &self.max_size)
            .finish_non_exhaustive()
    }
}

impl ConnectionPool {
    const TEST_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

    /// Returns a reference to the global configuration parameters applied for all DB pools. For consistency, these parameters
    /// should be changed early in the app life cycle.
    pub fn global_config() -> &'static GlobalConnectionPoolConfig {
        static CONFIG: GlobalConnectionPoolConfig = GlobalConnectionPoolConfig::new();
        &CONFIG
    }

    /// Creates a test pool with a reasonably large number of connections.
    ///
    /// Test pools trace their active connections. If acquiring a connection fails (e.g., with a timeout),
    /// the returned error will contain information on all active connections.
    pub async fn test_pool() -> ConnectionPool {
        const DEFAULT_CONNECTIONS: u32 = 50; // Expected to be enough for any unit test.
        Self::constrained_test_pool(DEFAULT_CONNECTIONS).await
    }

    /// Same as [`Self::test_pool()`], but with a configurable number of connections. This is useful to test
    /// behavior of components that rely on singleton / constrained pools in production.
    pub async fn constrained_test_pool(connections: u32) -> ConnectionPool {
        assert!(connections > 0, "Number of connections must be positive");
        let mut builder = TestTemplate::empty()
            .expect("failed creating test template")
            .create_db(connections)
            .await
            .expect("failed creating database for tests");
        let mut pool = builder
            .set_acquire_timeout(Some(Self::TEST_ACQUIRE_TIMEOUT))
            .build()
            .await
            .expect("cannot build connection pool");
        pool.traced_connections = Some(Arc::default());
        pool
    }

    /// Initializes a builder for connection pools.
    pub fn builder(database_url: &str, max_pool_size: u32) -> ConnectionPoolBuilder {
        ConnectionPoolBuilder {
            database_url: database_url.to_string(),
            max_size: max_pool_size,
            acquire_timeout: Duration::from_secs(30), // Default value used by `sqlx`
            statement_timeout: None,
        }
    }

    /// Initializes a builder for connection pools with a single connection. This is equivalent
    /// to calling `Self::builder(db_url, 1)`.
    pub fn singleton(database_url: &str) -> ConnectionPoolBuilder {
        Self::builder(database_url, 1)
    }

    /// Returns the maximum number of connections in this pool specified during its creation.
    /// This number may be distinct from the current number of connections in the pool (including
    /// idle ones).
    pub fn max_size(&self) -> u32 {
        self.max_size
    }

    /// Uses this pool to report Postgres-wide metrics (e.g., table sizes). Should be called sparingly to not spam
    /// identical metrics from multiple places. The returned future runs indefinitely and should be spawned as a Tokio task.
    pub async fn run_postgres_metrics_scraping(self, scrape_interval: Duration) {
        PostgresMetrics::run_scraping(self, scrape_interval).await;
    }

    /// Creates a `StorageProcessor` entity over a recoverable connection.
    /// Upon a database outage connection will block the thread until
    /// it will be able to recover the connection (or, if connection cannot
    /// be restored after several retries, this will be considered as
    /// irrecoverable database error and result in panic).
    ///
    /// This method is intended to be used in crucial contexts, where the
    /// database access is must-have (e.g. block committer).
    pub async fn access_storage(&self) -> anyhow::Result<StorageProcessor<'_>> {
        self.access_storage_inner(None).await
    }

    /// A version of `access_storage` that would also expose the duration of the connection
    /// acquisition tagged to the `requester` name. It also tracks the caller location for the purposes
    /// of logging (e.g., long-living connections) and debugging (when used with a test connection pool).
    ///
    /// WARN: This method should not be used if it will result in too many time series (e.g.
    /// from witness generators or provers), otherwise Prometheus won't be able to handle it.
    #[track_caller] // In order to use it, we have to de-sugar `async fn`
    pub fn access_storage_tagged(
        &self,
        requester: &'static str,
    ) -> impl Future<Output = anyhow::Result<StorageProcessor<'_>>> + '_ {
        let location = Location::caller();
        async move {
            let tags = StorageProcessorTags {
                requester,
                location,
            };
            self.access_storage_inner(Some(tags)).await
        }
    }

    async fn access_storage_inner(
        &self,
        tags: Option<StorageProcessorTags>,
    ) -> anyhow::Result<StorageProcessor<'_>> {
        let acquire_latency = CONNECTION_METRICS.acquire.start();
        let conn = self
            .acquire_connection_retried(tags.as_ref())
            .await
            .context("acquire_connection_retried()")?;
        let elapsed = acquire_latency.observe();
        if let Some(tags) = &tags {
            CONNECTION_METRICS.acquire_tagged[&tags.requester].observe(elapsed);
        }
        Ok(StorageProcessor::from_pool(
            conn,
            tags,
            self.traced_connections.as_deref(),
        ))
    }

    async fn acquire_connection_retried(
        &self,
        tags: Option<&StorageProcessorTags>,
    ) -> anyhow::Result<PoolConnection<Postgres>> {
        const DB_CONNECTION_RETRIES: usize = 3;
        const AVG_BACKOFF_INTERVAL: Duration = Duration::from_secs(1);

        for _ in 0..DB_CONNECTION_RETRIES {
            CONNECTION_METRICS
                .pool_size
                .observe(self.inner.size() as usize);
            CONNECTION_METRICS.pool_idle.observe(self.inner.num_idle());

            let connection = self.inner.acquire().await;
            let connection_err = match connection {
                Ok(connection) => return Ok(connection),
                Err(err) => err,
            };

            Self::report_connection_error(&connection_err);
            // Slightly randomize back-off interval so that we don't end up stampeding the DB.
            let jitter = rand::thread_rng().gen_range(0.8..1.2);
            let backoff_interval = AVG_BACKOFF_INTERVAL.mul_f32(jitter);
            let tags_display = StorageProcessorTags::display(tags);
            tracing::warn!(
                "Failed to get connection to DB ({tags_display}), backing off for {backoff_interval:?}: {connection_err}"
            );
            tokio::time::sleep(backoff_interval).await;
        }

        // Attempting to get the pooled connection for the last time
        match self.inner.acquire().await {
            Ok(conn) => Ok(conn),
            Err(err) => {
                Self::report_connection_error(&err);
                let tags_display = StorageProcessorTags::display(tags);
                if let Some(traced_connections) = &self.traced_connections {
                    anyhow::bail!(
                        "Run out of retries getting a DB connection ({tags_display}), last error: {err}\n\
                         Active connections: {traced_connections:#?}"
                    );
                } else {
                    anyhow::bail!("Run out of retries getting a DB connection ({tags_display}), last error: {err}");
                }
            }
        }
    }

    fn report_connection_error(err: &sqlx::Error) {
        CONNECTION_METRICS.pool_acquire_error[&err.into()].inc();
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;

    #[tokio::test]
    async fn setting_statement_timeout() {
        let db_url = TestTemplate::empty()
            .unwrap()
            .create_db(1)
            .await
            .unwrap()
            .database_url;

        let pool = ConnectionPool::singleton(&db_url)
            .set_statement_timeout(Some(Duration::from_secs(1)))
            .build()
            .await
            .unwrap();

        let mut storage = pool.access_storage().await.unwrap();
        let err = sqlx::query("SELECT pg_sleep(2)")
            .map(drop)
            .fetch_optional(storage.conn())
            .await
            .unwrap_err();
        assert_matches!(
            err,
            sqlx::Error::Database(db_err) if db_err.message().contains("statement timeout")
        );
    }
}
