//! This module defines and handles the Minecraft protocol and communication.
//!
//! This is necessary to exchange data with the target servers that should be probed. We only care about the packets
//! related to the [ServerListPing][server-list-ping] and therefore only implement that part of the Minecraft protocol.
//! The implementations may differ from the official Minecraft client implementation if the observed outcome is the same
//! and the result is reliable.
//!
//! [server-list-ping]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Server_List_Ping

use std::io::Cursor;
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;
use tracing::{debug, instrument};

/// The internal error type for all errors related to the protocol communication.
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
    /// The received `VarInt` cannot be correctly decoded (was formed incorrectly).
    #[error("invalid VarInt data")]
    InvalidVarInt,
    /// The received packet ID is not mapped to an expected packet.
    #[error("illegal packet ID: {actual} (expected {expected})")]
    IllegalPacketId {
        /// The expected value that should be present.
        expected: usize,
        /// The actual value that was observed.
        actual: usize,
    },
    /// The JSON response of the status packet is incorrectly encoded (not UTF-8).
    #[error("invalid ServerListPing response body (invalid encoding)")]
    InvalidEncoding,
    /// The current time is before the unix epoch: should not happen.
    #[error("unix epoch could not be calculated: {0}")]
    TimeUnavailable(#[from] SystemTimeError),
}

/// State is the desired state that the connection should be in after the initial handshake.
#[derive(Clone, Copy, Debug)]
enum State {
    /// The status state that is used to query server information without connecting.
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

/// `OutboundPacket`s are packets that are written and therefore have a fixed, specific packet ID.
trait OutboundPacket: Packet {
    /// Writes the data from this packet into the supplied [`S`].
    async fn write_to_buffer<S>(&self, buffer: &mut S) -> Result<(), Error>
    where
        S: AsyncWrite + Unpin + Send + Sync;
}

/// `InboundPacket`s are packets that are read and therefore are expected to be of a specific packet ID.
trait InboundPacket: Packet + Sized {
    /// Creates a new instance of this packet with the data from the buffer.
    async fn new_from_buffer<S>(buffer: &mut S) -> Result<Self, Error>
    where
        S: AsyncRead + Unpin + Send + Sync;
}

/// This packet initiates the status request attempt and tells the server the details of the client.
///
/// The data in this packet can differ from the actual data that was used but will be considered by the server when
/// assembling the response. Therefore, this data should mirror what a normal client would send.
#[derive(Debug)]
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
    /// Creates a new [`HandshakePacket`] with the supplied client information.
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
    async fn write_to_buffer<S>(&self, buffer: &mut S) -> Result<(), Error>
    where
        S: AsyncWrite + Unpin + Send + Sync,
    {
        #[allow(clippy::cast_sign_loss)]
        buffer.write_varint(self.protocol_version as usize).await?;
        buffer.write_string(&self.server_address).await?;
        buffer.write_u16(self.server_port).await?;
        buffer.write_varint(self.next_state.into()).await?;

        Ok(())
    }
}

/// This packet will be sent after the [`HandshakePacket`] and requests the server metadata.
///
/// The packet can only be sent after the [`HandshakePacket`] and must be written before any status information can be
/// read, as this is the differentiator between the status and the ping sequence.
#[derive(Debug)]
struct StatusRequestPacket;

impl StatusRequestPacket {
    /// Creates a new [`StatusRequestPacket`].
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
    async fn write_to_buffer<S>(&self, _buffer: &mut S) -> Result<(), Error>
    where
        S: AsyncWrite + Unpin + Send + Sync,
    {
        Ok(())
    }
}

/// This is the response for a specific [`StatusRequestPacket`] that contains all self-reported metadata.
///
/// This packet can be received only after a [`StatusRequestPacket`] and will not close the connection, allowing for a
/// ping sequence to be exchanged afterward.
#[derive(Debug)]
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
    async fn new_from_buffer<S>(buffer: &mut S) -> Result<Self, Error>
    where
        S: AsyncRead + Unpin + Send + Sync,
    {
        let body = buffer.read_string().await?;

        Ok(Self { body })
    }
}

/// This is the request for a specific [`PongPacket`] that can be used to measure the server ping.
///
/// This packet can be sent after a connection was established or the [`StatusResponsePacket`] was received. Initiating
/// the ping sequence will consume the connection after the [`PongPacket`] was received.
#[derive(Debug)]
struct PingPacket {
    /// The arbitrary payload that will be returned from the server (to identify the corresponding request).
    payload: u64,
}

