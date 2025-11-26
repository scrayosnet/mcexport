use criterion::{Criterion, criterion_group, criterion_main};
use mcexport::protocol::{HandshakeInfo, retrieve_status};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

// build a simple JSON status body of a typical small size
fn sample_status_json() -> String {
    // keep it reasonably small to exercise the fast paths
    "{\"version\":{\"name\":\"1.20.1\",\"protocol\":764},\"players\":{\"online\":5,\"max\":20,\"sample\":[{\"name\":\"abc\",\"id\":\"uuid\"}]},\"favicon\":null}".to_string()
}

// minimal VarInt encoding/decoding utilities for the bench driver side
fn write_varint(mut val: usize, out: &mut Vec<u8>) {
    loop {
        let mut temp = (val & 0b0111_1111) as u8;
        val >>= 7;
        if val != 0 {
            temp |= 0b1000_0000;
        }
        out.push(temp);
        if val == 0 {
            break;
        }
    }
}

async fn bench_roundtrip_once() {
    // create an in-memory full-duplex stream pair
    let (mut client_end, mut server_end) = duplex(16 * 1024);

    // prepare a simple server task that reads handshake + status request and responds with a status response
    let server = tokio::spawn(async move {
        // read first packet (handshake)
        // read VarInt length
        let mut len = 0usize;
        let mut num_read = 0u32;
        loop {
            let mut b = [0u8; 1];
            server_end.read_exact(&mut b).await.unwrap();
            let val = b[0] & 0b0111_1111;
            len |= (val as usize) << (7 * num_read);
            num_read += 1;
            if (b[0] & 0b1000_0000) == 0 {
                break;
            }
        }
        // read remaining bytes of handshake packet
        let mut buf = vec![0u8; len];
        server_end.read_exact(&mut buf).await.unwrap();
        // read next packet (status request)
        // length varint
        let mut len2 = 0usize;
        let mut num_read2 = 0u32;
        loop {
            let mut b = [0u8; 1];
            server_end.read_exact(&mut b).await.unwrap();
            let val = b[0] & 0b0111_1111;
            len2 |= (val as usize) << (7 * num_read2);
            num_read2 += 1;
            if (b[0] & 0b1000_0000) == 0 {
                break;
            }
        }
        let mut buf2 = vec![0u8; len2];
        server_end.read_exact(&mut buf2).await.unwrap();

        // write the StatusResponse packet
        let body = sample_status_json();
        let mut payload = Vec::with_capacity(5 + body.len());

        write_varint(0x00, &mut payload);
        // string (varint length + bytes)
        write_varint(body.len(), &mut payload);
        payload.extend_from_slice(body.as_bytes());

        // prepend packet length as VarInt
        let mut packet = Vec::with_capacity(payload.len() + 5);
        write_varint(payload.len(), &mut packet);
        packet.extend_from_slice(&payload);

        server_end.write_all(&packet).await.unwrap();
        server_end.flush().await.unwrap();
    });

    // client side: call retrieve_status using our public API
    let info = HandshakeInfo::new(764, "example.org".to_string(), 25565);
    let _json = retrieve_status(&mut client_end, &info).await.unwrap();

    server.await.unwrap();
}

fn benchmark_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("retrieve_status_roundtrip", |b| {
        b.to_async(&rt).iter(|| async {
            bench_roundtrip_once().await;
        })
    });
}

criterion_group!(benches, benchmark_roundtrip);
criterion_main!(benches);
