//! This module contains the HTTP api for InfluxDB IOx, including a
//! partial implementation of the /v2 HTTP api routes from InfluxDB
//! for compatibility.
//!
//! Note that these routes are designed to be just helpers for now,
//! and "close enough" to the real /v2 api to be able to test InfluxDB IOx
//! without needing to create and manage a mapping layer from name -->
//! id (this is done by other services in the influx cloud)
//!
//! Long term, we expect to create IOx specific api in terms of
//! database names and may remove this quasi /v2 API.

// Influx crates
use data_types::{
    names::{org_and_bucket_to_database, OrgBucketMappingError},
    DatabaseName,
};
use influxdb_iox_client::format::QueryOutputFormat;
use predicate::delete_predicate::{parse_delete, DeletePredicate};
use query::exec::ExecutionContextProvider;
use server::{connection::ConnectionManager, Error};

// External crates
use async_trait::async_trait;
use http::header::CONTENT_TYPE;
use hyper::{Body, Method, Request, Response, StatusCode};
use observability_deps::tracing::{debug, error};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt, Snafu};

use crate::influxdb_ioxd::{
    http::{
        metrics::LineProtocolMetrics,
        utils::parse_body,
        write::{HttpDrivenWrite, InnerWriteError, RequestOrResponse, WriteInfo},
    },
    planner::Planner,
    server_type::{ApiErrorCode, RouteError},
};
use mutable_batch::DbWrite;
use std::{
    fmt::Debug,
    str::{self, FromStr},
    sync::Arc,
};

use super::DatabaseServerType;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Snafu)]
pub enum ApplicationError {
    #[snafu(display("Internal error mapping org & bucket: {}", source))]
    BucketMappingError { source: OrgBucketMappingError },

    #[snafu(display("Internal error reading points from database {}:  {}", db_name, source))]
    Query {
        db_name: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Expected query string in request, but none was provided"))]
    ExpectedQueryString {},

    /// Error for when we could not parse the http query uri (e.g.
    /// `?foo=bar&bar=baz)`
    #[snafu(display("Invalid query string in HTTP URI '{}': {}", query_string, source))]
    InvalidQueryString {
        query_string: String,
        source: serde_urlencoded::de::Error,
    },

    #[snafu(display("Error reading request body as utf8: {}", source))]
    ReadingBodyAsUtf8 { source: std::str::Utf8Error },

    #[snafu(display("Error parsing delete {}: {}", input, source))]
    ParsingDelete {
        source: predicate::delete_predicate::Error,
        input: String,
    },

    #[snafu(display("Error building delete predicate {}: {}", input, source))]
    BuildingDeletePredicate {
        source: predicate::delete_predicate::Error,
        input: String,
    },

    #[snafu(display("Error executing delete {}: {}", input, source))]
    ExecutingDelete {
        source: server::db::Error,
        input: String,
    },

    #[snafu(display("No handler for {:?} {}", method, path))]
    RouteNotFound { method: Method, path: String },

    #[snafu(display("Invalid database name: {}", source))]
    DatabaseNameError {
        source: data_types::DatabaseNameError,
    },

    #[snafu(display("Database {} not found", db_name))]
    DatabaseNotFound { db_name: String },

    #[snafu(display("Internal error creating HTTP response:  {}", source))]
    CreatingResponse { source: http::Error },

    #[snafu(display("Invalid format '{}': : {}", format, source))]
    ParsingFormat {
        format: String,
        source: influxdb_iox_client::format::Error,
    },

    #[snafu(display(
        "Error formatting results of SQL query '{}' using '{:?}': {}",
        q,
        format,
        source
    ))]
    FormattingResult {
        q: String,
        format: QueryOutputFormat,
        source: influxdb_iox_client::format::Error,
    },

    #[snafu(display("Error while planning query: {}", source))]
    Planning {
        source: crate::influxdb_ioxd::planner::Error,
    },

    #[snafu(display("Server id not set"))]
    ServerIdNotSet,

    #[snafu(display("Server not initialized"))]
    ServerNotInitialized,

    #[snafu(display("Database {} not found", db_name))]
    DatabaseNotInitialized { db_name: String },

    #[snafu(display("Internal server error"))]
    InternalServerError,

    #[snafu(display("Cannot parse body: {}", source))]
    ParseBody {
        source: crate::influxdb_ioxd::http::utils::ParseBodyError,
    },

    #[snafu(display("Cannot write data: {}", source))]
    WriteError {
        source: crate::influxdb_ioxd::http::write::HttpWriteError,
    },
}

