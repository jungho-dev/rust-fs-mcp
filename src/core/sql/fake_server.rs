//! fake_server.rs
//! core::sql::fake_server
//!
//! Test-only loopback MySQL server that replays prepared replies and records the client requests.
//! Holds the packet builders and sample values the unit tests of this folder share.
//!

use super::handshake::{CLIENT_CONNECT_WITH_DB, CLIENT_LENENC_AUTH_DATA, REQUIRED_SERVER_FLAGS};
use super::packet::FRAGMENT_BYTES;
use super::types::{ConnectOptions, RowLimits, column_type};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) const TEST_NONCE: &[u8; 20] = b"abcdefghijklmnopqrst";
pub(super) const SWITCH_NONCE: &[u8; 20] = b"ABCDEFGHIJKLMNOPQRST";
pub(super) const TEST_PASSWORD: &str = "secret-pw";
// node:crypto 생성 1024비트 테스트 키
pub(super) const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\n\
    MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDnI+VuXgMZEbYClOpqIbN9FfcA\n\
    As82siH1PFE9pJGjpU6ONsb0cUlqwint4PqSsGMplY2fQFYlLOMNHucB2l2LbbKl\n\
    7bQmP0MK9sNQ3fbOnkkljxcbhH8i0/7yr4fTmyKdAYej+eyYHZospnCk/gJ9nkOd\n\
    OJhJFY6II3NwZOlfOwIDAQAB\n\
    -----END PUBLIC KEY-----\n";
pub(super) const OK_PAYLOAD: [u8; 7] = [0x00, 0, 0, 2, 0, 0, 0];
pub(super) const UNLIMITED: RowLimits = RowLimits {
    max_rows: usize::MAX,
    max_bytes: usize::MAX,
};

// 1. Shared test fixtures -----------------------------------------------------
// 연속 순번 프레임 (0xFFFFFF 분할 포함)
pub(super) fn frame_packets(first_sequence: u8, payloads: &[Vec<u8>]) -> Vec<u8> {
    let mut frames = Vec::new();
    let mut sequence = first_sequence;
    for payload in payloads {
        let mut rest = payload.as_slice();
        loop {
            let (fragment, tail) = rest.split_at(rest.len().min(FRAGMENT_BYTES));
            frames.extend_from_slice(&(fragment.len() as u32).to_le_bytes()[..3]);
            frames.push(sequence);
            frames.extend_from_slice(fragment);
            sequence = sequence.wrapping_add(1);
            rest = tail;
            if fragment.len() < FRAGMENT_BYTES {
                break;
            }
        }
    }
    frames
}
pub(super) fn build_greeting(plugin: &str) -> Vec<u8> {
    let capabilities = REQUIRED_SERVER_FLAGS | CLIENT_CONNECT_WITH_DB | CLIENT_LENENC_AUTH_DATA;
    let mut greeting = vec![10];
    greeting.extend_from_slice(b"8.0.0-fake\0");
    greeting.extend_from_slice(&77u32.to_le_bytes());
    greeting.extend_from_slice(&TEST_NONCE[..8]);
    greeting.push(0);
    greeting.extend_from_slice(&capabilities.to_le_bytes()[..2]);
    greeting.extend_from_slice(&[255, 2, 0]);
    greeting.extend_from_slice(&capabilities.to_le_bytes()[2..]);
    greeting.push(21);
    greeting.extend_from_slice(&[0; 10]);
    greeting.extend_from_slice(&TEST_NONCE[8..]);
    greeting.push(0);
    greeting.extend_from_slice(plugin.as_bytes());
    greeting.push(0);
    greeting
}
// 텍스트 결과 집합 패킷 본문 목록
pub(super) fn build_result_payloads(
    names: &[&str],
    rows: &[Vec<Option<&[u8]>>],
    last_status: u16,
) -> Vec<Vec<u8>> {
    let mut payloads = vec![vec![names.len() as u8]];
    for name in names {
        let mut definition = Vec::new();
        for field in ["def", "db", "t", "t", name, name] {
            definition.push(field.len() as u8);
            definition.extend_from_slice(field.as_bytes());
        }
        definition.extend_from_slice(&[0x0C, 45, 0, 0, 1, 0, 0]);
        definition.extend_from_slice(&[column_type::VAR_STRING, 0x80, 0x00, 31, 0, 0]);
        payloads.push(definition);
    }
    payloads.push(vec![0xFE, 0, 0, 2, 0]);
    for row in rows {
        let mut cells = Vec::new();
        for cell in row {
            match cell {
                None => cells.push(0xFB),
                Some(bytes) if bytes.len() < 251 => {
                    cells.push(bytes.len() as u8);
                    cells.extend_from_slice(bytes);
                }
                Some(bytes) => {
                    cells.push(0xFE);
                    cells.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    cells.extend_from_slice(bytes);
                }
            }
        }
        payloads.push(cells);
    }
    let mut last_payload = vec![0xFE, 0, 0];
    last_payload.extend_from_slice(&last_status.to_le_bytes());
    payloads.push(last_payload);
    payloads
}
// 준비 응답 루프백 서버
pub(super) fn spawn_fake_server(
    greeting: Vec<u8>,
    replies: Vec<Vec<u8>>,
) -> (ConnectOptions, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let options = ConnectOptions {
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        user: "tester".into(),
        password: TEST_PASSWORD.into(),
        database: Some("db".into()),
        connect_timeout: Duration::from_secs(5),
        io_timeout: Duration::from_secs(5),
    };
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut requests = Vec::new();
        stream.write_all(&frame_packets(0, &[greeting])).unwrap();
        for reply in replies {
            let mut header = [0u8; 4];
            if stream.read_exact(&mut header).is_err() {
                break;
            }
            let request_len = u32::from_le_bytes([header[0], header[1], header[2], 0]);
            let mut request = vec![0u8; request_len as usize];
            if stream.read_exact(&mut request).is_err() || stream.write_all(&reply).is_err() {
                break;
            }
            requests.push(request);
        }
        let mut trailing = Vec::new();
        let _ = stream.read_to_end(&mut trailing);
        requests.push(trailing);
        requests
    });
    (options, server)
}
