//! This module defines and handles the Minecraft protocol and communication.
//!
//! This is necessary to exchange data with the target servers that should be probed. We only care about the packets
//! related to the [ServerListPing](https://wiki.vg/Server_List_Ping) and therefore only implement that part of the
//! Minecraft protocol. The implementations may differ from the official Minecraft client implementation if the
//! observed outcome is the same and the result is reliable.

use std::io::Cursor;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

/// The internal error type for all errors related to the protocol communication
///
/// This includes errors with the expected packets, packet contents or encoding of the exchanged fields. Errors of the
/// underlying data layer (for Byte exchange) are wrapped from the underlying IO errors. Additionally, the internal
/// timeout limits also are covered as errors.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error occurred while reading or writing to the underlying byte stream.
    #[error("error reading or writing data: {0}")]
    Io(#[from] std::io::Error),
    /// The received packet is of an invalid length that we cannot process.
    #[error("illegal packet length")]
    IllegalPacketLength,
    /// The received VarInt cannot be correctly decoded (was formed incorrectly).
    #[error("invalid VarInt data")]
    InvalidVarInt,
    /// The received packet ID is not mapped to an expected packet.
    #[error("illegal packet ID: {actual} (expected {expected})")]
    IllegalPacketId { expected: usize, actual: usize },
    /// The JSON response of the status packet is incorrectly encoded (not UTF-8).
    #[error("invalid ServerListPing response body (invalid encoding)")]
    InvalidEncoding,
    /// An error occurred with the payload of a ping.
    #[error("mismatched payload value: {actual} (expected {expected})")]
    PayloadMismatch { expected: u64, actual: u64 },
}

/// State is the desired state that the connection should be in after the initial handshake.
#[derive(Clone, Copy)]
enum State {
    Status,
}

impl From<State> for usize {
    fn from(state: State) -> Self {
        match state {
            State::Status => 1,
        }
    }
}

/// Packets are network packets that are part of the protocol definition and identified by a context and ID.
trait Packet {
    /// Returns the defined ID of this network packet.
    fn get_packet_id() -> usize;
}

/// OutboundPackets are packets that are written and therefore have a fixed, specific packet ID.
trait OutboundPacket: Packet {
    /// Creates a new buffer with the data from this packet.
    async fn to_buffer(&self) -> Result<Vec<u8>, Error>;
}

/// InboundPackets are packets that are read and therefore are expected to be of a specific packet ID.
trait InboundPacket: Packet + Sized {
    /// Creates a new instance of this packet with the data from the buffer.
    async fn new_from_buffer(buffer: Vec<u8>) -> Result<Self, Error>;
}

/// This packet initiates the status request attempt and tells the server the details of the client.
///
/// The data in this packet can differ from the actual data that was used but will be considered by the server when
/// assembling the response. Therefore, this data should mirror what a normal client would send.
struct HandshakePacket {
    /// The pretended protocol version.
    protocol_version: isize,
    /// The pretended server address.
    server_address: String,
    /// The pretended server port.
    server_port: u16,
    /// The protocol state to initiate.
    next_state: State,
}

impl HandshakePacket {
    /// Creates a new [HandshakePacket] with the supplied client information.
    const fn new(protocol_version: isize, server_address: String, server_port: u16) -> Self {
        Self {
            protocol_version,
            server_address,
            server_port,
            next_state: State::Status,
        }
    }
}

impl Packet for HandshakePacket {
    fn get_packet_id() -> usize {
        0x00
    }
}

impl OutboundPacket for HandshakePacket {
    async fn to_buffer(&self) -> Result<Vec<u8>, Error> {
        let mut buffer = Cursor::new(Vec::<u8>::new());

        #[allow(clippy::cast_sign_loss)]
        buffer.write_varint(self.protocol_version as usize).await?;
        buffer.write_string(&self.server_address).await?;
        buffer.write_u16(self.server_port).await?;
        buffer.write_varint(self.next_state.into()).await?;

        Ok(buffer.into_inner())
    }
}

/// This packet will be sent after the [HandshakePacket] and requests the server metadata.
///
/// The packet can only be sent after the [HandshakePacket] and must be written before any status information can be
/// read, as this is the differentiator between the status and the ping sequence.
struct StatusRequestPacket;

impl StatusRequestPacket {
    /// Creates a new [StatusRequestPacket].
    const fn new() -> Self {
        Self
    }
}

impl Packet for StatusRequestPacket {
    fn get_packet_id() -> usize {
        0x00
    }
}

impl OutboundPacket for StatusRequestPacket {
    async fn to_buffer(&self) -> Result<Vec<u8>, Error> {
        Ok(Vec::new())
    }
}

/// This is the response for a specific [StatusRequestPacket] that contains all self-reported metadata.
///
/// This packet can be received only after a [StatusRequestPacket] and will not close the connection, allowing for a
/// ping sequence to be exchanged afterward.
struct StatusResponsePacket {
    /// The JSON response body that contains all self-reported server metadata.
    body: String,
}

