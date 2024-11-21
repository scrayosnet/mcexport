mod ping;
mod probe;
mod protocol;

use crate::ping::{get_server_status, PingError, PingStatus};
use crate::probe::ResolutionResult::{Plain, Srv};
use crate::probe::{ProbeError, ProbeInfo};
use crate::protocol::ProtocolError;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use hickory_resolver::TokioAsyncResolver;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{info, instrument};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// MetricError is the public response error wrapper for all errors that can be relayed to the caller.
///
/// This wraps all errors of the child modules into their own error type, so that they can be forwarded to the caller
/// of the probe requests. Those errors occur while trying to assemble the response for a specific probe and are then
/// wrapped into a parent type so that they can be more easily traced.
#[derive(Error, Debug)]
pub enum MetricError {
    /// An error occurred while reading or writing to the underlying byte stream.
    #[error("failed to resolve the intended target address: {0}")]
    ProbeError(#[from] ProbeError),
    /// An error occurred while encoding, decoding or interpreting the Minecraft protocol.
    #[error("failed to interpret or encode protocol messages: {0}")]
    ProtocolError(#[from] ProtocolError),
    /// An error occurred while reading or writing to the underlying byte stream.
    #[error("failed to communicate with the target server: {0}")]
    PingError(#[from] PingError),
}

/// AppState contains various, shared resources for the state of the application.
///
/// The state of the application can be shared across all requests to benefit from their caching, resource consumption
/// and configuration. The access is handled through [Arc], allowing for multiple threads to use the same resource
/// without any problems regarding thread safety.
#[derive(Debug)]
pub struct AppState {
    pub resolver: TokioAsyncResolver,
}

impl IntoResponse for MetricError {
    fn into_response(self) -> Response {
        // notify in the log (as we don't see it otherwise)
        info!(cause = &self.to_string(), "failed to resolve the target");

        // respond with the unsuccessful metrics
        generate_metrics_response(0, |_| {})
    }
}

impl IntoResponse for PingStatus {
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
            address_srv
                .get_or_create(&())
                .set(if self.srv { 1 } else { 0 });

            let players_online = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_online_total",
                "The number of players that are currently online",
                players_online.clone(),
            );
            players_online
                .get_or_create(&())
                .set(self.status.players.online);

            let players_max = Family::<(), Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_max_total",
                "The number of players that can join at maximum",
                players_max.clone(),
            );
            players_max.get_or_create(&()).set(self.status.players.max);

            let players_samples_count = Family::<(), Gauge>::default();
            registry.register(
                "players_samples_total",
                "The number of sample entries that have been sent",
                players_samples_count.clone(),
            );
            players_samples_count.get_or_create(&()).set(
                self.status
                    .players
                    .sample
                    .map_or_else(|| 0usize, |s| s.len()) as i64,
            );

            let protocol_version = Family::<(), Gauge>::default();
            registry.register(
                "protocol_version_info",
                "The numeric network protocol version",
                protocol_version.clone(),
            );
            protocol_version
                .get_or_create(&())
                .set(self.status.version.protocol);

            let favicon_bytes = Family::<(), Gauge>::default();
            registry.register(
                "favicon_bytes",
                "The size of the favicon in bytes",
                favicon_bytes.clone(),
            );
            let size = match self.status.favicon {
                Some(icon) => icon.len(),
                None => 0,
            };
            favicon_bytes.get_or_create(&()).set(size as i64);
        })
    }
}

/// Initializes the application and creates all necessary resources for the operation.
///
/// This binds the server socket and starts the HTTP server to serve the probe requests of Prometheus. This also
/// configures the corresponding routes for the status and probe endpoint and makes them publicly available. The
/// prometheus registry and metrics are initialized and made ready for the first probe requests.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_filter(LevelFilter::INFO),
        )
        .init();

    // initialize the application state
    let state = AppState {
        resolver: TokioAsyncResolver::tokio_from_system_conf().expect("failed to get DNS resolver"),
    };

    // initialize the axum app with all routes
    let state = Arc::new(state);
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
    Html("mcexport – <a href=\"/probe\">Probe</a>").into_response()
}

/// Handles and answers all axum requests on the probing path "/probe".
///
/// This endpoint is invoked by Prometheus' probing requests and issues pings to the requested targets. Prometheus will
/// send [info][ProbeInfo] on the corresponding target, and this endpoint will answer with the status and metrics of
/// this ping operation. The ping is only started once the request comes in and Prometheus is responsible for scheduling
/// the requests to this endpoint regularly.
#[instrument]
async fn handle_probe(
    Query(info): Query<ProbeInfo>,
    State(state): State<Arc<AppState>>,
) -> Result<PingStatus, MetricError> {
    // try to resolve the real probe address
    let resolve_result = info.target.to_socket_addrs(&state.resolver).await?;

    // issue the status request
    let ping_response = match resolve_result {
        Srv(addr) => get_server_status(&info, &addr, true).await?,
        Plain(addr) => get_server_status(&info, &addr, false).await?,
    };

    Ok(ping_response)
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
