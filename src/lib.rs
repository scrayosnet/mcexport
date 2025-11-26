//! A Minecraft server Prometheus multi-target exporter.
//!
//! This application uses the [Multi-Target Exporter Pattern](https://prometheus.io/docs/guides/multi-target-exporter/)
//! to ping and process each target individually. This improves the error handling, as well as the individual timeouts
//! and overall efficiency. Different targets can be pinged with different protocol versions and varying timeouts
//! independent of each other. Therefore, errors within one ping operation cannot affect other targets and their
//! response and state can be returned directly.
//!
//! Prometheus is responsible for scheduling the probe requests as well as possible, and all configuration is performed
//! within Prometheus as well. Therefore, mcexport has no knowledge regarding the list of targets and their individual
//! timeouts. Instead, the targets are requested one-by-one and the appropriate timeout is taken from a specific header
//! in the request. The processing has to be interrupted some time earlier, to account for latency and handling.
//!
//! The protocol implementation is compatible with all Minecraft: Java Edition servers from version 1.7 up. It is
//! expected that the servers strictly follow the protocol specification and appropriately return the expected values
//! at a given time. If the servers fail to do so, the probe requests are marked as unsuccessful.

#![deny(clippy::all)]
#![forbid(unsafe_code)]

pub mod config;
mod ping;
mod probe;
pub mod protocol;
mod timeout;

use crate::config::AppState;
use crate::ping::{ProbeStatus, get_server_status};
use crate::probe::{
    ProbingInfo,
    ResolutionResult::{Plain, Srv},
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use axum::{Router, body::Body, routing::get};
use prometheus_client::registry::Registry;
use prometheus_client::{encoding::text::encode, metrics::family::Family, metrics::gauge::Gauge};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, atomic::AtomicU32, atomic::AtomicU64};
use tokio::{net::TcpListener, time::timeout};
use tower_http::trace::TraceLayer;
use tracing::{info, instrument, warn};

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
    TimeoutInvalid(#[source] timeout::Error, ProbingInfo),
    /// The supplied timeout expired before the response was there.
    #[error("scrape timeout expired while waiting for a response")]
    TimeoutExpired(ProbingInfo),
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
            let ping_duration = Family::<Vec<(String, String)>, Gauge<f64, AtomicU64>>::default();
            registry.register(
                "ping_duration_seconds",
                "The duration that's elapsed since the probe request",
                ping_duration.clone(),
            );
            ping_duration
                .get_or_create(&vec![])
                .set(self.ping.as_secs_f64());

            // srv record (Gauge)
            let address_srv = Family::<Vec<(String, String)>, Gauge>::default();
            registry.register(
                "address_srv_info",
                "Whether there was an SRV record for the hostname",
                address_srv.clone(),
            );
            address_srv.get_or_create(&vec![]).set(i64::from(self.srv));

            // srv record (Gauge)
            let address_srv = Family::<Vec<(String, String)>, Gauge>::default();
            registry.register(
                "address_ping_valid_info",
                "Whether the ping response contained the correct payload",
                address_srv.clone(),
            );
            address_srv
                .get_or_create(&vec![])
                .set(i64::from(self.valid));

            // online players (gauge)
            let players_online = Family::<Vec<(String, String)>, Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_online_total",
                "The number of players that are currently online",
                players_online.clone(),
            );
            players_online
                .get_or_create(&vec![])
                .set(self.status.players.online);

            // max players (gauge)
            let players_max = Family::<Vec<(String, String)>, Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_max_total",
                "The number of players that can join at maximum",
                players_max.clone(),
            );
            players_max
                .get_or_create(&vec![])
                .set(self.status.players.max);

            // sample players (gauge)
            let players_samples_count =
                Family::<Vec<(String, String)>, Gauge<u32, AtomicU32>>::default();
            registry.register(
                "players_samples_total",
                "The number of sample entries that have been sent",
                players_samples_count.clone(),
            );
            players_samples_count.get_or_create(&vec![]).set(
                self.status
                    .players
                    .sample
                    .map_or_else(|| 0_usize, |samples| samples.len()) as u32,
            );

            // protocol version (gauge)
            let protocol_version = Family::<Vec<(String, String)>, Gauge>::default();
            registry.register(
                "protocol_version_info",
                "The numeric network protocol version",
                protocol_version.clone(),
            );
            protocol_version
                .get_or_create(&vec![])
                .set(self.status.version.protocol);

            // protocol version (gauge)
            let mut hasher = DefaultHasher::new();
            self.status.version.name.hash(&mut hasher);
            let protocol_hash = Family::<Vec<(String, String)>, Gauge>::default();
            registry.register(
                "protocol_version_hash",
                "The numeric hash of the visual network protocol",
                protocol_hash.clone(),
            );
            protocol_hash
                .get_or_create(&vec![])
                .set(hasher.finish() as i64);

            // favicon bytes (gauge)
            let favicon_bytes = Family::<Vec<(String, String)>, Gauge<u32, AtomicU32>>::default();
            registry.register(
                "favicon_bytes",
                "The size of the favicon in bytes",
                favicon_bytes.clone(),
            );
            let size: usize = self.status.favicon.map_or(0, |icon| icon.len());
            favicon_bytes.get_or_create(&vec![]).set(size as u32);
        })
    }
}