impl Packet for StatusResponsePacket {
    fn get_packet_id() -> usize {
        0x00
    }
}

impl InboundPacket for StatusResponsePacket {
    async fn new_from_buffer(buffer: Vec<u8>) -> Result<Self, Error> {
        let mut reader = Cursor::new(buffer);

        let body = reader.read_string().await?;

        Ok(Self { body })
    }
}

/// This is the request for a specific [PongPacket] that can be used to measure the server ping.
///
/// This packet can be sent after a connection was established or the [StatusResponsePacket] was received. Initiating
/// the ping sequence will consume the connection after the [PongPacket] was received.
struct PingPacket {
    /// The arbitrary payload that will be returned from the server (to identify the corresponding request).
    payload: u64,
}

impl PingPacket {
    /// Creates a new [PingPacket] with the supplied payload.
    const fn new(payload: u64) -> Self {
        Self { payload }
    }
}

impl Packet for PingPacket {
    fn get_packet_id() -> usize {
        0x01
    }
}

impl OutboundPacket for PingPacket {
    async fn to_buffer(&self) -> Result<Vec<u8>, Error> {
        let mut buffer = Cursor::new(Vec::<u8>::new());

        buffer.write_u64(self.payload).await?;

        Ok(buffer.into_inner())
    }
}

/// This is the response to a specific [PingPacket] that can be used to measure the server ping.
///
/// This packet can be received after a corresponding [PingPacket] and will have the same payload as the request. This
/// also consumes the connection, ending the Server List Ping sequence.
struct PongPacket {
    /// The arbitrary payload that was sent from the client (to identify the corresponding response).
    payload: u64,
}

impl Packet for PongPacket {
    fn get_packet_id() -> usize {
        0x01
    }
}

impl InboundPacket for PongPacket {
    async fn new_from_buffer(buffer: Vec<u8>) -> Result<Self, Error> {
        let mut reader = Cursor::new(buffer);

        let payload = reader.read_u64().await?;

        Ok(Self { payload })
    }
}

/// AsyncReadPacket allows reading a specific [InboundPacket] from an [AsyncWrite].
///
/// Only [InboundPackets][InboundPacket] can be read as only those packets are received. There are additional
/// methods to read the data that is encoded in a Minecraft-specific manner. Their implementation is analogous to the
/// [write implementation][AsyncWritePacket].
trait AsyncReadPacket {
    /// Reads the supplied [InboundPacket] type from this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#Packet_format).
    async fn read_packet<T: InboundPacket + Send + Sync>(&mut self) -> Result<T, Error>;

    /// Reads a VarInt from this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#VarInt_and_VarLong).
    async fn read_varint(&mut self) -> Result<usize, Error>;

    /// Reads a String from this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#Type:String).
    async fn read_string(&mut self) -> Result<String, Error>;
}

impl<R: AsyncRead + Unpin + Send + Sync> AsyncReadPacket for R {
    async fn read_packet<T: InboundPacket + Send + Sync>(&mut self) -> Result<T, Error> {
        // extract the length of the packet and check for any following content
        let length = self.read_varint().await?;
        if length == 0 {
            return Err(Error::IllegalPacketLength);
        }

        // extract the encoded packet id and validate if it is expected
        let packet_id = self.read_varint().await?;
        let expected_packet_id = T::get_packet_id();
        if packet_id != expected_packet_id {
            return Err(Error::IllegalPacketId {
                expected: expected_packet_id,
                actual: packet_id,
            });
        }

        // read the remaining content of the packet into a new buffer
        let mut buffer = vec![0; length - 1];
        self.read_exact(&mut buffer).await?;

        // convert the received buffer into our expected packet
        T::new_from_buffer(buffer).await
    }

    async fn read_varint(&mut self) -> Result<usize, Error> {
        let mut read = 0;
        let mut result = 0;
        loop {
            let read_value = self.read_u8().await?;
            let value = read_value & 0b0111_1111;
            result |= (value as usize) << (7 * read);
            read += 1;
            if read > 5 {
                return Err(Error::InvalidVarInt);
            }
            if (read_value & 0b1000_0000) == 0 {
                return Ok(result);
            }
        }
    }

    async fn read_string(&mut self) -> Result<String, Error> {
        let length = self.read_varint().await?;

        let mut buffer = vec![0; length];
        self.read_exact(&mut buffer).await?;

        String::from_utf8(buffer).map_err(|_| Error::InvalidEncoding)
    }
}

/// AsyncWritePacket allows writing a specific [OutboundPacket] to an [AsyncWrite].
///
/// Only [OutboundPackets][OutboundPacket] can be written as only those packets are sent. There are additional
/// methods to write the data that is encoded in a Minecraft-specific manner. Their implementation is analogous to the
/// [read implementation][AsyncReadPacket].
trait AsyncWritePacket {
    /// Writes the supplied [OutboundPacket] onto this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#Packet_format).
    async fn write_packet<T: OutboundPacket + Send + Sync>(
        &mut self,
        packet: T,
    ) -> Result<(), Error>;

