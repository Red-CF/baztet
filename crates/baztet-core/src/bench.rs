//! Startup / decode / snapshot benchmarks (honest ones: bazel itself
//! dominates wall-clock; these measure our process-level costs).

use std::time::{Duration, Instant};

use prost::Message;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone)]
pub struct BenchResult {
    pub decoded: u64,
    pub decode_duration: Duration,
    pub snapshot_roundtrip: Duration,
}

/// Stream-decode `n` synthetic `Build.Target` frames (phase-1 query shape),
/// then a 10k-target snapshot roundtrip.
pub async fn run(n: u64) -> anyhow::Result<BenchResult> {
    let t0 = Instant::now();
    let decoded = stream_decode(n as usize).await?;
    let decode_duration = t0.elapsed();
    let snapshot_roundtrip = bench_snapshot();
    let r#final = Instant::now();
    let _ = r#final;
    Ok(BenchResult { decoded: decoded as u64, decode_duration, snapshot_roundtrip })
}

/// Count framed messages from an in-process duplex stream.
async fn stream_decode(n: usize) -> anyhow::Result<usize> {
    let (writer_half, mut reader) = tokio::io::duplex(256 * 1024);
    let writer_task = tokio::spawn(async move {
        let mut writer = writer_half;
        let t = dummy_target();
        // encode fully in memory, then write (message sizes are tiny)
        let mut bytes = Vec::new();
        for _ in 0..n {
            if t.encode_length_delimited(&mut bytes).is_err() {
                break;
            }
        }
        use tokio::io::AsyncWriteExt;
        let _ = writer.write_all(&bytes).await;
    });
    let mut decoded = 0usize;
    let mut buf: Vec<u8> = vec![];
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        decoded += consume_frames(&mut buf);
    }
    writer_task.await?;
    Ok(decoded)
}

fn dummy_target() -> baztet_protos::blaze_query::Target {
    baztet_protos::blaze_query::Target::default()
}

/// Consume complete varint-prefixed frames; returns how many.
fn consume_frames(buf: &mut Vec<u8>) -> usize {
    use baztet_protos::blaze_query;
    let mut count = 0usize;
    let mut pos = 0usize;
    while let Some(mut len) = read_varint(buf, &mut pos) {
        if buf.len() < pos + len {
            break;
        }
        if blaze_query::Target::decode(&buf[pos..pos + len]).is_err() {
            break;
        }
        pos += len;
        count += 1;
        let _ = &mut len;
    }
    if pos > 0 {
        buf.drain(..pos);
    }
    count
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let mut len: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        len |= u64::from(byte & 0x7f) << shift;
        *pos += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    usize::try_from(len).ok()
}

fn bench_snapshot() -> Duration {
    let mut s = baztet_snapshot::Snapshot::new("/tmp".into());
    for i in 0..10_000u32 {
        let _ = s.graph.get_or_insert(format!("//pkg{i}:lib"), "java_library", None, vec![]);
    }
    let t0 = Instant::now();
    let bytes = s.to_bytes().unwrap();
    let back = baztet_snapshot::Snapshot::from_bytes(&bytes).unwrap();
    std::assert_eq!(back.graph.len(), 10_000);
    t0.elapsed()
}