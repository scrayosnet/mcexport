use criterion::{Criterion, criterion_group, criterion_main};
use mcexport::protocol::{HandshakeInfo, execute_ping, retrieve_status};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

// Sample JSON responses of different sizes for retrieve_status benchmarks
fn small_status_json() -> String {
    r#"{"version":{"name":"1.20.1","protocol":764},"players":{"online":5,"max":20},"description":"Test Server"}"#.to_string()
}

fn medium_status_json() -> String {
    r#"{"version":{"name":"1.20.1","protocol":764},"players":{"online":15,"max":100,"sample":[{"name":"Player1","id":"550e8400-e29b-41d4-a716-446655440001"},{"name":"Player2","id":"550e8400-e29b-41d4-a716-446655440002"},{"name":"Player3","id":"550e8400-e29b-41d4-a716-446655440003"},{"name":"Player4","id":"550e8400-e29b-41d4-a716-446655440004"},{"name":"Player5","id":"550e8400-e29b-41d4-a716-446655440005"}]},"description":{"text":"A Medium Test Server","color":"gold"},"favicon":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}"#.to_string()
}

fn large_status_json() -> String {
    let mut players = Vec::new();
    for i in 0..20 {
        players.push(format!(
            r#"{{"name":"Player{}","id":"550e8400-e29b-41d4-a716-4466554400{:02}"}}"#,
            i, i
        ));
    }
    let players_str = players.join(",");

    format!(
        r#"{{"version":{{"name":"1.20.1","protocol":764}},"players":{{"online":50,"max":200,"sample":[{}]}},"description":{{"text":"A Large Test Server with Long Description","color":"aqua","bold":true,"extra":[{{"text":"Additional info line 1","color":"yellow"}},{{"text":"Additional info line 2","color":"green"}}]}},"favicon":"data:image/png;base64,{}"}}"#,
        players_str,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".repeat(10)
    )
}

// VarInt encoding helper
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

// Benchmark complete roundtrip with different JSON sizes
async fn bench_roundtrip_with_json(json_response: String) {
    let (mut client_end, mut server_end) = duplex(64 * 1024);

    let server = tokio::spawn(async move {
        // Read handshake packet
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
        let mut buf = vec![0u8; len];
        server_end.read_exact(&mut buf).await.unwrap();

        // Read status request packet
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

        // Write status response
        let body = json_response;
        let mut payload = Vec::with_capacity(5 + body.len());
        write_varint(0x00, &mut payload);
        write_varint(body.len(), &mut payload);
        payload.extend_from_slice(body.as_bytes());

        let mut packet = Vec::with_capacity(payload.len() + 5);
        write_varint(payload.len(), &mut packet);
        packet.extend_from_slice(&payload);

        server_end.write_all(&packet).await.unwrap();
        server_end.flush().await.unwrap();

        // Read ping packet
        let mut ping_len = 0usize;
        let mut ping_read = 0u32;
        loop {
            let mut b = [0u8; 1];
            server_end.read_exact(&mut b).await.unwrap();
            let val = b[0] & 0b0111_1111;
            ping_len |= (val as usize) << (7 * ping_read);
            ping_read += 1;
            if (b[0] & 0b1000_0000) == 0 {
                break;
            }
        }
        let mut ping_buf = vec![0u8; ping_len];
        server_end.read_exact(&mut ping_buf).await.unwrap();

        // Extract ping payload
        let payload = u64::from_be_bytes([
            ping_buf[1],
            ping_buf[2],
            ping_buf[3],
            ping_buf[4],
            ping_buf[5],
            ping_buf[6],
            ping_buf[7],
            ping_buf[8],
        ]);

        // Write pong response
        let mut pong_payload = Vec::with_capacity(10);
        write_varint(0x01, &mut pong_payload);
        pong_payload.extend_from_slice(&payload.to_be_bytes());

        let mut pong_packet = Vec::with_capacity(pong_payload.len() + 2);
        write_varint(pong_payload.len(), &mut pong_packet);
        pong_packet.extend_from_slice(&pong_payload);

        server_end.write_all(&pong_packet).await.unwrap();
        server_end.flush().await.unwrap();
    });

    let info = HandshakeInfo::new(764, "example.org".to_string(), 25565);
    let _json = retrieve_status(&mut client_end, &info).await.unwrap();
    let (_ping, _valid) = execute_ping(&mut client_end).await.unwrap();

    server.await.unwrap();
}

