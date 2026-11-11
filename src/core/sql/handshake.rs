//! handshake.rs
//! core::sql::handshake
//!
//! Protocol-10 handshake, protocol 4.1 response, and the authentication exchange.
//! Auth plugins: `mysql_native_password` and `caching_sha2_password` (fast path and RSA full authentication).
//! No TLS, compression, LOCAL INFILE, or multi-result support: those capability flags are never sent.
//!

use super::connection::Connection;
use super::digest::{digest_sha1, digest_sha256};
use super::error::MysqlError;
use super::packet::{take_bytes, take_nul_bytes, take_uint};
use super::random::fill_random_bytes;
use super::rsa::{encrypt_oaep, parse_public_key};
use super::statement::parse_server_error;
use super::types::ConnectOptions;

pub(super) const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
pub(super) const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
pub(super) const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub(super) const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub(super) const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub(super) const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub(super) const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
// CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
pub(super) const CLIENT_LENENC_AUTH_DATA: u32 = 0x0020_0000;
pub(super) const REQUIRED_SERVER_FLAGS: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
// utf8mb4_unicode_ci
const CHARSET_UTF8MB4: u8 = 224;
// 서버 통지용 최대 패킷
pub(super) const MAX_PACKET_BYTES: u32 = 16 * 1024 * 1024;
const NONCE_BYTES: usize = 20;
const AUTH_ROUND_LIMIT: usize = 6;
pub(super) const NATIVE_PLUGIN: &str = "mysql_native_password";
pub(super) const SHA2_PLUGIN: &str = "caching_sha2_password";

