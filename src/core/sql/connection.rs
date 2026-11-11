//! connection.rs
//! core::sql::connection
//!
//! MySQL connection over plain TCP: connect, accessors, I/O timeout, and close (`COM_QUIT` on drop).
//! An `Io` or `Protocol` error, a truncated result, or a refused request leaves the connection not reusable.
//!

use super::error::MysqlError;
use super::types::ConnectOptions;
use std::io::{self, BufReader, ErrorKind, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

const COM_QUIT: u8 = 0x01;
const READ_BUFFER_BYTES: usize = 64 * 1024;

// 1. Connection ---------------------------------------------------------------
#[derive(Debug)]
pub struct Connection {
    pub(super) reader: BufReader<TcpStream>,
    // 다음 패킷 순번
    pub(super) sequence: u8,
    pub(super) server_version: String,
    pub(super) connection_id: u32,
    pub(super) reusable: bool,
}
impl Connection {
    pub fn connect(options: &ConnectOptions) -> Result<Connection, MysqlError> {
        // 1. TCP 연결
        let mut stream = None;
        let mut last_error =
            io::Error::new(ErrorKind::AddrNotAvailable, "host resolved to no address");
        for address in (options.host.as_str(), options.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, options.connect_timeout) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = error,
            }
        }
        let Some(stream) = stream else {
            return Err(MysqlError::Io(last_error));
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(options.io_timeout))?;
        stream.set_write_timeout(Some(options.io_timeout))?;
        let mut connection = Connection {
            reader: BufReader::with_capacity(READ_BUFFER_BYTES, stream),
            sequence: 0,
            server_version: String::new(),
            connection_id: 0,
            reusable: true,
        };

        // 2. 핸드셰이크 및 인증
        let greeting = connection.read_handshake()?;
        connection.send_handshake_response(options, &greeting)?;
        connection.complete_authentication(options, greeting)?;
        Ok(connection)
    }
    pub fn server_version(&self) -> &str {
        &self.server_version
    }
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }
    pub fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), MysqlError> {
        let stream = self.reader.get_ref();
        let applied = stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)));
        if applied.is_err() {
            self.reusable = false;
        }
        applied.map_err(MysqlError::Io)
    }
    pub fn is_reusable(&self) -> bool {
        self.reusable
    }
    // COM_QUIT 전송은 Drop 담당
    pub fn close(self) {
        drop(self);
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let stream = self.reader.get_mut();
        let _ = stream.write_all(&[1, 0, 0, 0, COM_QUIT]);
        let _ = stream.shutdown(Shutdown::Both);
    }
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::spawn_fake_server;
    use super::super::fake_server::{OK_PAYLOAD, TEST_NONCE, TEST_PASSWORD, UNLIMITED};
    use super::super::fake_server::{build_greeting, build_result_payloads, frame_packets};
    use super::super::handshake::build_auth_response;
    use super::super::handshake::{CLIENT_CONNECT_WITH_DB, CLIENT_LENENC_AUTH_DATA};
    use super::super::handshake::{CLIENT_LONG_FLAG, CLIENT_LONG_PASSWORD, CLIENT_PLUGIN_AUTH};
    use super::super::handshake::{CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION};
    use super::super::handshake::{CLIENT_TRANSACTIONS, MAX_PACKET_BYTES, NATIVE_PLUGIN};
    use super::super::packet::FRAGMENT_BYTES;
    use super::super::types::column_type;
    use super::super::types::{ColumnMeta, FLAG_BINARY, StatementOutcome, StatementSummary};
    use super::*;

    #[test]
    fn connects_and_reads_statements() {
        let rows: [Vec<Option<&[u8]>>; 2] = [
            vec![Some(b"1"), None],
            vec![Some("한글".as_bytes()), Some(b"")],
        ];
        let replies = vec![
            frame_packets(2, &[OK_PAYLOAD.to_vec()]),
            frame_packets(1, &build_result_payloads(&["id", "note"], &rows, 2)),
            frame_packets(1, &[b"\xFF\x28\x04#42000syntax".to_vec()]),
            frame_packets(1, &[b"\x00\x02\x07\x02\x00\x01\x00\x04info".to_vec()]),
        ];
        let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
        let mut connection = Connection::connect(&options).unwrap();
        assert_eq!(connection.server_version(), "8.0.0-fake");
        assert_eq!(connection.connection_id(), 77);

        // 1. 결과 집합
        let outcome = connection.run_statement("SELECT 1", &UNLIMITED).unwrap();
        let StatementOutcome::Rows(result_set) = outcome else {
            panic!("rows expected");
        };
        assert_eq!(
            result_set.columns[1],
            ColumnMeta {
                name: "note".into(),
                column_type: column_type::VAR_STRING,
                flags: FLAG_BINARY,
                charset: 45,
                decimals: 31,
            }
        );
        assert_eq!(
            result_set.rows,
            [
                vec![Some(b"1".to_vec()), None],
                vec![Some("한글".as_bytes().to_vec()), Some(Vec::new())],
            ]
        );
        assert!(!result_set.truncated);

        // 2. 서버 오류 뒤 재사용
        let error = connection.run_statement("SELEC", &UNLIMITED).unwrap_err();
        assert!(matches!(
            &error,
            MysqlError::Server { code: 1064, sql_state, .. } if sql_state == "42000"
        ));
        assert!(connection.is_reusable());
        let outcome = connection.run_statement("UPDATE t", &UNLIMITED).unwrap();
        assert_eq!(
            outcome,
            StatementOutcome::Done(StatementSummary {
                affected_rows: 2,
                last_insert_id: 7,
                warnings: 1,
                info: "info".into(),
            })
        );
        connection.close();

        // 3. 핸드셰이크 응답 및 COM_QUIT
        let requests = server.join().unwrap();
        let response = &requests[0];
        let capabilities = CLIENT_LONG_PASSWORD
            | CLIENT_LONG_FLAG
            | CLIENT_CONNECT_WITH_DB
            | CLIENT_PROTOCOL_41
            | CLIENT_TRANSACTIONS
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_LENENC_AUTH_DATA;
        assert_eq!(response[..4], capabilities.to_le_bytes());
        assert_eq!(response[4..8], MAX_PACKET_BYTES.to_le_bytes());
        assert_eq!(response[8], 224);
        let mut identity = b"tester\0\x14".to_vec();
        identity.extend(build_auth_response(NATIVE_PLUGIN, TEST_PASSWORD, TEST_NONCE).unwrap());
        identity.extend_from_slice(b"db\0mysql_native_password\0");
        assert_eq!(response[32..], identity);
        assert_eq!(requests[1], b"\x03SELECT 1");
        assert_eq!(requests[4], [1, 0, 0, 0, COM_QUIT]);
    }
    #[test]
    fn rejects_oversized_statement_and_flags_timeout() {
        let replies = vec![frame_packets(2, &[OK_PAYLOAD.to_vec()])];
        let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);

        // 1. 무응답 서버 읽기 시간 초과
        let mut connection = Connection::connect(&options).unwrap();
        connection
            .set_io_timeout(Duration::from_millis(200))
            .unwrap();
        let error = connection
            .run_statement("SELECT SLEEP(9)", &UNLIMITED)
            .unwrap_err();
        assert!(error.is_timeout(), "{error}");
        assert!(!connection.is_reusable());
        drop(connection);
        server.join().unwrap();

        // 2. 단일 패킷 한계 이상 문장
        let replies = vec![frame_packets(2, &[OK_PAYLOAD.to_vec()])];
        let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
        let mut connection = Connection::connect(&options).unwrap();
        let oversized = "a".repeat(FRAGMENT_BYTES - 1);
        assert!(matches!(
            connection.run_statement(&oversized, &UNLIMITED),
            Err(MysqlError::Protocol(_))
        ));
        assert!(!connection.is_reusable());
        drop(connection);
        let requests = server.join().unwrap();
        assert_eq!(requests[1], [1, 0, 0, 0, COM_QUIT]);
    }
}