type Result<T, E = ApplicationError> = std::result::Result<T, E>;

impl RouteError for ApplicationError {
    fn response(&self) -> Response<Body> {
        match self {
            Self::BucketMappingError { .. } => self.internal_error(ApiErrorCode::UNKNOWN),
            Self::Query { .. } => self.internal_error(ApiErrorCode::UNKNOWN),
            Self::ExpectedQueryString { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::InvalidQueryString { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::ReadingBodyAsUtf8 { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::ParsingDelete { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::BuildingDeletePredicate { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::ExecutingDelete { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::RouteNotFound { .. } => self.not_found(),
            Self::DatabaseNameError { .. } => self.bad_request(ApiErrorCode::DB_INVALID_NAME),
            Self::DatabaseNotFound { .. } => self.not_found(),
            Self::CreatingResponse { .. } => self.internal_error(ApiErrorCode::UNKNOWN),
            Self::FormattingResult { .. } => self.internal_error(ApiErrorCode::UNKNOWN),
            Self::ParsingFormat { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::Planning { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::ServerIdNotSet => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::ServerNotInitialized => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::DatabaseNotInitialized { .. } => self.bad_request(ApiErrorCode::UNKNOWN),
            Self::InternalServerError => self.internal_error(ApiErrorCode::UNKNOWN),
            Self::ParseBody { source } => source.response(),
            Self::WriteError { source } => source.response(),
        }
    }
}

impl From<server::Error> for ApplicationError {
    fn from(e: Error) -> Self {
        match e {
            Error::IdNotSet => Self::ServerIdNotSet,
            Error::ServerNotInitialized { .. } => Self::ServerNotInitialized,
            Error::DatabaseNotInitialized { db_name } => Self::DatabaseNotInitialized { db_name },
            Error::DatabaseNotFound { db_name } => Self::DatabaseNotFound { db_name },
            Error::InvalidDatabaseName { source } => Self::DatabaseNameError { source },
            e => {
                error!(%e, "unexpected server error");
                // Don't return potentially sensitive information in response
                Self::InternalServerError
            }
        }
    }
}

#[async_trait]
impl<M> HttpDrivenWrite for DatabaseServerType<M>
where
    M: ConnectionManager + Send + Sync + Debug + 'static,
{
    fn max_request_size(&self) -> usize {
        self.max_request_size
    }

    fn lp_metrics(&self) -> Arc<LineProtocolMetrics> {
        Arc::clone(&self.lp_metrics)
    }

    async fn write(
        &self,
        db_name: &DatabaseName<'_>,
        write: DbWrite,
    ) -> Result<(), InnerWriteError> {
        self.server
            .write(db_name, write)
            .await
            .map_err(|e| match e {
                server::Error::DatabaseNotFound { .. } => InnerWriteError::NotFound {
                    db_name: db_name.to_string(),
                },
                e => InnerWriteError::OtherError {
                    source: Box::new(e),
                },
            })
    }
}

pub async fn route_request<M>(
    server_type: &DatabaseServerType<M>,
    req: Request<Body>,
) -> Result<Response<Body>, ApplicationError>
where
    M: ConnectionManager + Send + Sync + Debug + 'static,
{
    match server_type
        .route_write_http_request(req)
        .await
        .context(WriteError)?
    {
        RequestOrResponse::Response(resp) => Ok(resp),
        RequestOrResponse::Request(req) => {
            let method = req.method().clone();
            let uri = req.uri().clone();

            match (method.clone(), uri.path()) {
                (Method::POST, "/api/v2/delete") => delete(req, server_type).await,
                (Method::GET, "/api/v3/query") => query(req, server_type).await,

                (method, path) => Err(ApplicationError::RouteNotFound {
                    method,
                    path: path.to_string(),
                }),
            }
        }
    }
}

async fn delete<M>(
    req: Request<Body>,
    server_type: &DatabaseServerType<M>,
) -> Result<Response<Body>, ApplicationError>
where
    M: ConnectionManager + Send + Sync + Debug + 'static,
{
    let DatabaseServerType {
        server,
        max_request_size,
        ..
    } = server_type;
    let max_request_size = *max_request_size;
    let server = Arc::clone(server);

    // Extract the DB name from the request
    // db_name = orrID_bucketID
    let query = req.uri().query().context(ExpectedQueryString)?;
    let delete_info: WriteInfo = serde_urlencoded::from_str(query).context(InvalidQueryString {
        query_string: String::from(query),
    })?;
    let db_name = org_and_bucket_to_database(&delete_info.org, &delete_info.bucket)
        .context(BucketMappingError)?;

    // Parse body
    let body = parse_body(req, max_request_size).await.context(ParseBody)?;
    let body = str::from_utf8(&body).context(ReadingBodyAsUtf8)?;

    // Parse and extract table name (which can be empty), start, stop, and predicate
    let parsed_delete = parse_delete(body).context(ParsingDelete { input: body })?;

    let table_name = parsed_delete.table_name;
    let predicate = parsed_delete.predicate;
    let start = parsed_delete.start_time;
    let stop = parsed_delete.stop_time;
    debug!(%table_name, %predicate, %start, %stop, body_size=body.len(), %db_name, org=%delete_info.org, bucket=%delete_info.bucket, "delete data from database");

    // Validate that the database name is legit
    let db = server.db(&db_name)?;

    // Build delete predicate
    let del_predicate = DeletePredicate::try_new(&start, &stop, &predicate)
        .context(BuildingDeletePredicate { input: body })?;

    // Tables data will be deleted from
    // Note for developer:  this the only place we support INFLUX DELETE that deletes
    // data from many tables in one command. If you want to use general delete API to
    // delete data from a specified table, use the one in the management API (src/influxdb_ioxd/rpc/management.rs) instead
    let mut tables = vec![];
    if table_name.is_empty() {
        tables = db.table_names();
    } else {
        tables.push(table_name);
    }

    // Execute delete
    for table in tables {
        db.delete(&table, Arc::new(del_predicate.clone()))
            .await
            .context(ExecutingDelete { input: body })?;
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

#[derive(Deserialize, Debug, PartialEq)]
/// Parsed URI Parameters of the request to the .../query endpoint
struct QueryParams {
    #[serde(alias = "database")]
    d: String,
    #[serde(alias = "query")]
    q: String,
    #[serde(default = "default_format")]
    format: String,
}

fn default_format() -> String {
    QueryOutputFormat::default().to_string()
}

async fn query<M: ConnectionManager + Send + Sync + Debug + 'static>(
    req: Request<Body>,
    server_type: &DatabaseServerType<M>,
) -> Result<Response<Body>, ApplicationError> {
    let server = &server_type.server;

    let uri_query = req.uri().query().context(ExpectedQueryString {})?;

    let QueryParams { d, q, format } =
        serde_urlencoded::from_str(uri_query).context(InvalidQueryString {
            query_string: uri_query,
        })?;

    let format = QueryOutputFormat::from_str(&format).context(ParsingFormat { format })?;

    let db_name = DatabaseName::new(&d).context(DatabaseNameError)?;
    debug!(uri = ?req.uri(), %q, ?format, %db_name, "running SQL query");

    let db = server.db(&db_name)?;

    let ctx = db.new_query_context(req.extensions().get().cloned());
    let physical_plan = Planner::new(&ctx).sql(&q).await.context(Planning)?;

    // TODO: stream read results out rather than rendering the
    // whole thing in mem
    let batches = ctx
        .collect(physical_plan)
        .await
        .map_err(|e| Box::new(e) as _)
        .context(Query { db_name })?;

    let results = format
        .format(&batches)
        .context(FormattingResult { q, format })?;

    let body = Body::from(results.into_bytes());

    let response = Response::builder()
        .header(CONTENT_TYPE, format.content_type())
        .body(body)
        .context(CreatingResponse)?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use crate::influxdb_ioxd::{
        http::test_utils::{
            assert_health, assert_metrics, check_response, get_content_type, TestServer,
        },
        server_type::{common_state::CommonServerState, ServerType},
    };

    use super::*;
    use std::convert::TryFrom;

    use arrow::record_batch::RecordBatch;
    use arrow_util::assert_batches_eq;
    use http::header::CONTENT_ENCODING;
    use reqwest::Client;

    use data_types::{database_rules::DatabaseRules, server_id::ServerId, DatabaseName};
    use metric::{Attributes, DurationHistogram, Metric, U64Counter, U64Histogram};
    use object_store::ObjectStore;
    use server::{
        connection::ConnectionManagerImpl, db::Db, rules::ProvidedDatabaseRules, ApplicationState,
        Server,
    };
    use trace::RingBufferTraceCollector;

    fn make_application() -> Arc<ApplicationState> {
        Arc::new(ApplicationState::new(
            Arc::new(ObjectStore::new_in_memory()),
            None,
            None,
        ))
    }

    fn make_server(application: Arc<ApplicationState>) -> Arc<Server<ConnectionManagerImpl>> {
        Arc::new(Server::new(
            ConnectionManagerImpl::new(),
            application,
            Default::default(),
        ))
    }

    #[tokio::test]
    async fn test_health() {
        assert_health(setup_server().await).await;
    }

    #[tokio::test]
    async fn test_metrics() {
        assert_metrics(setup_server().await).await;
    }

    #[tokio::test]
    async fn test_tracing() {
        let trace_collector = Arc::new(RingBufferTraceCollector::new(5));
        let application = Arc::new(ApplicationState::new(
            Arc::new(ObjectStore::new_in_memory()),
            None,
            Some(Arc::<RingBufferTraceCollector>::clone(&trace_collector)),
        ));
        let app_server = make_server(Arc::clone(&application));
        let server_type =
            DatabaseServerType::new(application, app_server, &CommonServerState::for_testing());
        let test_server = TestServer::new(Arc::new(server_type));

        let client = Client::new();
        let response = client
            .get(&format!("{}/health", test_server.url()))
            .header("uber-trace-id", "34f3495:36e34:0:1")
            .send()
            .await;

        // Print the response so if the test fails, we have a log of what went wrong
        check_response("health", response, StatusCode::OK, Some("OK")).await;

        let mut spans = trace_collector.spans();
        assert_eq!(spans.len(), 1);

        let span = spans.pop().unwrap();
        assert_eq!(span.ctx.trace_id.get(), 0x34f3495);
        assert_eq!(span.ctx.parent_span_id.unwrap().get(), 0x36e34);
    }

    #[tokio::test]
    async fn test_write() {
        let test_server = setup_server().await;

        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000";

        // send write data
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(lp_data)
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, Some("")).await;

        // Check that the data got into the right bucket
        let test_db = test_server
            .server_type()
            .server
            .db(&DatabaseName::new("MyOrg_MyBucket").unwrap())
            .expect("Database exists");

        let batches = run_query(test_db, "select * from h2o_temperature").await;
        let expected = vec![
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| bottom_degrees | location     | state | surface_degrees | time                 |",
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| 50.4           | santa_monica | CA    | 65.2            | 2021-04-01T14:10:24Z |",
            "+----------------+--------------+-------+-----------------+----------------------+",
        ];
        assert_batches_eq!(expected, &batches);
    }

    #[tokio::test]
    async fn test_delete() {
        // Set up server
        let test_server = setup_server().await;

        // Set up client
        let client = Client::new();
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";

        // Client requests delete something from an empty DB
        let delete_line = r#"{"start":"1970-01-01T00:00:00Z","stop":"2070-01-02T00:00:00Z", "predicate":"host=\"Orient.local\""}"#;
        let response = client
            .post(&format!(
                "{}/api/v2/delete?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(delete_line)
            .send()
            .await;
        check_response("delete", response, StatusCode::NO_CONTENT, Some("")).await;

        // Client writes data to the server
        let lp_data = r#"h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000
               h2o_temperature,location=Boston,state=MA surface_degrees=47.5,bottom_degrees=35 1617286224000000123"#;
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(lp_data)
            .send()
            .await;
        check_response("write", response, StatusCode::NO_CONTENT, Some("")).await;

        // Check that the data got into the right bucket
        let test_db = test_server
            .server_type()
            .server
            .db(&DatabaseName::new("MyOrg_MyBucket").unwrap())
            .expect("Database exists");
        let batches = run_query(
            Arc::clone(&test_db),
            "select * from h2o_temperature order by location",
        )
        .await;
        let expected = vec![
            "+----------------+--------------+-------+-----------------+--------------------------------+",
            "| bottom_degrees | location     | state | surface_degrees | time                           |",
            "+----------------+--------------+-------+-----------------+--------------------------------+",
            "| 35             | Boston       | MA    | 47.5            | 2021-04-01T14:10:24.000000123Z |",
            "| 50.4           | santa_monica | CA    | 65.2            | 2021-04-01T14:10:24Z           |",
            "+----------------+--------------+-------+-----------------+--------------------------------+",
        ];
        assert_batches_eq!(expected, &batches);

        // Now delete something
        let delete_line = r#"{"start":"2021-04-01T14:00:00Z","stop":"2021-04-02T14:00:00Z", "predicate":"location=Boston"}"#;
        let response = client
            .post(&format!(
                "{}/api/v2/delete?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(delete_line)
            .send()
            .await;
        check_response("delete", response, StatusCode::NO_CONTENT, Some("")).await;

        // query again and should not get the deleted data
        let batches = run_query(test_db, "select * from h2o_temperature").await;
        let expected = vec![
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| bottom_degrees | location     | state | surface_degrees | time                 |",
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| 50.4           | santa_monica | CA    | 65.2            | 2021-04-01T14:10:24Z |",
            "+----------------+--------------+-------+-----------------+----------------------+",
        ];
        assert_batches_eq!(expected, &batches);

        // -------------------
        // negative tests
        // Not able to parse _measurement="not_a_table"  (it must be _measurement=\"not_a_table\" to work)
        let delete_line = r#"{"start":"2021-04-01T14:00:00Z","stop":"2021-04-02T14:00:00Z", "predicate":"_measurement="not_a_table" and location=Boston"}"#;
        let response = client
            .post(&format!(
                "{}/api/v2/delete?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(delete_line)
            .send()
            .await;
        check_response(
            "delete",
            response,
            StatusCode::BAD_REQUEST,
            Some("Unable to parse delete string"),
        )
        .await;

        // delete from non-existing table
        let delete_line = r#"{"start":"2021-04-01T14:00:00Z","stop":"2021-04-02T14:00:00Z", "predicate":"_measurement=not_a_table and location=Boston"}"#;
        let response = client
            .post(&format!(
                "{}/api/v2/delete?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(delete_line)
            .send()
            .await;
        check_response(
            "delete",
            response,
            StatusCode::BAD_REQUEST,
            Some("Cannot delete data from non-existing table"),
        )
        .await;
    }

    #[tokio::test]
    async fn test_write_metrics() {
        let test_server = setup_server().await;
        let metric_registry = test_server.server_type().metric_registry();

        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1568756160";
        let incompatible_lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=\"incompatible\" 1568756170";

        // send good data
        let org_name = "MyOrg";
        let bucket_name = "MyBucket";
        let post_url = format!(
            "{}/api/v2/write?bucket={}&org={}",
            test_server.url(),
            bucket_name,
            org_name
        );
        client
            .post(&post_url)
            .body(lp_data)
            .send()
            .await
            .expect("sent data");

        // The request completed successfully
        let request_count = metric_registry
            .get_instrument::<Metric<U64Counter>>("http_requests")
            .unwrap();

        let request_count_ok = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        let request_count_client_error = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "client_error"),
            ]))
            .unwrap()
            .clone();

        let request_count_server_error = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "server_error"),
            ]))
            .unwrap()
            .clone();

        let request_duration_ok = metric_registry
            .get_instrument::<Metric<DurationHistogram>>("http_request_duration")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        assert_eq!(request_duration_ok.fetch().sample_count(), 1);
        assert_eq!(request_count_ok.fetch(), 1);
        assert_eq!(request_count_client_error.fetch(), 0);
        assert_eq!(request_count_server_error.fetch(), 0);

        // A single successful point landed
        let ingest_lines = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_lines")
            .unwrap();

        let ingest_lines_ok = ingest_lines
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        let ingest_lines_error = ingest_lines
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "error"),
            ]))
            .unwrap()
            .clone();

        assert_eq!(ingest_lines_ok.fetch(), 1);
        assert_eq!(ingest_lines_error.fetch(), 0);

        // Which consists of two fields
        let observation = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_fields")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation, 2);

        // Bytes of data were written
        let observation = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_bytes")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation, 98);

        // Batch size distribution is measured
        let observation = metric_registry
            .get_instrument::<Metric<U64Histogram>>("ingest_batch_size_bytes")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation.total, 98);
        assert_eq!(observation.buckets[0].count, 1);
        assert_eq!(observation.buckets[1].count, 0);

        // Write to a non-existent database
        client
            .post(&format!(
                "{}/api/v2/write?bucket=NotMyBucket&org=NotMyOrg",
                test_server.url(),
            ))
            .body(lp_data)
            .send()
            .await
            .unwrap();

        // An invalid database should not be reported as a new metric
        assert!(metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_lines")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "NotMyOrg_NotMyBucket"),
                ("status", "error"),
            ]))
            .is_none());
        assert_eq!(ingest_lines_ok.fetch(), 1);
        assert_eq!(ingest_lines_error.fetch(), 0);

        // Perform an invalid write
        client
            .post(&post_url)
            .body(incompatible_lp_data)
            .send()
            .await
            .unwrap();

        // This currently results in an InternalServerError which is correctly recorded
        // as a server error, but this should probably be a BadRequest client error (#2538)
        assert_eq!(ingest_lines_ok.fetch(), 1);
        assert_eq!(ingest_lines_error.fetch(), 1);
        assert_eq!(request_duration_ok.fetch().sample_count(), 1);
        assert_eq!(request_count_ok.fetch(), 1);
        assert_eq!(request_count_client_error.fetch(), 0);
        assert_eq!(request_count_server_error.fetch(), 1);
    }

