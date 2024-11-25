mod ping;
mod probe;
mod protocol;

use crate::ping::{get_server_status, ProbeStatus};
use crate::probe::ProbingInfo;
use crate::probe::ResolutionResult::{Plain, Srv};
use crate::TimeoutError::NegativeDuration;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::{ToStrError, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use hickory_resolver::TokioAsyncResolver;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::num::ParseFloatError;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tower_http::trace::TraceLayer;
use tracing::{info, instrument, warn};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, Layer};

/// The name of the request header from Prometheus, that sets the scrape timeout.
///
/// This header should always be present on requests from Prometheus and can contain floating point numbers. This time
/// is the overall scraping time allocated for this probe request. Therefore, the [TIMEOUT_OFFSET_SECS] is subtracted
/// from the time to account for network latency and other processing overheads, not part of the timeout.
const SCRAPE_TIMEOUT_HEADER: &str = "X-Prometheus-Scrape-Timeout-Seconds";

/// The default timeout length that should be used if no specific header is set.
const DEFAULT_TIMEOUT_SECS: f64 = 120.0;

/// The offset that should always be subtracted from the timeout to account for latency and other overheads.
const TIMEOUT_OFFSET_SECS: f64 = 0.25;

/// The public response error wrapper for all errors that can be relayed to the caller.
///
/// This wraps all errors of the child modules into their own error type, so that they can be forwarded to the caller
/// of the probe requests. Those errors occur while trying to assemble the response for a specific probe and are then
/// wrapped into a parent type so that they can be more easily traced.
#[derive(thiserror::Error, Debug)]
enum Error {
    /// An error occurred while reading or writing to the underlying byte stream.
    #[error("failed to resolve the intended target address: {0}")]
    ProbeFail(#[source] probe::Error, ProbingInfo),
    /// An error occurred while reading or writing to the underlying byte stream.
    #[error("failed to communicate with the target server: {0}")]
    PingFail(#[source] ping::Error, ProbingInfo),
    /// An error occurred while trying to read and parse the supplied timeout.
    #[error("failed to parse timeout header: {0}")]
    TimeoutInvalid(#[source] TimeoutError, ProbingInfo),
    /// The supplied timeout expired before the response was there.
    #[error("scrape timeout expired while waiting for a response")]
    TimeoutExpired(ProbingInfo),
}

/// The internal timeout error wrapper for all errors that occur while parsing the timeout.
#[derive(thiserror::Error, Debug)]
enum TimeoutError {
    /// An error occurred while trying to read the header value into an ASCII string.
    #[error("not all bytes are ascii printable: {0}")]
    InvalidChars(#[source] ToStrError),
    /// An error occurred while trying to parse the header value into a float.
    #[error("the value \"{1}\" could not be parsed into a float: {0}")]
    InvalidNumber(#[source] ParseFloatError, String),
    /// The calculated timeout value would end up negative with the offset.
    #[error("the resulting duration ended up being negative: {result:?} (original {original:?})")]
    NegativeDuration { original: f64, result: f64 },
}

/// AppState contains various, shared resources for the state of the application.
///
/// The state of the application can be shared across all requests to benefit from their caching, resource consumption
/// and configuration. The access is handled through [Arc], allowing for multiple threads to use the same resource
/// without any problems regarding thread safety.
#[derive(Debug)]
struct AppState {
    pub resolver: TokioAsyncResolver,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // notify in the log (as we don't see it otherwise)
        match &self {
            Self::ProbeFail(_, info)
            | Self::PingFail(_, info)
            | Self::TimeoutInvalid(_, info)
            | Self::TimeoutExpired(info) => warn!(
                cause = self.to_string(),
                target = info.target.to_string(),
                module = info.module,
                "failed to resolve the target",
            ),
        }

        // respond with the unsuccessful metrics
        generate_metrics_response(0, |_| {})
    }
}

impl IntoResponse for ProbeStatus {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn into_response(self) -> Response {
        // return the generated metrics
        generate_metrics_response(1, |registry| {
            // ping duration (gauge)
            let ping_duration = Family::<(), Gauge<f64, AtomicU64>>::default();
            registry.register(
                "ping_duration_seconds",
                "The duration that's elapsed since the probe request",
                ping_duration.clone(),
            );
            ping_duration
                .get_or_create(&())
                .set(self.ping.as_secs_f64());

            // srv record (Gauge)
            let address_srv = Family::<(), Gauge>::default();
            registry.register(
                "address_srv_info",
                "Whether there was an SRV record for the hostname",
                address_srv.clone(),
            );
            address_srv.get_or_create(&()).set(i64::from(self.srv));

            // online players (gauge)
            let players_online = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_online_total",
                "The number of players that are currently online",
                players_online.clone(),
            );
            players_online
                .get_or_create(&())
                .set(self.status.players.online);

            // max players (gauge)
            let players_max = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_max_total",
                "The number of players that can join at maximum",
                players_max.clone(),
            );
            players_max.get_or_create(&()).set(self.status.players.max);

            // sample players (gauge)
            let players_samples_count = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_samples_total",
                "The number of sample entries that have been sent",
                players_samples_count.clone(),
            );
            players_samples_count.get_or_create(&()).set(
                self.status
                    .players
                    .sample
                    .map_or_else(|| 0usize, |s| s.len()) as u32,
            );

            // protocol version (gauge)
            let protocol_version = Family::<(), Gauge>::default();
            registry.register(
                "protocol_version_info",
                "The numeric network protocol version",
                protocol_version.clone(),
            );
            protocol_version
                .get_or_create(&())
                .set(self.status.version.protocol);

            // protocol version (gauge)
            let mut hasher = DefaultHasher::new();
            self.status.version.name.hash(&mut hasher);
            let protocol_hash = Family::<(), Gauge>::default();
            registry.register(
                "protocol_version_hash",
                "The numeric hash of the visual network protocol",
                protocol_hash.clone(),
            );
            protocol_hash.get_or_create(&()).set(hasher.finish() as i64);

            // favicon bytes (gauge)
            let favicon_bytes = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "favicon_bytes",
                "The size of the favicon in bytes",
                favicon_bytes.clone(),
            );
            let size: usize = self.status.favicon.map_or(0, |icon| icon.len());
            favicon_bytes.get_or_create(&()).set(size as u32);
        })
    }
}