fn benchmark_full_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("full_roundtrip");

    group.bench_function("small_response", |b| {
        b.to_async(&rt).iter(|| async {
            bench_roundtrip_with_json(small_status_json()).await;
        });
    });

    group.bench_function("medium_response", |b| {
        b.to_async(&rt).iter(|| async {
            bench_roundtrip_with_json(medium_status_json()).await;
        });
    });

    group.bench_function("large_response", |b| {
        b.to_async(&rt).iter(|| async {
            bench_roundtrip_with_json(large_status_json()).await;
        });
    });

    group.finish();
}

// Benchmark status retrieval only (without ping)
async fn bench_status_only(json_response: String) {
    let (mut client_end, mut server_end) = duplex(64 * 1024);

    let server = tokio::spawn(async move {
        // Read handshake
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
        let mut buf = vec![0u8; len];
        server_end.read_exact(&mut buf).await.unwrap();

        // Read status request
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

        // Send status response
        let body = json_response;
        let mut payload = Vec::with_capacity(5 + body.len());
        write_varint(0x00, &mut payload);
        write_varint(body.len(), &mut payload);
        payload.extend_from_slice(body.as_bytes());

        let mut packet = Vec::with_capacity(payload.len() + 5);
        write_varint(payload.len(), &mut packet);
        packet.extend_from_slice(&payload);

        server_end.write_all(&packet).await.unwrap();
        server_end.flush().await.unwrap();
    });

    let info = HandshakeInfo::new(764, "example.org".to_string(), 25565);
    let _json = retrieve_status(&mut client_end, &info).await.unwrap();

    server.await.unwrap();
}

fn benchmark_status_retrieval(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("status_retrieval");

    group.bench_function("small_json", |b| {
        b.to_async(&rt).iter(|| async {
            bench_status_only(small_status_json()).await;
        });
    });

    group.bench_function("medium_json", |b| {
        b.to_async(&rt).iter(|| async {
            bench_status_only(medium_status_json()).await;
        });
    });

    group.bench_function("large_json", |b| {
        b.to_async(&rt).iter(|| async {
            bench_status_only(large_status_json()).await;
        });
    });

    group.finish();
}

// Benchmark ping execution only
async fn bench_ping_only() {
    let (mut client_end, mut server_end) = duplex(1024);

    let server = tokio::spawn(async move {
        // Read ping packet
        let mut ping_len = 0usize;
        let mut ping_read = 0u32;
        loop {
            let mut b = [0u8; 1];
            server_end.read_exact(&mut b).await.unwrap();
            let val = b[0] & 0b0111_1111;
            ping_len |= (val as usize) << (7 * ping_read);
            ping_read += 1;
            if (b[0] & 0b1000_0000) == 0 {
                break;
            }
        }
        let mut ping_buf = vec![0u8; ping_len];
        server_end.read_exact(&mut ping_buf).await.unwrap();

        // Extract payload
        let payload = u64::from_be_bytes([
            ping_buf[1],
            ping_buf[2],
            ping_buf[3],
            ping_buf[4],
            ping_buf[5],
            ping_buf[6],
            ping_buf[7],
            ping_buf[8],
        ]);

        // Send pong
        let mut pong_payload = Vec::with_capacity(10);
        write_varint(0x01, &mut pong_payload);
        pong_payload.extend_from_slice(&payload.to_be_bytes());

        let mut pong_packet = Vec::with_capacity(pong_payload.len() + 2);
        write_varint(pong_payload.len(), &mut pong_packet);
        pong_packet.extend_from_slice(&pong_payload);

        server_end.write_all(&pong_packet).await.unwrap();
        server_end.flush().await.unwrap();
    });

    let (_ping, _valid) = execute_ping(&mut client_end).await.unwrap();

    server.await.unwrap();
}

fn benchmark_ping_execution(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("ping_execution", |b| {
        b.to_async(&rt).iter(|| async {
            bench_ping_only().await;
        });
    });
}

criterion_group!(
    benches,
    benchmark_full_roundtrip,
    benchmark_status_retrieval,
    benchmark_ping_execution
);
criterion_main!(benches);
