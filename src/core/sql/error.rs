//! error.rs
//! core::sql::error
//!
//! Error type of the MySQL client: socket, server-reported, protocol, and authentication failures.
//! `is_timeout` marks a read or write that ran past the connection's I/O timeout.
//!

use std::fmt;
use std::io::{self, ErrorKind};

// 1. Errors -------------------------------------------------------------------
#[derive(Debug)]
pub enum MysqlError {
    Io(io::Error),
    Server {
        code: u16,
        sql_state: String,
        message: String,
    },
    Protocol(String),
    Auth(String),
}
impl MysqlError {
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Io(error) => matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock),
            _ => false,
        }
    }
}
impl fmt::Display for MysqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "mysql io error: {error}"),
            Self::Server {
                code,
                sql_state,
                message,
            } => write!(
                formatter,
                "mysql server error {code} ({sql_state}): {message}"
            ),
            Self::Protocol(detail) => write!(formatter, "mysql protocol error: {detail}"),
            Self::Auth(detail) => write!(formatter, "mysql authentication error: {detail}"),
        }
    }
}
impl std::error::Error for MysqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for MysqlError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
#[cfg(test)]
mod tests {
    use super::super::connection::Connection;
    use super::super::fake_server::{TEST_PASSWORD, spawn_fake_server};
    use super::super::statement::parse_server_error;
    use super::*;

    #[test]
    fn hides_password_and_classifies_errors() {
        let (options, server) =
            spawn_fake_server(b"\xFF\x10\x04Too many connections".to_vec(), vec![]);
        let debug_text = format!("{options:?}");
        assert!(debug_text.contains("password: \"<redacted>\""));
        assert!(!debug_text.contains(TEST_PASSWORD));

        // 핸드셰이크 자리 ERR 패킷
        let Err(error) = Connection::connect(&options) else {
            panic!("handshake error expected");
        };
        assert!(matches!(
            &error,
            MysqlError::Server { code: 1040, sql_state, message }
                if sql_state.is_empty() && message == "Too many connections"
        ));
        server.join().unwrap();

        // 시간 초과 판별 및 표시 문자열
        assert!(MysqlError::Io(ErrorKind::TimedOut.into()).is_timeout());
        assert!(MysqlError::Io(ErrorKind::WouldBlock.into()).is_timeout());
        assert!(!MysqlError::Io(ErrorKind::ConnectionReset.into()).is_timeout());
        assert!(!MysqlError::Protocol(String::new()).is_timeout());
        let denied = parse_server_error(b"\xFF\x15\x04#28000Access denied");
        assert_eq!(
            denied.to_string(),
            "mysql server error 1045 (28000): Access denied"
        );
    }
}
