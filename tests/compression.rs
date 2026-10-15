//! HTTP compression on the API client: it asks for gzip and brotli, decodes what comes back,
//! and the 32 MiB answer cap still counts DECODED bytes (an inflate bomb is small on the wire).
//!
//! Everything talks to a local wiremock server on 127.0.0.1; nothing leaves the machine.

use miniz_oxide::deflate::compress_to_vec;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::net;

/// CRC-32 (the gzip trailer's check), bit by bit: only used on small test bodies.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

/// `data` as a gzip file: header, raw deflate, CRC-32 and length. `crc` is given so a
/// bomb's multi-megabyte body does not have to be summed in a debug build.
fn gzip(data: &[u8], crc: u32) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    out.extend(compress_to_vec(data, 1));
    out.extend(crc.to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

fn gzip_response(body: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-encoding", "gzip")
        .set_body_bytes(body)
}

fn peak_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[tokio::test]
async fn the_api_client_asks_for_gzip_and_brotli() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(200).set_body_string("plain"))
        .mount(&server)
        .await;
    let resp = net::client("ytmfast-test/0")
        .get(format!("{}/a", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(net::read_capped(resp, 1 << 20).await.unwrap(), b"plain");
    let reqs = server.received_requests().await.unwrap();
    let asked = reqs[0]
        .headers
        .get("accept-encoding")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        asked.contains("gzip") && asked.contains("br"),
        "Accept-Encoding was {asked:?}"
    );
}

#[tokio::test]
async fn a_gzip_answer_is_read_whole() {
    // About 3 MB of text-like data, the size of a player script.
    let script: Vec<u8> = (0..3_000_000u32)
        .map(|i| b"var a=function(b){return b+1};\n"[(i % 31) as usize])
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/base.js"))
        .respond_with(gzip_response(gzip(&script, crc32(&script))))
        .mount(&server)
        .await;
    let resp = net::client("ytmfast-test/0")
        .get(format!("{}/base.js", server.uri()))
        .send()
        .await
        .unwrap();
    let got = net::read_capped(resp, net::MAX_ANSWER).await.unwrap();
    assert_eq!(got.len(), script.len());
    assert!(got == script);
}

#[tokio::test]
async fn a_brotli_answer_is_read_whole() {
    use std::io::Write;
    let text: Vec<u8> = (0..500_000u32)
        .map(|i| b"{\"title\":\"a song\",\"id\":1234}\n"[(i % 29) as usize])
        .collect();
    let mut wire = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut wire, 4096, 5, 22);
        w.write_all(&text).unwrap();
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "br")
                .set_body_bytes(wire),
        )
        .mount(&server)
        .await;
    let resp = net::client("ytmfast-test/0")
        .get(format!("{}/b", server.uri()))
        .send()
        .await
        .unwrap();
    let got = net::read_capped(resp, net::MAX_ANSWER).await.unwrap();
    assert!(got == text);
}

#[tokio::test]
async fn an_inflate_bomb_is_refused_at_the_cap_on_the_decoded_bytes() {
    // 160 MiB of zeros is about 0.5 MiB gzipped: far under the cap on the wire, five times
    // over it once decoded.
    let bomb = vec![0u8; 160 << 20];
    let wire = gzip(&bomb, 0);
    assert!(
        wire.len() < 2 << 20,
        "the bomb is {} bytes on the wire",
        wire.len()
    );
    drop(bomb);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bomb"))
        .respond_with(gzip_response(wire))
        .mount(&server)
        .await;
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_kib();
    let resp = net::client("ytmfast-test/0")
        .get(format!("{}/bomb", server.uri()))
        .send()
        .await
        .unwrap();
    let out = net::read_capped(resp, net::MAX_ANSWER).await;
    let grew_mib = peak_kib().saturating_sub(before) / 1024;
    let err = match out {
        Ok(body) => panic!(
            "a body that inflates past the cap was read: {} bytes",
            body.len()
        ),
        Err(e) => e,
    };
    assert!(format!("{err:?}").contains("too large"), "{err:?}");
    // Refused near the 32 MiB cap (plus the buffer's regrowth), not after inflating it all.
    assert!(
        grew_mib < 110,
        "the engine held about {grew_mib} MiB while refusing it"
    );
}

#[tokio::test]
async fn the_download_client_does_not_decode() {
    // Audio is fetched as sent, with Range and length checks that want the file's own bytes.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(200).set_body_string("audio"))
        .mount(&server)
        .await;
    let resp = net::stream_client("ytmfast-test/0")
        .get(format!("{}/track", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"audio");
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].headers.get("accept-encoding").is_none());
}