/// Initializes the axum http server and creates all necessary resources for the operation.
///
/// This binds the server socket and starts the HTTP server to serve the probe requests of Prometheus. This also
/// configures the corresponding routes for the status and probe endpoint and makes them publicly available. The
/// prometheus registry and metrics are initialized and made ready for the first probe requests.
///
/// # Errors
///
/// Will return an appropriate error if the DNS resolver cannot get from the system configuration, the socket cannot
/// be bound to the supplied address, or the axum server cannot be properly initialized.
pub async fn start(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    // initialize the axum app with all routes
    let app = Router::new()
        .route("/", get(handle_root))
        .route("/probe", get(handle_probe))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // bind the socket address on all interfaces
    let addr = state.address;
    info!(addr = addr.to_string(), "binding socket address");
    let listener = TcpListener::bind(addr).await?;
    info!(addr = addr.to_string(), "successfully bound server socket");

    // serve the axum service on the bound socket address
    axum::serve(listener, app).await?;
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
async fn handle_root() -> Html<&'static str> {
    Html("mcexport - <a href=\"/probe\">Probe</a>")
}

/// Handles and answers all axum requests on the probing path "/probe".
///
/// This endpoint is invoked by Prometheus' probing requests and issues pings to the requested targets. Prometheus will
/// send [info][ProbingInfo] on the corresponding target, and this endpoint will answer with the status and metrics of
/// this ping operation. The ping is only started once the request comes in and Prometheus is responsible for scheduling
/// the requests to this endpoint regularly.
#[instrument(skip(state, info), fields(target = %info.target, module = %info.module.clone().unwrap_or_else(|| "-".to_owned())
))]
async fn handle_probe(
    headers: HeaderMap,
    Query(info): Query<ProbingInfo>,
    State(state): State<Arc<AppState>>,
) -> Result<ProbeStatus, Error> {
    // get the desired timeout for this request
    let timeout_duration =
        timeout::get_duration(&headers, state.timeout_fallback, state.timeout_offset)
            .map_err(|err| Error::TimeoutInvalid(err, info.clone()))?;

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
            Srv(addr) => get_server_status(&info, &addr, true, state.protocol_version).await,
            Plain(addr) => get_server_status(&info, &addr, false, state.protocol_version).await,
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
    let success = Family::<Vec<(String, String)>, Gauge>::default();
    registry.register(
        "success",
        "Whether the probe operation was successful",
        success.clone(),
    );

    // set the success status of this metric
    success.get_or_create(&vec![]).set(success_state);

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