impl PingPacket {
    /// Creates a new [`PingPacket`] with the supplied payload.
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
    async fn write_to_buffer<S>(&self, buffer: &mut S) -> Result<(), Error>
    where
        S: AsyncWrite + Unpin + Send + Sync,
    {
        buffer.write_u64(self.payload).await?;

        Ok(())
    }
}

/// This is the response to a specific [`PingPacket`] that can be used to measure the server ping.
///
/// This packet can be received after a corresponding [`PingPacket`] and will have the same payload as the request. This
/// also consumes the connection, ending the Server List Ping sequence.
#[derive(Debug)]
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
    async fn new_from_buffer<S>(buffer: &mut S) -> Result<Self, Error>
    where
        S: AsyncRead + Unpin + Send + Sync,
    {
        let payload = buffer.read_u64().await?;

        Ok(Self { payload })
    }
}

/// `AsyncWritePacket` allows writing a specific [`OutboundPacket`] to an [`AsyncWrite`].
///
/// Only [`OutboundPacket`s](OutboundPacket) can be written as only those packets are sent. There are additional
/// methods to write the data that is encoded in a Minecraft-specific manner. Their implementation is analogous to the
/// [read implementation](AsyncReadPacket).
trait AsyncWritePacket {
    /// Writes the supplied [`OutboundPacket`] onto this object as described in the official
    /// [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#Packet_format
    async fn write_packet<T: OutboundPacket + Send + Sync>(
        &mut self,
        packet: T,
    ) -> Result<(), Error>;

    /// Writes a `VarInt` onto this object as described in the official [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#VarInt_and_VarLong
    async fn write_varint(&mut self, int: usize) -> Result<(), Error>;

    /// Writes a `String` onto this object as described in the official [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#Type:String
    async fn write_string(&mut self, string: &str) -> Result<(), Error>;
}

impl<W: AsyncWrite + Unpin + Send + Sync> AsyncWritePacket for W {
    async fn write_packet<T: OutboundPacket + Send + Sync>(
        &mut self,
        packet: T,
    ) -> Result<(), Error> {
        // create a new buffer and write the packet onto it (to get the size)
        let mut buffer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        buffer.write_varint(T::get_packet_id()).await?;
        packet.write_to_buffer(&mut buffer).await?;

        // write the length of the content (length frame encoder) and then the packet
        let inner = buffer.into_inner();
        self.write_varint(inner.len()).await?;
        self.write_all(&inner).await?;

        Ok(())
    }

