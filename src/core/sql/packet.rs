//! packet.rs
//! core::sql::packet
//!
//! Packet framing on the connection (sequence check, 0xFFFFFF fragment reassembly) and the payload readers.
//! A reassembled payload stops at 256 MiB; an outgoing packet is never split.
//!

use super::connection::Connection;
use super::error::MysqlError;
use std::io::{Read, Write};

// 패킷 조각 분할 경계
pub(super) const FRAGMENT_BYTES: usize = 0x00FF_FFFF;
// 재조립 본문 상한
const MAX_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

// 1. Packet framing -----------------------------------------------------------
impl Connection {
    pub(super) fn read_packet(&mut self) -> Result<Vec<u8>, MysqlError> {
        let mut payload = Vec::new();
        loop {
            // 1. 헤더 및 순번
            let mut header = [0u8; 4];
            self.reader.read_exact(&mut header)?;
            let fragment_len = usize::from(header[0])
                | (usize::from(header[1]) << 8)
                | (usize::from(header[2]) << 16);
            if header[3] != self.sequence {
                return Err(MysqlError::Protocol("packet sequence mismatch".into()));
            }
            self.sequence = self.sequence.wrapping_add(1);

            // 2. 조각 본문
            let fragment_start = payload.len();
            if fragment_start + fragment_len > MAX_PAYLOAD_BYTES {
                return Err(MysqlError::Protocol(
                    "packet exceeds the 256 MiB limit".into(),
                ));
            }
            payload.resize(fragment_start + fragment_len, 0);
            self.reader.read_exact(&mut payload[fragment_start..])?;

            // 3. 마지막 조각
            if fragment_len < FRAGMENT_BYTES {
                return Ok(payload);
            }
        }
    }
    // 분할 전송 미지원
    pub(super) fn write_packet(&mut self, head: &[u8], body: &[u8]) -> Result<(), MysqlError> {
        let payload_len = head.len() + body.len();
        if payload_len >= FRAGMENT_BYTES {
            return Err(MysqlError::Protocol(
                "outgoing packet reaches the 16 MiB limit".into(),
            ));
        }
        let mut frame = Vec::with_capacity(payload_len + 4);
        frame.extend_from_slice(&(payload_len as u32).to_le_bytes()[..3]);
        frame.push(self.sequence);
        frame.extend_from_slice(head);
        frame.extend_from_slice(body);
        self.sequence = self.sequence.wrapping_add(1);
        self.reader.get_mut().write_all(&frame)?;
        Ok(())
    }
}

// 2. Payload readers ----------------------------------------------------------
pub(super) fn take_bytes<'a>(input: &mut &'a [u8], count: u64) -> Result<&'a [u8], MysqlError> {
    let split = usize::try_from(count)
        .ok()
        .and_then(|count| input.split_at_checked(count));
    let Some((taken, rest)) = split else {
        return Err(MysqlError::Protocol(
            "packet shorter than its declared fields".into(),
        ));
    };
    *input = rest;
    Ok(taken)
}
// 리틀엔디언 고정폭 정수
pub(super) fn take_uint(input: &mut &[u8], width: u64) -> Result<u64, MysqlError> {
    let bytes = take_bytes(input, width)?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0, |total, byte| (total << 8) | u64::from(*byte)))
}
pub(super) fn take_lenenc_int(input: &mut &[u8]) -> Result<u64, MysqlError> {
    match take_uint(input, 1)? {
        short @ 0..=0xFA => Ok(short),
        0xFC => take_uint(input, 2),
        0xFD => take_uint(input, 3),
        0xFE => take_uint(input, 8),
        _ => Err(MysqlError::Protocol(
            "invalid length-encoded integer".into(),
        )),
    }
}
// NUL 부재 시 끝까지
pub(super) fn take_nul_bytes<'a>(input: &mut &'a [u8]) -> &'a [u8] {
    let whole: &'a [u8] = input;
    let end = whole
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(whole.len());
    let (text, rest) = whole.split_at(end);
    *input = rest.get(1..).unwrap_or_default();
    text
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::{OK_PAYLOAD, UNLIMITED, build_greeting, build_result_payloads};
    use super::super::fake_server::{frame_packets, spawn_fake_server};
    use super::super::handshake::NATIVE_PLUGIN;
    use super::super::types::StatementOutcome;
    use super::*;

    #[test]
    fn reads_length_encoded_fields() {
        let mut input: &[u8] = &[
            0xFA, 0xFC, 0x01, 0x02, 0xFD, 0x01, 0x02, 0x03, 0xFE, 1, 0, 0, 0, 0, 0, 0, 0x80, b'a',
            b'b', 0, b'c',
        ];
        assert_eq!(take_lenenc_int(&mut input).unwrap(), 0xFA);
        assert_eq!(take_lenenc_int(&mut input).unwrap(), 0x0201);
        assert_eq!(take_lenenc_int(&mut input).unwrap(), 0x03_0201);
        assert_eq!(take_lenenc_int(&mut input).unwrap(), 0x8000_0000_0000_0001);
        assert_eq!(take_nul_bytes(&mut input), b"ab");
        assert_eq!(take_nul_bytes(&mut input), b"c");
        assert!(input.is_empty());
        assert!(matches!(
            take_uint(&mut input, 1),
            Err(MysqlError::Protocol(_))
        ));

        // 잘린 정수, 무효 접두, 과대 길이
        for malformed in [&[0xFC, 0x01][..], &[0xFF][..]] {
            let mut input = malformed;
            assert!(take_lenenc_int(&mut input).is_err());
        }
        let mut input: &[u8] = &[1, 2];
        assert!(take_bytes(&mut input, u64::MAX).is_err());
        assert_eq!(input, [1, 2]);
    }
    #[test]
    fn reassembles_split_payload() {
        // 본문 0xFFFFFF 바이트 (빈 종결 조각)
        let wide_cell = vec![b'a'; FRAGMENT_BYTES - 9];
        let rows: [Vec<Option<&[u8]>>; 2] = [vec![Some(&wide_cell)], vec![Some(b"tail")]];
        let replies = vec![
            frame_packets(2, &[OK_PAYLOAD.to_vec()]),
            frame_packets(1, &build_result_payloads(&["body"], &rows, 2)),
        ];
        let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
        let mut connection = Connection::connect(&options).unwrap();
        let outcome = connection.run_statement("SELECT body", &UNLIMITED).unwrap();
        let StatementOutcome::Rows(result_set) = outcome else {
            panic!("rows expected");
        };
        assert_eq!(result_set.rows.len(), 2);
        assert!(result_set.rows[0][0].as_deref() == Some(wide_cell.as_slice()));
        assert_eq!(result_set.rows[1][0].as_deref(), Some(&b"tail"[..]));
        assert!(connection.is_reusable());
        drop(connection);
        server.join().unwrap();
    }
}