/// Initializes the application and creates all necessary resources for the operation.
///
/// This binds the server socket and starts the HTTP server to serve the probe requests of Prometheus. This also
/// configures the corresponding routes for the status and probe endpoint and makes them publicly available. The
/// prometheus registry and metrics are initialized and made ready for the first probe requests.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer().compact().with_filter(LevelFilter::INFO))
        .init();

    // initialize the application state
    let state = Arc::new(AppState {
        resolver: TokioAsyncResolver::tokio_from_system_conf().expect("failed to get DNS resolver"),
    });

    // initialize the axum app with all routes
    let app = Router::new()
        .route("/", get(handle_root))
        .route("/probe", get(handle_probe))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // bind the socket address on all interfaces
    let addr = SocketAddr::from(([0, 0, 0, 0], 10026));
    info!(addr = addr.to_string(), "binding socket address");
    let listener = TcpListener::bind(addr).await?;
    info!(addr = addr.to_string(), "successfully bound server socket");

    // serve the axum service on the bound socket address
    axum::serve(listener, app)
        .await
        .expect("failed to serve axum server");
    info!("http server stopped successfully");

    // exit with success
    Ok(())
}

/// Handles and answers all axum requests on the root path "/".
///
/// It statically returns "mcexport" so that it can be used as a startup probe in Kubernetes. That means that this
/// endpoint can be checked for its response: Once there is a response, this means that mcexport is successfully
/// initialized and probe requests may be received and handled. It can also be used as a liveness probe to check whether
/// mcexport is still running as expected. Since mcexport is stateless and has no permanent connections, this endpoint
/// can accurately reflect the readiness and liveness of mcexport.
#[instrument]
async fn handle_root() -> Response {
    Html("mcexport - <a href=\"/probe\">Probe</a>").into_response()
}

/// Handles and answers all axum requests on the probing path "/probe".
///
/// This endpoint is invoked by Prometheus' probing requests and issues pings to the requested targets. Prometheus will
/// send [info][ProbeInfo] on the corresponding target, and this endpoint will answer with the status and metrics of
/// this ping operation. The ping is only started once the request comes in and Prometheus is responsible for scheduling
/// the requests to this endpoint regularly.
#[instrument(skip(state, info), fields(target = %info.target, module = %info.module.clone().unwrap_or_else(|| "-".to_string())
))]
async fn handle_probe(
    headers: HeaderMap,
    Query(info): Query<ProbingInfo>,
    State(state): State<Arc<AppState>>,
) -> Result<ProbeStatus, Error> {
    // get the desired timeout for this request
    let timeout_duration =
        get_timeout_duration(&headers).map_err(|err| Error::TimeoutInvalid(err, info.clone()))?;

    // declare the overall work to perform
    let result = timeout(timeout_duration, async {
        // try to resolve the real probe address
        let resolve_result = info
            .target
            .to_socket_addrs(&state.resolver)
            .await
            .map_err(|err| Error::ProbeFail(err, info.clone()))?;

        // issue the status request
        let ping_response = match resolve_result {
            Srv(addr) => get_server_status(&info, &addr, true).await,
            Plain(addr) => get_server_status(&info, &addr, false).await,
        }
        .map_err(|err| Error::PingFail(err, info.clone()))?;

        Ok(ping_response)
    })
    .await;

    // handle timeout error and value return
    match result {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(Error::TimeoutExpired(info)),
    }
}