// 1. Handshake and authentication ---------------------------------------------
pub(super) struct ServerGreeting {
    capabilities: u32,
    plugin: String,
    nonce: Vec<u8>,
}
impl Connection {
    pub(super) fn read_handshake(&mut self) -> Result<ServerGreeting, MysqlError> {
        // 1. 오류 응답 및 프로토콜 버전
        let payload = self.read_packet()?;
        let mut input = payload.as_slice();
        match take_uint(&mut input, 1)? {
            10 => {}
            0xFF => return Err(parse_server_error(&payload)),
            _ => {
                return Err(MysqlError::Protocol(
                    "unsupported handshake protocol version".into(),
                ));
            }
        }

        // 2. 서버 버전 및 연결 ID
        self.server_version = String::from_utf8_lossy(take_nul_bytes(&mut input)).into_owned();
        self.connection_id = take_uint(&mut input, 4)? as u32;

        // 3. 난수 전반부 및 능력 플래그
        let mut nonce = take_bytes(&mut input, 8)?.to_vec();
        take_bytes(&mut input, 1)?;
        let lower_flags = take_uint(&mut input, 2)?;
        take_bytes(&mut input, 3)?;
        let upper_flags = take_uint(&mut input, 2)?;
        let capabilities = (lower_flags | (upper_flags << 16)) as u32;
        if capabilities & REQUIRED_SERVER_FLAGS != REQUIRED_SERVER_FLAGS {
            return Err(MysqlError::Protocol(
                "server lacks protocol 4.1 plugin authentication".into(),
            ));
        }

        // 4. 난수 후반부 및 플러그인 이름
        let nonce_len = take_uint(&mut input, 1)?;
        take_bytes(&mut input, 10)?;
        let tail_len = nonce_len.saturating_sub(8).max(13);
        nonce.extend_from_slice(take_bytes(&mut input, tail_len)?);
        let plugin = String::from_utf8_lossy(take_nul_bytes(&mut input)).into_owned();
        Ok(ServerGreeting {
            capabilities,
            plugin,
            nonce,
        })
    }
    pub(super) fn send_handshake_response(
        &mut self,
        options: &ConnectOptions,
        greeting: &ServerGreeting,
    ) -> Result<(), MysqlError> {
        // 1. 이름 검증
        let database = options.database.as_deref().filter(|name| !name.is_empty());
        if options.user.contains('\0') || database.is_some_and(|name| name.contains('\0')) {
            return Err(MysqlError::Protocol(
                "user or database name contains a NUL byte".into(),
            ));
        }

        // 2. 능력 플래그
        let mut capabilities = CLIENT_LONG_PASSWORD
            | CLIENT_LONG_FLAG
            | CLIENT_PROTOCOL_41
            | CLIENT_TRANSACTIONS
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH;
        if database.is_some() {
            capabilities |= CLIENT_CONNECT_WITH_DB;
        }
        capabilities |= greeting.capabilities & CLIENT_LENENC_AUTH_DATA;

        // 3. 응답 패킷
        let auth_response =
            build_auth_response(&greeting.plugin, &options.password, &greeting.nonce)?;
        let mut packet = Vec::with_capacity(96 + options.user.len());
        packet.extend_from_slice(&capabilities.to_le_bytes());
        packet.extend_from_slice(&MAX_PACKET_BYTES.to_le_bytes());
        packet.push(CHARSET_UTF8MB4);
        packet.extend_from_slice(&[0; 23]);
        packet.extend_from_slice(options.user.as_bytes());
        packet.push(0);
        // 32바이트 이하 (lenenc 동일 표현)
        packet.push(auth_response.len() as u8);
        packet.extend_from_slice(&auth_response);
        if let Some(name) = database {
            packet.extend_from_slice(name.as_bytes());
            packet.push(0);
        }
        packet.extend_from_slice(greeting.plugin.as_bytes());
        packet.push(0);
        self.write_packet(&packet, &[])
    }
    pub(super) fn complete_authentication(
        &mut self,
        options: &ConnectOptions,
        greeting: ServerGreeting,
    ) -> Result<(), MysqlError> {
        let mut plugin = greeting.plugin;
        let mut nonce = greeting.nonce;
        for _ in 0..AUTH_ROUND_LIMIT {
            let payload = self.read_packet()?;
            match payload.split_first() {
                // 1. 인증 완료
                Some((0x00, _)) => return Ok(()),
                // 2. 서버 거부
                Some((0xFF, _)) => return Err(parse_server_error(&payload)),
                // 3. 플러그인 전환
                Some((0xFE, switch_body)) => {
                    let mut input = switch_body;
                    plugin = String::from_utf8_lossy(take_nul_bytes(&mut input)).into_owned();
                    nonce = input.to_vec();
                    let response = build_auth_response(&plugin, &options.password, &nonce)?;
                    self.write_packet(&response, &[])?;
                }
                // 4. caching_sha2 빠른 인증 통과
                Some((0x01, [0x03])) if plugin == SHA2_PLUGIN => {}
                // 5. caching_sha2 전체 인증
                Some((0x01, [0x04])) if plugin == SHA2_PLUGIN => {
                    self.write_packet(&[0x02], &[])?;
                    let key_packet = self.read_packet()?;
                    let pem = match key_packet.split_first() {
                        Some((0x01, pem)) => pem,
                        Some((0xFF, _)) => return Err(parse_server_error(&key_packet)),
                        _ => {
                            return Err(MysqlError::Protocol("missing server public key".into()));
                        }
                    };
                    let encrypted = encrypt_password(&options.password, &nonce, pem)?;
                    self.write_packet(&encrypted, &[])?;
                }
                _ => {
                    return Err(MysqlError::Protocol(
                        "unexpected authentication packet".into(),
                    ));
                }
            }
        }
        Err(MysqlError::Protocol(
            "authentication exchange did not finish".into(),
        ))
    }
}
pub(super) fn build_auth_response(
    plugin: &str,
    password: &str,
    nonce: &[u8],
) -> Result<Vec<u8>, MysqlError> {
    // 1. 미지원 플러그인
    if plugin != NATIVE_PLUGIN && plugin != SHA2_PLUGIN {
        return Err(MysqlError::Auth(format!(
            "unsupported authentication plugin `{plugin}`"
        )));
    }

    // 2. 빈 비밀번호
    if password.is_empty() {
        return Ok(Vec::new());
    }
    let Some(nonce) = nonce.get(..NONCE_BYTES) else {
        return Err(MysqlError::Protocol(
            "authentication nonce shorter than 20 bytes".into(),
        ));
    };

    // 3. mysql_native_password
    if plugin == NATIVE_PLUGIN {
        let password_hash = digest_sha1(password.as_bytes());
        let mut salted = nonce.to_vec();
        salted.extend_from_slice(&digest_sha1(&password_hash));
        let mask = digest_sha1(&salted);
        return Ok(password_hash
            .iter()
            .zip(mask)
            .map(|(hash_byte, mask_byte)| hash_byte ^ mask_byte)
            .collect());
    }

    // 4. caching_sha2_password
    let password_hash = digest_sha256(password.as_bytes());
    let mut salted = digest_sha256(&password_hash).to_vec();
    salted.extend_from_slice(nonce);
    let mask = digest_sha256(&salted);
    Ok(password_hash
        .iter()
        .zip(mask)
        .map(|(hash_byte, mask_byte)| hash_byte ^ mask_byte)
        .collect())
}
pub(super) fn encrypt_password(
    password: &str,
    nonce: &[u8],
    pem: &[u8],
) -> Result<Vec<u8>, MysqlError> {
    // 1. 서버 공개키
    let (modulus, exponent) = parse_public_key(pem)?;

    // 2. 난수 반복 XOR
    let Some(nonce) = nonce.get(..NONCE_BYTES) else {
        return Err(MysqlError::Protocol(
            "authentication nonce shorter than 20 bytes".into(),
        ));
    };
    let mut message: Vec<u8> = password.bytes().chain([0]).collect();
    for (byte, mask_byte) in message.iter_mut().zip(nonce.iter().cycle()) {
        *byte ^= mask_byte;
    }

    // 3. OAEP 암호화
    let mut seed = [0u8; 20];
    fill_random_bytes(&mut seed)?;
    encrypt_oaep(&message, &modulus, &exponent, &seed)
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::spawn_fake_server;
    use super::super::fake_server::{OK_PAYLOAD, SWITCH_NONCE, TEST_NONCE, TEST_PASSWORD};
    use super::super::fake_server::{TEST_PUBLIC_KEY, build_greeting, frame_packets};
    use super::*;

    #[test]
    fn switches_plugin_and_completes_sha2_authentication() {
        let switch_to = |plugin: &str| -> Vec<u8> {
            let mut payload = vec![0xFE];
            payload.extend_from_slice(plugin.as_bytes());
            payload.push(0);
            payload.extend_from_slice(SWITCH_NONCE);
            payload.push(0);
            payload
        };
        let mut key_payload = vec![0x01];
        key_payload.extend_from_slice(TEST_PUBLIC_KEY.as_bytes());

        // 1. mysql_native_password 전환
        let replies = vec![
            frame_packets(2, &[switch_to(NATIVE_PLUGIN)]),
            frame_packets(4, &[OK_PAYLOAD.to_vec()]),
        ];
        let (options, server) = spawn_fake_server(build_greeting(SHA2_PLUGIN), replies);
        Connection::connect(&options).unwrap().close();
        let requests = server.join().unwrap();
        let sha2_scramble = build_auth_response(SHA2_PLUGIN, TEST_PASSWORD, TEST_NONCE).unwrap();
        assert!(requests[0].ends_with(b"caching_sha2_password\0"));
        assert_eq!(requests[0][39..40], [32]);
        assert_eq!(requests[0][40..72], sha2_scramble);
        assert_eq!(
            requests[1],
            build_auth_response(NATIVE_PLUGIN, TEST_PASSWORD, SWITCH_NONCE).unwrap()
        );

        // 2. caching_sha2 전환 뒤 전체 인증
        let replies = vec![
            frame_packets(2, &[switch_to(SHA2_PLUGIN)]),
            frame_packets(4, &[vec![0x01, 0x04]]),
            frame_packets(6, &[key_payload]),
            frame_packets(8, &[OK_PAYLOAD.to_vec()]),
        ];
        let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
        Connection::connect(&options).unwrap().close();
        let requests = server.join().unwrap();
        assert_eq!(
            requests[1],
            build_auth_response(SHA2_PLUGIN, TEST_PASSWORD, SWITCH_NONCE).unwrap()
        );
        assert_eq!(requests[2], [0x02]);
        assert_eq!(requests[3].len(), 128);

        // 3. 빠른 인증 통과
        let replies = vec![frame_packets(2, &[vec![0x01, 0x03], OK_PAYLOAD.to_vec()])];
        let (options, server) = spawn_fake_server(build_greeting(SHA2_PLUGIN), replies);
        Connection::connect(&options).unwrap().close();
        server.join().unwrap();

        // 4. 미지원 플러그인 전환 및 접근 거부
        let replies = vec![frame_packets(2, &[switch_to("sha256_password")])];
        let (options, server) = spawn_fake_server(build_greeting(SHA2_PLUGIN), replies);
        let Err(error) = Connection::connect(&options) else {
            panic!("auth error expected");
        };
        assert!(matches!(&error, MysqlError::Auth(detail) if detail.contains("sha256_password")));
        server.join().unwrap();
        let replies = vec![frame_packets(
            2,
            &[b"\xFF\x15\x04#28000Access denied".to_vec()],
        )];
        let (options, server) = spawn_fake_server(build_greeting(SHA2_PLUGIN), replies);
        let Err(error) = Connection::connect(&options) else {
            panic!("server error expected");
        };
        assert!(matches!(&error, MysqlError::Server { code: 1045, .. }));
        assert!(!error.to_string().contains(TEST_PASSWORD));
        server.join().unwrap();
    }
}