    /// Writes a VarInt onto this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#VarInt_and_VarLong).
    async fn write_varint(&mut self, int: usize) -> Result<(), Error>;

    /// Writes a String onto this object as described in the official
    /// [protocol documentation](https://wiki.vg/Protocol#Type:String).
    async fn write_string(&mut self, string: &str) -> Result<(), Error>;
}

impl<W: AsyncWrite + Unpin + Send + Sync> AsyncWritePacket for W {
    async fn write_packet<T: OutboundPacket + Send + Sync>(
        &mut self,
        packet: T,
    ) -> Result<(), Error> {
        // write the packet into a buffer and box it as a slice (sized)
        let packet_buffer = packet.to_buffer().await?;
        let raw_packet = packet_buffer.into_boxed_slice();

        // create a new buffer and write the packet onto it (to get the size)
        let mut buffer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        buffer.write_varint(T::get_packet_id()).await?;
        buffer.write_all(&raw_packet).await?;

        // write the length of the content (length frame encoder) and then the packet
        let inner = buffer.into_inner();
        self.write_varint(inner.len()).await?;
        self.write_all(&inner).await?;

        Ok(())
    }

    async fn write_varint(&mut self, int: usize) -> Result<(), Error> {
        let mut int = (int as u64) & 0xFFFF_FFFF;
        let mut written = 0;
        let mut buffer = [0; 5];
        loop {
            let temp = (int & 0b0111_1111) as u8;
            int >>= 7;
            if int != 0 {
                buffer[written] = temp | 0b1000_0000;
            } else {
                buffer[written] = temp;
            }
            written += 1;
            if int == 0 {
                break;
            }
        }
        self.write_all(&buffer[0..written]).await?;

        Ok(())
    }

    async fn write_string(&mut self, string: &str) -> Result<(), Error> {
        self.write_varint(string.len()).await?;
        self.write_all(string.as_bytes()).await?;

        Ok(())
    }
}

/// The necessary information that will be reported to the server on a handshake of the Server List Ping protocol.
pub struct HandshakeInfo {
    /// The intended protocol version.
    pub protocol_version: isize,
    /// The hostname to connect.
    pub hostname: String,
    /// The server port to connect.
    pub server_port: u16,
}

impl HandshakeInfo {
    pub const fn new(protocol_version: isize, server_address: String, server_port: u16) -> Self {
        Self {
            protocol_version,
            hostname: server_address,
            server_port,
        }
    }
}

/// Performs the status protocol exchange and returns the self-reported server status.
///
/// This sends the [Handshake][HandshakePacket] and the [StatusRequest][StatusRequestPacket] packet and awaits the
/// [StatusResponse][StatusResponsePacket] from the server. This response is in JSON and will not be interpreted by this
/// function. The connection is not consumed by this operation, and the protocol allows for pings to be exchanged after
/// the status has been returned.
pub async fn retrieve_status(
    stream: &mut TcpStream,
    info: &HandshakeInfo,
) -> Result<String, Error> {
    // create a new handshake packet and send it
    let handshake = HandshakePacket::new(
        info.protocol_version,
        info.hostname.clone(),
        info.server_port,
    );
    stream.write_packet(handshake).await?;

    // create a new status request packet and send it
    let request = StatusRequestPacket::new();
    stream.write_packet(request).await?;

    // await the response for the status request and read it
    let response: StatusResponsePacket = stream.read_packet().await?;

    Ok(response.body)
}

/// Performs the ping protocol exchange and records the duration it took.
///
/// This sends the [Ping][PingPacket] and awaits the response of the [Pong][PongPacket], while recording the time it
/// takes to get a response. From this recorded RTT (Round-Trip-Time) the latency is calculated by dividing this value
/// by two. This is the most accurate way to measure the ping we can use.
pub async fn execute_ping(stream: &mut TcpStream) -> Result<Duration, Error> {
    // create a new value for the payload (to distinguish it)
    let payload = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time since epoch could not be retrieved")
        .as_secs();

    // record the current time to get the round trip time
    let start = Instant::now();

    // create and send a new ping packet
    let ping_request = PingPacket::new(payload);
    stream.write_packet(ping_request).await?;

    // await the retrieval of the corresponding pong packet
    let ping_response: PongPacket = stream.read_packet().await?;

    // take the time for the response and divide it to get the latency
    let mut duration = start.elapsed();
    duration = duration.div_f32(2.0);

    // if the pong packet did not match, something unexpected happened with the server
    if ping_response.payload != payload {
        return Err(Error::PayloadMismatch {
            expected: payload,
            actual: ping_response.payload,
        });
    }

    Ok(duration)
}