    async fn write_varint(&mut self, value: usize) -> Result<(), Error> {
        let mut int = (value as u64) & 0xFFFF_FFFF;
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

/// `AsyncReadPacket` allows reading a specific [`InboundPacket`] from an [`AsyncWrite`].
///
/// Only [`InboundPacket`s](InboundPacket) can be read as only those packets are received. There are additional
/// methods to read the data that is encoded in a Minecraft-specific manner. Their implementation is analogous to the
/// [write implementation](AsyncWritePacket).
trait AsyncReadPacket {
    /// Reads the supplied [`InboundPacket`] type from this object as described in the official
    /// [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#Packet_format
    async fn read_packet<T: InboundPacket + Send + Sync>(&mut self) -> Result<T, Error>;

    /// Reads a `VarInt` from this object as described in the official [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#VarInt_and_VarLong
    async fn read_varint(&mut self) -> Result<usize, Error>;

    /// Reads a `String` from this object as described in the official [protocol documentation][protocol-doc].
    ///
    /// [protocol-doc]: https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Protocol#Type:String
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
        let mut buffer = self.take(length as u64);

        // convert the received buffer into our expected packet
        T::new_from_buffer(&mut buffer).await
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
    /// Creates a new [`HandshakeInfo`] with the supplied values for the server handshake.
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
/// This sends the [`Handshake`](HandshakePacket) and the [`StatusRequest`](StatusRequestPacket) packet and awaits the
/// [`StatusResponse`](StatusResponsePacket) from the server. This response is in JSON and will not be interpreted by
/// this function. The connection is not consumed by this operation, and the protocol allows for pings to be exchanged
/// after the status has been returned.
#[instrument(skip(stream, info), fields(protocol_version = ?info.protocol_version))]
pub async fn retrieve_status<S>(stream: &mut S, info: &HandshakeInfo) -> Result<String, Error>
where
    S: AsyncWrite + AsyncRead + Unpin + Send + Sync,
{
    // create a new handshake packet and send it
    let handshake = HandshakePacket::new(
        info.protocol_version,
        info.hostname.clone(),
        info.server_port,
    );
    debug!(packet = debug(&handshake), "sending handshake packet");
    stream.write_packet(handshake).await?;

    // create a new status request packet and send it
    let request = StatusRequestPacket::new();
    debug!(packet = debug(&request), "sending status request packet");
    stream.write_packet(request).await?;

    // await the response for the status request and read it
    debug!("awaiting and reading status response packet");
    let response: StatusResponsePacket = stream.read_packet().await?;
    debug!(packet = debug(&response), "received status response packet");

    debug!(
        packet = debug(&response),
        "received a status response packet"
    );
    Ok(response.body)
}

/// Performs the ping protocol exchange and records the duration it took.
///
/// This sends the [Ping][PingPacket] and awaits the response of the [Pong][PongPacket], while recording the time it
/// takes to get a response. From this recorded RTT (Round-Trip-Time) the latency is calculated by dividing this value
/// by two. This is the most accurate way to measure the ping we can use.
pub async fn execute_ping<S>(stream: &mut S) -> Result<(Duration, bool), Error>
where
    S: AsyncWrite + AsyncRead + Unpin + Send + Sync,
{
    // create a new value for the payload (to distinguish it)
    let payload = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|val| val.as_secs())
        .map_err(Error::TimeUnavailable)?;

    // record the current time to get the round trip time
    let start = Instant::now();

    // create and send a new ping packet
    let ping_request = PingPacket::new(payload);
    debug!(packet = debug(&ping_request), "sending ping packet");
    stream.write_packet(ping_request).await?;

    // await the retrieval of the corresponding pong packet
    debug!("awaiting and reading pong packet");
    let ping_response: PongPacket = stream.read_packet().await?;
    debug!(packet = debug(&ping_response), "received pong packet");

    // take the time for the response and divide it to get the latency
    let mut duration = start.elapsed();
    duration = duration.div_f32(2.0);

    // if the pong packet did not match, something unexpected happened with the server
    if ping_response.payload != payload {
        return Ok((duration, false))
    }

    Ok((duration, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn packet_ids_valid() {
        assert_eq!(HandshakePacket::get_packet_id(), 0x00);
        assert_eq!(StatusRequestPacket::get_packet_id(), 0x00);
        assert_eq!(StatusResponsePacket::get_packet_id(), 0x00);
        assert_eq!(PingPacket::get_packet_id(), 0x01);
        assert_eq!(PongPacket::get_packet_id(), 0x01);
    }

    #[tokio::test]
    async fn serialize_handshake() {
        let packet = HandshakePacket::new(13, "test".to_string(), 5);
        let mut packet_buffer = Cursor::new(Vec::<u8>::new());
        packet.write_to_buffer(&mut packet_buffer).await.unwrap();
        let mut buffer: Cursor<Vec<u8>> = Cursor::new(packet_buffer.into_inner());

        let version = buffer.read_varint().await.unwrap();
        assert_eq!(version as isize, packet.protocol_version);

        let server_address = buffer.read_string().await.unwrap();
        assert_eq!(server_address, packet.server_address);

        let server_port = buffer.read_u16().await.unwrap();
        assert_eq!(server_port, packet.server_port);

        let state = buffer.read_varint().await.unwrap();
        assert_eq!(state, usize::from(packet.next_state));

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn serialize_status_request() {
        let packet = StatusRequestPacket::new();
        let mut packet_buffer = Cursor::new(Vec::<u8>::new());
        packet.write_to_buffer(&mut packet_buffer).await.unwrap();
        let buffer: Cursor<Vec<u8>> = Cursor::new(packet_buffer.into_inner());

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn deserialize_status_response() {
        let body = "{\"some\": \"values\"}";

        let mut buffer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        buffer.write_string(body).await.unwrap();
        let mut read_buffer = Cursor::new(buffer.into_inner());
        let packet = StatusResponsePacket::new_from_buffer(&mut read_buffer)
            .await
            .unwrap();
        assert_eq!(packet.body, body);

        assert_eq!(
            read_buffer.position() as usize,
            read_buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn serialize_ping() {
        // write the packet into a buffer and box it as a slice (sized)
        let packet = PingPacket::new(17);
        let mut packet_buffer = Cursor::new(Vec::<u8>::new());
        packet.write_to_buffer(&mut packet_buffer).await.unwrap();
        let mut buffer: Cursor<Vec<u8>> = Cursor::new(packet_buffer.into_inner());

        let payload = buffer.read_u64().await.unwrap();
        assert_eq!(payload, packet.payload);

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn deserialize_pong() {
        let payload = 11u64;

        let mut buffer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        buffer.write_u64(payload).await.unwrap();
        let mut read_buffer = Cursor::new(buffer.into_inner());
        let packet = PongPacket::new_from_buffer(&mut read_buffer)
            .await
            .unwrap();
        assert_eq!(packet.payload, payload);

        assert_eq!(
            read_buffer.position() as usize,
            read_buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn write_packet() {
        let packet = PingPacket::new(17);
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_packet(packet).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        let length = buffer.read_varint().await.unwrap();
        assert_eq!(length, 9);

        let packet_id = buffer.read_varint().await.unwrap();
        assert_eq!(packet_id, 0x01);

        let payload = buffer.read_u64().await.unwrap();
        assert_eq!(payload, 17);

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn read_packet() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(9).await.unwrap();
        writer.write_varint(0x01).await.unwrap();
        writer.write_u64(11).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        let packet = buffer.read_packet::<PongPacket>().await.unwrap();
        assert_eq!(packet.payload, 11);

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_read_packet_illegal_packet_id() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(9).await.unwrap();
        writer.write_varint(0x00).await.unwrap();
        writer.write_u64(11).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        buffer.read_packet::<PongPacket>().await.unwrap();
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_read_packet_illegal_packet_length() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(0).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        buffer.read_packet::<PongPacket>().await.unwrap();
    }

    #[tokio::test]
    async fn write_varint() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(0usize).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let first_byte = buffer.read_u8().await.unwrap();
        assert_eq!(first_byte, 0b0000_0000);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(65usize).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let first_byte = buffer.read_u8().await.unwrap();
        assert_eq!(first_byte, 0b0100_0001);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(129usize).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let first_byte = buffer.read_u8().await.unwrap();
        let second_byte = buffer.read_u8().await.unwrap();
        assert_eq!(first_byte, 0b1000_0001);
        assert_eq!(second_byte, 0b0000_0001);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn read_varint() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_u8(0b0000_0000).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let number = buffer.read_varint().await.unwrap();
        assert_eq!(number, 0usize);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_u8(0b0100_0001).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let number = buffer.read_varint().await.unwrap();
        assert_eq!(number, 65usize);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_u8(0b1000_0001).await.unwrap();
        writer.write_u8(0b0000_0001).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        let number = buffer.read_varint().await.unwrap();
        assert_eq!(number, 129usize);
        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_read_varint_invalid() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_u8(0b1000_0000).await.unwrap();
        writer.write_u8(0b1000_0000).await.unwrap();
        writer.write_u8(0b1000_0000).await.unwrap();
        writer.write_u8(0b1000_0000).await.unwrap();
        writer.write_u8(0b1000_0000).await.unwrap();
        writer.write_u8(0b1000_0000).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        buffer.read_varint().await.unwrap();
    }

    #[tokio::test]
    async fn write_string() {
        let text = "test";

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_string(text).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        let length = buffer.read_varint().await.unwrap();
        assert_eq!(length, text.len());

        let mut content = vec![0; length];
        buffer.read_exact(&mut content).await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), text);

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    async fn read_string() {
        let text = "other";

        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(text.len()).await.unwrap();
        writer.write_all(text.as_bytes()).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());

        let content = buffer.read_string().await.unwrap();
        assert_eq!(content, text);

        assert_eq!(
            buffer.position() as usize,
            buffer.get_ref().len(),
            "There are remaining bytes in the buffer"
        );
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_read_string_illegal_encoding() {
        let mut writer: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        writer.write_varint(4).await.unwrap();
        writer.write_all(&[0u8, 159u8, 146u8, 150u8]).await.unwrap();
        let mut buffer = Cursor::new(writer.get_ref());
        buffer.read_string().await.unwrap();
    }
}
