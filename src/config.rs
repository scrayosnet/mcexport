//! This module defines and handles the configuration and settings for mcexport.
//!
//! The configuration values have an impact on the responses and the behavior of mcexport and come with their own
//! defaults, allowing for startup with zero explicit configuration. Instead, all configuration is implicit, falling
//! back to sensible defaults. The configuration is also used as the immutable state of the application within axum,
//! so that all tasks can use the supplied values.

use hickory_resolver::TokioAsyncResolver;
use std::net::SocketAddr;

/// The (at the time of the last release) most recent, supported protocol version.
///
/// As per the [Server List Ping][server-list-ping] documentation, it is a convention to use `-1` to determine the
/// version of the server. It would therefore be ideal to use this value. However, it turned out that very few servers
/// supported this convention, so we just go with the most recent version, which showed better results in all cases.
///
/// [server-list-ping]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Server_List_Ping#Handshake
pub const LATEST_PROTOCOL_VERSION: isize = 769;

/// The default socket address touple and port that mcexport should listen on.
pub const DEFAULT_ADDRESS: ([u8; 4], u16) = ([0, 0, 0, 0], 10026);

/// The default timeout length that should be used if no specific header is set.
pub const DEFAULT_TIMEOUT_SECS: f64 = 120.0;

/// The default offset that should always be subtracted from the timeout to account for latency and other overheads.
pub const DEFAULT_TIMEOUT_OFFSET_SECS: f64 = 0.25;

/// `AppState` contains various, shared resources for the state of the application.
///
/// The state of the application can be shared across all requests to benefit from their caching, resource consumption
/// and configuration. The access is handled through axum, allowing for multiple threads to use the same resource
/// without any problems regarding thread safety.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The network address that should be used to bind the HTTP server for probing requests.
    pub address: SocketAddr,
    /// The timeout in fractional seconds that is used as a default value.
    pub timeout_fallback: f64,
    /// The offset of the timeout in fractional seconds that is always subtracted from the value.
    pub timeout_offset: f64,
    /// The protocol version that should be used as a fallback (if not explicitly defined).
    pub protocol_version: isize,
    /// The resolver that will be used to dynamically resolve all target addresses.
    pub resolver: TokioAsyncResolver,
}

impl AppState {
    /// Creates a new [`AppState`] from the supplied configuration parameters that can be used in axum.
    #[must_use]
    pub const fn new(
        address: SocketAddr,
        timeout_fallback: f64,
        timeout_offset: f64,
        protocol_version: isize,
        resolver: TokioAsyncResolver,
    ) -> Self {
        Self {
            address,
            timeout_fallback,
            timeout_offset,
            protocol_version,
            resolver,
        }
    }
}
