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
    Ok(ProbeStatus {
        srv,
        ping,
        valid,
        status,
    })
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

    #[test]
    fn deserialize_without_optional_fields() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":765,\"name\":\"1.20.4\"},\"players\":{\"online\":0,\"max\":20},\"description\":\"test server\"}").expect("could not deserialize server status");

        assert_eq!("1.20.4", status.version.name);
        assert_eq!(765, status.version.protocol);
        assert_eq!(0, status.players.online);
        assert_eq!(20, status.players.max);
        assert_eq!(None, status.players.sample);
        assert_eq!(None, status.favicon);
    }

    #[test]
    fn deserialize_with_null_favicon() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":764,\"name\":\"1.20.1\"},\"players\":{\"online\":5,\"max\":100},\"description\":\"test\",\"favicon\":null}").expect("could not deserialize server status");

        assert_eq!("1.20.1", status.version.name);
        assert_eq!(764, status.version.protocol);
        assert_eq!(5, status.players.online);
        assert_eq!(100, status.players.max);
        assert_eq!(None, status.favicon);
    }

    #[test]
    fn deserialize_with_empty_player_sample() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":763,\"name\":\"1.20\"},\"players\":{\"online\":10,\"max\":50,\"sample\":[]},\"description\":\"empty sample\"}").expect("could not deserialize server status");

        assert_eq!("1.20", status.version.name);
        assert_eq!(763, status.version.protocol);
        assert_eq!(10, status.players.online);
        assert_eq!(50, status.players.max);
        assert_eq!(Some(vec![]), status.players.sample);
    }

    #[test]
    fn deserialize_with_max_players() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":766,\"name\":\"1.20.6\"},\"players\":{\"online\":999,\"max\":1000},\"description\":\"full server\"}").expect("could not deserialize server status");

        assert_eq!(999, status.players.online);
        assert_eq!(1000, status.players.max);
    }

    #[test]
    fn deserialize_with_multiple_player_samples() {
        let status: ServerStatus = from_str("{\"version\":{\"protocol\":764,\"name\":\"1.20.1\"},\"players\":{\"online\":3,\"max\":10,\"sample\":[{\"name\":\"player1\",\"id\":\"uuid1\"},{\"name\":\"player2\",\"id\":\"uuid2\"},{\"name\":\"player3\",\"id\":\"uuid3\"}]},\"description\":\"test\"}").expect("could not deserialize server status");

        let expected_sample = Some(vec![
            ServerPlayer {
                name: "player1".to_string(),
                id: "uuid1".to_string(),
            },
            ServerPlayer {
                name: "player2".to_string(),
                id: "uuid2".to_string(),
            },
            ServerPlayer {
                name: "player3".to_string(),
                id: "uuid3".to_string(),
            },
        ]);

        assert_eq!(3, status.players.online);
        assert_eq!(expected_sample, status.players.sample);
    }

    #[test]
    fn fail_deserialize_malformed_json() {
        let result: Result<ServerStatus, _> = from_str("{\"version\":{\"protocol\":764");
        assert!(result.is_err());
    }

    #[test]
    fn fail_deserialize_missing_required_fields() {
        let result: Result<ServerStatus, _> = from_str("{\"version\":{\"protocol\":764}}");
        assert!(result.is_err());
    }

    #[test]
    fn fail_deserialize_invalid_protocol_type() {
        let result: Result<ServerStatus, _> = from_str(
            "{\"version\":{\"protocol\":\"not a number\",\"name\":\"test\"},\"players\":{\"online\":0,\"max\":20},\"description\":\"test\"}",
        );
        assert!(result.is_err());
    }

    #[test]
    fn error_display_cannot_reach() {
        let error = Error::CannotReach;
        assert_eq!(error.to_string(), "failed to connect to the server");
    }

    #[test]
    fn error_display_invalid_json() {
        let json_err = serde_json::from_str::<ServerStatus>("invalid").unwrap_err();
        let error = Error::InvalidJson(json_err, "invalid".to_string());
        assert!(
            error
                .to_string()
                .contains("failed to parse illegal JSON response")
        );
        assert!(error.to_string().contains("invalid"));
    }

    #[test]
    fn error_display_illegal_protocol() {
        let error = Error::IllegalProtocol("abc".to_string());
        assert_eq!(error.to_string(), "illegal protocol version: abc");
    }

    #[tokio::test]
    async fn test_get_server_status_with_mock_connection() {
        use crate::protocol::HandshakeInfo;
        use tokio::io::{AsyncWriteExt, duplex};

        let (mut client, mut server) = duplex(4096);

        // Spawn a task to simulate server responses
        let server_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;

            // Read handshake packet
            let mut len_buf = [0u8; 10];
            let mut len = 0usize;
            let mut bytes_read = 0;
            loop {
                server
                    .read_exact(&mut len_buf[bytes_read..bytes_read + 1])
                    .await
                    .unwrap();
                let byte = len_buf[bytes_read];
                len |= ((byte & 0x7F) as usize) << (7 * bytes_read);
                bytes_read += 1;
                if (byte & 0x80) == 0 {
                    break;
                }
            }

            let mut packet_buf = vec![0u8; len];
            server.read_exact(&mut packet_buf).await.unwrap();

            // Read status request packet
            let mut len2 = 0usize;
            let mut bytes_read2 = 0;
            loop {
                let mut byte_buf = [0u8; 1];
                server.read_exact(&mut byte_buf).await.unwrap();
                let byte = byte_buf[0];
                len2 |= ((byte & 0x7F) as usize) << (7 * bytes_read2);
                bytes_read2 += 1;
                if (byte & 0x80) == 0 {
                    break;
                }
            }

            let mut packet_buf2 = vec![0u8; len2];
            server.read_exact(&mut packet_buf2).await.unwrap();

            // Send status response
            let response_json = "{\"version\":{\"protocol\":764,\"name\":\"1.20.1\"},\"players\":{\"online\":5,\"max\":20},\"description\":\"Test Server\"}";
            let mut response_payload = Vec::new();

            // Write packet ID (0x00)
            response_payload.push(0x00);

            // Write string length as VarInt
            let mut len = response_json.len();
            loop {
                let mut temp = (len & 0x7F) as u8;
                len >>= 7;
                if len != 0 {
                    temp |= 0x80;
                }
                response_payload.push(temp);
                if len == 0 {
                    break;
                }
            }

            // Write string bytes
            response_payload.extend_from_slice(response_json.as_bytes());

            // Write packet length as VarInt
            let mut packet_len = response_payload.len();
            let mut length_bytes = Vec::new();
            loop {
                let mut temp = (packet_len & 0x7F) as u8;
                packet_len >>= 7;
                if packet_len != 0 {
                    temp |= 0x80;
                }
                length_bytes.push(temp);
                if packet_len == 0 {
                    break;
                }
            }

            server.write_all(&length_bytes).await.unwrap();
            server.write_all(&response_payload).await.unwrap();
            server.flush().await.unwrap();

            // Read ping packet
            let mut ping_len = 0usize;
            let mut ping_bytes_read = 0;
            loop {
                let mut byte_buf = [0u8; 1];
                server.read_exact(&mut byte_buf).await.unwrap();
                let byte = byte_buf[0];
                ping_len |= ((byte & 0x7F) as usize) << (7 * ping_bytes_read);
                ping_bytes_read += 1;
                if (byte & 0x80) == 0 {
                    break;
                }
            }

            let mut ping_packet = vec![0u8; ping_len];
            server.read_exact(&mut ping_packet).await.unwrap();

            // Extract payload (skip packet ID byte, read u64)
            let payload = u64::from_be_bytes([
                ping_packet[1],
                ping_packet[2],
                ping_packet[3],
                ping_packet[4],
                ping_packet[5],
                ping_packet[6],
                ping_packet[7],
                ping_packet[8],
            ]);

            // Send pong response
            let mut pong_payload = Vec::new();
            pong_payload.push(0x01); // Pong packet ID
            pong_payload.extend_from_slice(&payload.to_be_bytes());

            // Write pong packet length
            let mut pong_len = pong_payload.len();
            let mut pong_length_bytes = Vec::new();
            loop {
                let mut temp = (pong_len & 0x7F) as u8;
                pong_len >>= 7;
                if pong_len != 0 {
                    temp |= 0x80;
                }
                pong_length_bytes.push(temp);
                if pong_len == 0 {
                    break;
                }
            }

            server.write_all(&pong_length_bytes).await.unwrap();
            server.write_all(&pong_payload).await.unwrap();
            server.flush().await.unwrap();
        });

        // Execute the ping protocol
        let handshake_info = HandshakeInfo::new(764, "test.server".to_string(), 25565);
        let status_result = protocol::retrieve_status(&mut client, &handshake_info).await;
        assert!(status_result.is_ok());

        let status_json = status_result.unwrap();
        let status: ServerStatus = from_str(&status_json).expect("could not deserialize");

        assert_eq!("1.20.1", status.version.name);
        assert_eq!(764, status.version.protocol);
        assert_eq!(5, status.players.online);
        assert_eq!(20, status.players.max);

        let (ping, valid) = protocol::execute_ping(&mut client).await.unwrap();
        assert!(valid);
        assert!(ping.as_millis() < 1000); // Should be very fast for in-memory connection

        server_task.await.unwrap();
    }
}