    /// Sets up a test database with some data for testing the query endpoint
    /// returns a client for communicating with the server, and the server
    /// endpoint
    async fn setup_test_data() -> (
        Client,
        TestServer<DatabaseServerType<ConnectionManagerImpl>>,
    ) {
        let test_server = setup_server().await;

        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000";

        // send write data
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(lp_data)
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, Some("")).await;
        (client, test_server)
    }

    #[tokio::test]
    async fn test_query_pretty() {
        let (client, test_server) = setup_test_data().await;

        // send query data
        let response = client
            .get(&format!(
                "{}/api/v3/query?d=MyOrg_MyBucket&q={}",
                test_server.url(),
                "select%20*%20from%20h2o_temperature"
            ))
            .send()
            .await;

        assert_eq!(get_content_type(&response), "text/plain");

        let expected = r#"+----------------+--------------+-------+-----------------+----------------------+
| bottom_degrees | location     | state | surface_degrees | time                 |
+----------------+--------------+-------+-----------------+----------------------+
| 50.4           | santa_monica | CA    | 65.2            | 2021-04-01T14:10:24Z |
+----------------+--------------+-------+-----------------+----------------------+"#;

        check_response("query", response, StatusCode::OK, Some(expected)).await;

        // same response is expected if we explicitly request 'format=pretty'
        let response = client
            .get(&format!(
                "{}/api/v3/query?d=MyOrg_MyBucket&q={}&format=pretty",
                test_server.url(),
                "select%20*%20from%20h2o_temperature"
            ))
            .send()
            .await;
        assert_eq!(get_content_type(&response), "text/plain");

        check_response("query", response, StatusCode::OK, Some(expected)).await;
    }

    #[tokio::test]
    async fn test_query_csv() {
        let (client, test_server) = setup_test_data().await;

        // send query data
        let response = client
            .get(&format!(
                "{}/api/v3/query?d=MyOrg_MyBucket&q={}&format=csv",
                test_server.url(),
                "select%20*%20from%20h2o_temperature"
            ))
            .send()
            .await;

        assert_eq!(get_content_type(&response), "text/csv");

        let res = "bottom_degrees,location,state,surface_degrees,time\n\
                   50.4,santa_monica,CA,65.2,2021-04-01T14:10:24.000000000\n";
        check_response("query", response, StatusCode::OK, Some(res)).await;
    }

    #[tokio::test]
    async fn test_query_json() {
        let (client, test_server) = setup_test_data().await;

        // send a second line of data to demonstrate how that works
        let lp_data =
            "h2o_temperature,location=Boston,state=MA surface_degrees=50.2 1617286224000000000";

        // send write data
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(lp_data)
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, Some("")).await;

        // send query data
        let response = client
            .get(&format!(
                "{}/api/v3/query?d=MyOrg_MyBucket&q={}&format=json",
                test_server.url(),
                "select%20*%20from%20h2o_temperature%20order%20by%20surface_degrees"
            ))
            .send()
            .await;

        assert_eq!(get_content_type(&response), "application/json");

        // Note two json records: one record on each line
        let res = r#"[{"location":"Boston","state":"MA","surface_degrees":50.2,"time":"2021-04-01 14:10:24"},{"bottom_degrees":50.4,"location":"santa_monica","state":"CA","surface_degrees":65.2,"time":"2021-04-01 14:10:24"}]"#;
        check_response("query", response, StatusCode::OK, Some(res)).await;
    }

    #[tokio::test]
    async fn test_query_invalid_name() {
        let (client, test_server) = setup_test_data().await;

        // send query data
        let response = client
            .get(&format!(
                "{}/api/v3/query?d=&q={}",
                test_server.url(),
                "select%20*%20from%20h2o_temperature%20order%20by%20surface_degrees"
            ))
            .send()
            .await;

        check_response(
            "query",
            response,
            StatusCode::BAD_REQUEST,
            Some(r#""error_code":101"#),
        )
        .await;
    }

    fn gzip_str(s: &str) -> Vec<u8> {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        write!(encoder, "{}", s).expect("writing into encoder");
        encoder.finish().expect("successfully encoding gzip data")
    }

    #[tokio::test]
    async fn test_gzip_write() {
        let test_server = setup_server().await;

        let client = Client::new();
        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000";

        // send write data encoded with gzip
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .header(CONTENT_ENCODING, "gzip")
            .body(gzip_str(lp_data))
            .send()
            .await;

        check_response("gzip_write", response, StatusCode::NO_CONTENT, Some("")).await;

        // Check that the data got into the right bucket
        let test_db = test_server
            .server_type()
            .server
            .db(&DatabaseName::new("MyOrg_MyBucket").unwrap())
            .expect("Database exists");

        let batches = run_query(test_db, "select * from h2o_temperature").await;

        let expected = vec![
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| bottom_degrees | location     | state | surface_degrees | time                 |",
            "+----------------+--------------+-------+-----------------+----------------------+",
            "| 50.4           | santa_monica | CA    | 65.2            | 2021-04-01T14:10:24Z |",
            "+----------------+--------------+-------+-----------------+----------------------+",
        ];
        assert_batches_eq!(expected, &batches);
    }

    #[tokio::test]
    async fn write_to_invalid_database() {
        let test_server = setup_server().await;

        let client = Client::new();

        let bucket_name = "NotMyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body("cpu bar=1 10")
            .send()
            .await;

        check_response(
            "write_to_invalid_databases",
            response,
            StatusCode::NOT_FOUND,
            Some(""),
        )
        .await;
    }

    /// Run the specified SQL query and return formatted results as a string
    async fn run_query(db: Arc<Db>, query: &str) -> Vec<RecordBatch> {
        let ctx = db.new_query_context(None);
        let physical_plan = Planner::new(&ctx).sql(query).await.unwrap();

        ctx.collect(physical_plan).await.unwrap()
    }

    /// return a test server and the url to contact it for `MyOrg_MyBucket`
    async fn setup_server() -> TestServer<DatabaseServerType<ConnectionManagerImpl>> {
        let application = make_application();

        let app_server = make_server(Arc::clone(&application));
        app_server.set_id(ServerId::try_from(1).unwrap()).unwrap();
        app_server.wait_for_init().await.unwrap();
        app_server
            .create_database(make_rules("MyOrg_MyBucket"))
            .await
            .unwrap();

        let server_type =
            DatabaseServerType::new(application, app_server, &CommonServerState::for_testing());

        TestServer::new(Arc::new(server_type))
    }

    fn make_rules(db_name: impl Into<String>) -> ProvidedDatabaseRules {
        let db_name = DatabaseName::new(db_name.into()).unwrap();
        ProvidedDatabaseRules::new_rules(DatabaseRules::new(db_name).into())
            .expect("Tests should create valid DatabaseRules")
    }
}