/// Generates a response for the supplied success state and optionally allows adding more metrics.
///
/// This initializes the [registry][Registry] for this request and registers the success metric with the supplied state
/// within it. Then the closure is called to add more dynamic metrics (if desired). Finally, the metrics are encoded
/// into a buffer and the response is created with the appropriate content type and status code.
fn generate_metrics_response<F>(success_state: i64, add_metrics: F) -> Response
where
    F: FnOnce(&mut Registry),
{
    // create a new registry with the appropriate prefix
    let mut registry = Registry::with_prefix("mcexport");

    // create and register the success metric
    let success = Family::<(), Gauge>::default();
    registry.register(
        "success",
        "Whether the probe operation was successful",
        success.clone(),
    );

    // set the success status of this metric
    success.get_or_create(&()).set(success_state);

    // add dynamic, desired metrics
    add_metrics(&mut registry);

    // create a new buffer to store the metrics in
    let mut buffer = String::new();

    // encode the metrics content into the buffer
    encode(&mut buffer, &registry).expect("failed to encode metrics into the buffer");

    // return a response of the metrics specification
    Response::builder()
        .status(StatusCode::OK)
        .header(
            CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
        .body(Body::from(buffer))
        .expect("failed to build success target response")
}

/// Returns the desired request timeout for the probe endpoint from the supplied headers.
///
/// If the [SCRAPE_TIMEOUT_HEADER] is set, the value is parsed into a floating point number, that is interpreted as
/// the duration in seconds, that will be allowed for the request. Should no such header exist, the
/// [DEFAULT_TIMEOUT_SECS] is used instead. The duration is then reduced by [TIMEOUT_OFFSET_SECS] to account for the
/// network latency and processing overhead that does not count into the timeout. This method never returns a negative
/// duration and instead returns a [NegativeDuration].
fn get_timeout_duration(headers: &HeaderMap) -> Result<Duration, TimeoutError> {
    // try to use the header value to parse the duration
    let header = headers.get(SCRAPE_TIMEOUT_HEADER);
    let timeout = if let Some(raw_value) = header {
        let value = raw_value.to_str().map_err(TimeoutError::InvalidChars)?;
        value
            .parse::<f64>()
            .map_err(|err| TimeoutError::InvalidNumber(err, value.to_string()))?
    } else {
        DEFAULT_TIMEOUT_SECS
    };

    // subtract the timeout offset
    let result = timeout - TIMEOUT_OFFSET_SECS;

    // check whether the duration would be positive
    if result < 0.0 {
        return Err(NegativeDuration {
            original: timeout,
            result,
        });
    }

    // wrap it into a duration object
    Ok(Duration::from_secs_f64(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    static ILLEGAL_VALUE_BYTES: [u8; 4] = [0x00u8, 0x30u8, 0x30u8, 0x00u8];

    #[test]
    fn get_timeout_duration_with_header() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("5"));

        assert_eq!(
            get_timeout_duration(&headers).unwrap(),
            Duration::from_secs_f64(5.0 - TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    fn get_timeout_duration_with_header_decimal() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("5.5"));

        assert_eq!(
            get_timeout_duration(&headers).unwrap(),
            Duration::from_secs_f64(5.5 - TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    #[should_panic]
    fn fail_get_timeout_duration_with_invalid_header_str() {
        let mut headers = HeaderMap::new();
        unsafe {
            headers.insert(
                SCRAPE_TIMEOUT_HEADER,
                HeaderValue::from_maybe_shared_unchecked(ILLEGAL_VALUE_BYTES.as_ref()),
            );
        }

        get_timeout_duration(&headers).unwrap();
    }

    #[test]
    #[should_panic]
    fn fail_get_timeout_duration_with_invalid_header_parse() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("invalid"));

        get_timeout_duration(&headers).unwrap();
    }

    #[test]
    fn get_timeout_duration_without_header() {
        let headers = HeaderMap::new();

        assert_eq!(
            get_timeout_duration(&headers).unwrap(),
            Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS - TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    #[should_panic]
    fn fail_get_timeout_duration_negative_original() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("-1"));

        get_timeout_duration(&headers).unwrap();
    }

    #[test]
    #[should_panic]
    fn fail_get_timeout_duration_negative_result() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("0"));

        get_timeout_duration(&headers).unwrap();
    }
}
