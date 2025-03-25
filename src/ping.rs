//! This module defines and handles the network communication and data layer of Minecraft communication.
//!
//! While the protocol module handles the details of the communication protocol, the ping module handles the network
//! communication in terms of establishing a TCP connection to the server. This includes the initialization of the data
//! stream to the target without any specifics of the Minecraft protocol. The ping packet takes the result of the
//! protocol communication and wraps it into easily usable data structs.

use crate::probe::ProbingInfo;
use crate::protocol;
use crate::protocol::{HandshakeInfo, execute_ping, retrieve_status};
use serde::Deserialize;
use serde_json::from_str;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{debug, instrument};

/// The internal error type for all errors related to the network communication.
///
/// This includes errors with the IO involved in establishing a TCP connection or transferring bytes from mcexport to
/// a target server. Additionally, this also covers the errors that occur while parsing and interpreting the results
/// returned by the corresponding server, that don't have to do with the protocol itself.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// No connection could be initiated to the target server (it is not reachable).
    #[error("failed to connect to the server")]
    CannotReach,
    /// The supplied JSON responses of the server could not be parsed into valid JSON.
    #[error("failed to parse illegal JSON response: {0}: \"{1}\"")]
    InvalidJson(#[source] serde_json::error::Error, String),
    /// An error occurred while trying to perform the server handshake or ping.
    #[error("mismatch in communication protocol: {0}")]
    ProtocolMismatch(#[from] protocol::Error),
    /// An error occurred while parsing the protocol version (module).
    #[error("illegal protocol version: {0}")]
    IllegalProtocol(String),
}

/// The information on the protocol version of a server.
#[derive(Debug, Deserialize)]
pub struct ServerVersion {
    /// The textual protocol version to display this version visually.
    pub name: String,
    /// The numeric protocol version (for compatibility checking).
    pub protocol: i64,
}

/// The information on a single, sampled player entry.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct ServerPlayer {
    /// The visual name to display this player.
    pub name: String,
    /// The unique identifier to reference this player.
    pub id: String,
}

/// The information on the current, maximum and sampled players.
#[derive(Debug, Deserialize)]
pub struct ServerPlayers {
    /// The current number of players that are online at this moment.
    pub online: u32,
    /// The maximum number of players that can join (slots).
    pub max: u32,
    /// An optional list of player information samples (version hover).
    pub sample: Option<Vec<ServerPlayer>>,
}

/// The self-reported status of a pinged server with all public metadata.
#[derive(Debug, Deserialize)]
pub struct ServerStatus {
    /// The version and protocol information of the server.
    pub version: ServerVersion,
    /// The current, maximum and sampled players of the server.
    pub players: ServerPlayers,
    /// The optional favicon of the server.
    pub favicon: Option<String>,
}

/// The full status of the ping operation along with the resolution information and ping duration.
#[derive(Debug, Deserialize)]
pub struct ProbeStatus {
    /// Whether an SRV record was used to resolve the real address of the server.
    pub srv: bool,
    /// The latency to get information from one side to the other (RTT/2).
    pub ping: Duration,
    /// Whether the ping response was valid (contained the right payload).
    pub valid: bool,
    /// The self-reported status of the server.
    pub status: ServerStatus,
}

/// Requests the status of the supplied server and records the ping duration.
///
/// This opens a new [TCP stream][TcpStream] and performs a [Server List Ping][server-list-ping] exchange, including the
/// general handshake, status request, and ping protocol. If any error with the underlying connection or the
/// communication protocol is encountered, the request is interrupted immediately. The connection is closed after the
/// ping interaction. If the async interaction is stopped, the connection will also be terminated prematurely,
/// accounting for Prometheus' timeouts.
///
/// [server-list-ping]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Server_List_Ping
#[instrument(skip(info))]
pub async fn get_server_status(
    info: &ProbingInfo,
    addr: &SocketAddr,
    srv: bool,
    fallback_protocol_version: isize,
) -> Result<ProbeStatus, Error> {
    // create a new tcp stream to the target
    debug!("establishing connection to target server");
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|_| Error::CannotReach)?;

    // try to use specific, requested version and fall back to "recent"
    let protocol_version = match &info.module {
        Some(ver) => ver
            .parse()
            .map_err(|_| Error::IllegalProtocol(ver.clone()))?,
        None => fallback_protocol_version,
    };

    // retrieve the server status (general information)
    let handshake_info = HandshakeInfo::new(
        protocol_version,
        info.target.hostname.clone(),
        info.target.port,
    );
    debug!(
        protocol_version = protocol_version,
        "requesting server status"
    );
    let status_string = retrieve_status(&mut stream, &handshake_info).await?;
    let status: ServerStatus =
        from_str(&status_string).map_err(|err| Error::InvalidJson(err, status_string))?;

    // perform the server ping to measure duration
    debug!(
        protocol_version = protocol_version,
        "requesting server ping"
    );
    let (ping, valid) = execute_ping(&mut stream).await?;

    // wrap everything into a ping response
    Ok(ProbeStatus { srv, ping, valid, status })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_component_description() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":1337,\"name\":\"version 0\"},\"players\":{\"online\":4,\"max\":6,\"sample\":[{\"name\": \"cool\", \"id\": \"test\"}]},\"description\":{\"text\":\"description text\"},\"favicon\":\"favicon content\"}").expect("could not deserialize server status");
        let sample = Some(vec![ServerPlayer {
            name: "cool".to_string(),
            id: "test".to_string(),
        }]);
        let favicon = Some("favicon content".to_string());

        assert_eq!("version 0", status.version.name);
        assert_eq!(1337, status.version.protocol);
        assert_eq!(4, status.players.online);
        assert_eq!(6, status.players.max);
        assert_eq!(sample, status.players.sample);
        assert_eq!(favicon, status.favicon);
        assert_eq!(favicon, status.favicon);
    }

    #[test]
    fn deserialize_plain_description() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":1338,\"name\":\"version 1\"},\"players\":{\"online\":6,\"max\":8,\"sample\":[{\"name\": \"cooler\", \"id\": \"tests\"}]},\"description\":\"description text 2\",\"favicon\":\"favicon content 2\"}").expect("could not deserialize server status");
        let sample = Some(vec![ServerPlayer {
            name: "cooler".to_string(),
            id: "tests".to_string(),
        }]);
        let favicon = Some("favicon content 2".to_string());

        assert_eq!("version 1", status.version.name);
        assert_eq!(1338, status.version.protocol);
        assert_eq!(6, status.players.online);
        assert_eq!(8, status.players.max);
        assert_eq!(sample, status.players.sample);
        assert_eq!(favicon, status.favicon);
    }
}
