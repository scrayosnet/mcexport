use clap::Parser;
use hickory_resolver::Resolver;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::prelude::*;

/// Arguments to configure this runtime of the application before it is started.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, env, default_value = "INFO")]
    log_level: LevelFilter,
    #[arg(long, env, default_value_t = SocketAddr::from(mcexport::config::DEFAULT_ADDRESS))]
    address: SocketAddr,
    #[arg(long, env, default_value_t = mcexport::config::DEFAULT_TIMEOUT_SECS)]
    timeout_fallback: f64,
    #[arg(long, env, default_value_t = mcexport::config::LATEST_PROTOCOL_VERSION)]
    protocol_version: isize,
    #[arg(long, env, default_value_t = mcexport::config::DEFAULT_TIMEOUT_OFFSET_SECS)]
    timeout_offset: f64,
}

/// Initializes the application and invokes mcexport.
///
/// This initializes the logging, aggregates configuration and starts the multithreaded tokio runtime. This is only a
/// thin-wrapper around the mcexport crate that supplies the necessary settings.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // parse the arguments and configuration
    let args = Args::parse();

    // initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_filter(args.log_level),
        )
        .init();

    // initialize the application state
    let state = Arc::new(mcexport::config::AppState::new(
        args.address,
        args.timeout_fallback,
        args.timeout_offset,
        args.protocol_version,
        Resolver::builder_tokio()?.build(),
    ));

    // run mcexport blocking
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { mcexport::start(state).await })
}
