//! connection.rs
//! core::redis::connection
//!
//! Redis connection over plain TCP (RESP2, no HELLO): connect, AUTH, SELECT, and one command per round trip.
//! The socket address comes only from ConnectOptions; MOVED and ASK replies are returned, never followed.
//! An I/O or protocol error, or a reply stopped past the discard limit, leaves the connection not reusable.
//! Display is the bare detail; the tool layer adds the target name and command number.
//!

use super::resp::{Reply, ReplyLimits, encode_command, read_reply};
use std::fmt;
use std::io::{self, BufReader, ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const CONTROL_LIMITS: ReplyLimits = ReplyLimits {
    max_items: 16,
    max_bytes: 64 * 1024,
};

// 1. Connect options and errors -----------------------------------------------
#[derive(Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    // 빈 문자열 = 비밀번호 없음
    pub password: String,
    pub db: u32,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}
// 비밀번호 비노출
impl fmt::Debug for ConnectOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("db", &self.db)
            .field("connect_timeout", &self.connect_timeout)
            .field("io_timeout", &self.io_timeout)
            .finish()
    }
}
#[derive(Debug)]
pub enum RedisError {
    Io(io::Error),
    Protocol(String),
    // AUTH 거부 서버 문구
    Auth(String),
    // SELECT 거부 서버 문구
    Server(String),
}
impl RedisError {
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Io(error) => matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock),
            _ => false,
        }
    }
}
impl fmt::Display for RedisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Protocol(detail) | Self::Auth(detail) | Self::Server(detail) => {
                formatter.write_str(detail)
            }
        }
    }
}
impl std::error::Error for RedisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for RedisError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// 2. Connection ---------------------------------------------------------------
#[derive(Debug)]
pub struct Connection {
    reader: BufReader<TcpStream>,
    db: u32,
    reusable: bool,
}
impl Connection {
    pub fn connect(options: &ConnectOptions) -> Result<Connection, RedisError> {
        // 1. TCP 연결 (주소별 시도, 전체 시한)
        let deadline = Instant::now() + options.connect_timeout;
        let mut stream = None;
        let mut last_error =
            io::Error::new(ErrorKind::AddrNotAvailable, "host resolved to no address");
        for address in (options.host.as_str(), options.port).to_socket_addrs()? {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                last_error = io::Error::new(ErrorKind::TimedOut, "connect timed out");
                break;
            }
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = error,
            }
        }
        let Some(stream) = stream else {
            return Err(RedisError::Io(last_error));
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(options.io_timeout))?;
        stream.set_write_timeout(Some(options.io_timeout))?;
        let mut connection = Connection {
            reader: BufReader::new(stream),
            db: 0,
            reusable: true,
        };

        // 2. 인증 (AUTH user pass | AUTH pass)
        let password = options.password.as_bytes().to_vec();
        let auth_argv = match (&options.user, password.is_empty()) {
            (Some(user), _) => vec![b"AUTH".to_vec(), user.as_bytes().to_vec(), password],
            (None, false) => vec![b"AUTH".to_vec(), password],
            (None, true) => Vec::new(),
        };
        let mut limits = CONTROL_LIMITS;
        let auth_reply = if auth_argv.is_empty() {
            Reply::Status("OK".to_string())
        } else {
            connection.run(&auth_argv, &mut limits)?
        };
        match auth_reply {
            Reply::Status(_) => {}
            Reply::Error(text) => return Err(RedisError::Auth(text)),
            _ => {
                return Err(RedisError::Protocol("unexpected reply to AUTH".to_string()));
            }
        }

        // 3. 데이터베이스 선택
        if options.db != 0 {
            connection.select(options.db)?;
        }
        Ok(connection)
    }
    // 서버 오류 응답은 Ok(Reply::Error)
    pub fn run(&mut self, argv: &[Vec<u8>], limits: &mut ReplyLimits) -> Result<Reply, RedisError> {
        // 1. 전송 가능 여부
        if argv.is_empty() {
            return Err(RedisError::Protocol(
                "a command needs at least one argument".to_string(),
            ));
        }
        if !self.reusable {
            return Err(RedisError::Protocol(
                "the connection lost reply sync; reconnect first".to_string(),
            ));
        }

        // 2. 전송 및 응답
        let written = self.reader.get_mut().write_all(&encode_command(argv));
        if let Err(error) = written {
            self.reusable = false;
            return Err(RedisError::Io(error));
        }
        let parsed = match read_reply(&mut self.reader, limits) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.reusable = false;
                return Err(error);
            }
        };
        self.reusable = parsed.in_sync;
        Ok(parsed.reply)
    }
    pub fn select(&mut self, db: u32) -> Result<(), RedisError> {
        let argv = vec![b"SELECT".to_vec(), db.to_string().into_bytes()];
        let mut limits = CONTROL_LIMITS;
        match self.run(&argv, &mut limits)? {
            Reply::Status(_) => {
                self.db = db;
                Ok(())
            }
            Reply::Error(text) => Err(RedisError::Server(text)),
            _ => {
                self.reusable = false;
                Err(RedisError::Protocol(
                    "unexpected reply to SELECT".to_string(),
                ))
            }
        }
    }
    pub fn db(&self) -> u32 {
        self.db
    }
    pub fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), RedisError> {
        let stream = self.reader.get_ref();
        let applied = stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)));
        if applied.is_err() {
            self.reusable = false;
        }
        applied.map_err(RedisError::Io)
    }
    pub fn is_reusable(&self) -> bool {
        self.reusable
    }
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::spawn_fake_redis;
    use super::*;

    const TEST_PASSWORD: &str = "secret-pw";

    fn build_options(port: u16, user: Option<&str>, password: &str, db: u32) -> ConnectOptions {
        ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            user: user.map(str::to_string),
            password: password.to_string(),
            db,
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(5),
        }
    }
    fn build_argv(words: &[&str]) -> Vec<Vec<u8>> {
        words.iter().map(|word| word.as_bytes().to_vec()).collect()
    }
    fn wide_limits() -> ReplyLimits {
        ReplyLimits {
            max_items: 100,
            max_bytes: 1024,
        }
    }
    #[test]
    fn authenticates_with_user_and_selects_db() {
        let replies = vec![
            Some(b"+OK\r\n".to_vec()),
            Some(b"+OK\r\n".to_vec()),
            Some(b"$5\r\nhello\r\n".to_vec()),
            Some(b"-WRONGTYPE Operation against a key\r\n".to_vec()),
            Some(b"-ERR DB index is out of range\r\n".to_vec()),
            Some(b"+OK\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let options = build_options(port, Some("tester"), TEST_PASSWORD, 2);
        let mut connection = Connection::connect(&options).unwrap();
        assert_eq!(connection.db(), 2);

        // 1. 응답과 오류 응답
        let mut limits = wide_limits();
        let reply = connection.run(&build_argv(&["GET", "k"]), &mut limits);
        assert_eq!(reply.unwrap(), Reply::Bulk(b"hello".to_vec()));
        let reply = connection.run(&build_argv(&["LPUSH", "k", "v"]), &mut limits);
        assert_eq!(
            reply.unwrap(),
            Reply::Error("WRONGTYPE Operation against a key".to_string())
        );
        assert!(connection.is_reusable());

        // 2. SELECT 거부 뒤 성공
        let refused = connection.select(99).unwrap_err();
        assert!(matches!(&refused, RedisError::Server(text) if text.contains("out of range")));
        assert_eq!(connection.db(), 2);
        connection.select(3).unwrap();
        assert_eq!(connection.db(), 3);
        drop(connection);
        let commands = server.join().unwrap();
        assert_eq!(
            commands,
            [
                build_argv(&["AUTH", "tester", TEST_PASSWORD]),
                build_argv(&["SELECT", "2"]),
                build_argv(&["GET", "k"]),
                build_argv(&["LPUSH", "k", "v"]),
                build_argv(&["SELECT", "99"]),
                build_argv(&["SELECT", "3"]),
            ]
        );
    }
    #[test]
    fn sends_password_only_auth_or_none() {
        // 1. 비밀번호만
        let replies = vec![Some(b"+OK\r\n".to_vec()), Some(b"+PONG\r\n".to_vec())];
        let (port, server) = spawn_fake_redis(replies);
        let mut connection = Connection::connect(&build_options(port, None, "pw", 0)).unwrap();
        let reply = connection.run(&build_argv(&["PING"]), &mut wide_limits());
        assert_eq!(reply.unwrap(), Reply::Status("PONG".to_string()));
        drop(connection);
        let commands = server.join().unwrap();
        assert_eq!(
            commands,
            [build_argv(&["AUTH", "pw"]), build_argv(&["PING"])]
        );

        // 2. 인증 없음
        let (port, server) = spawn_fake_redis(vec![Some(b"+PONG\r\n".to_vec())]);
        let mut connection = Connection::connect(&build_options(port, None, "", 0)).unwrap();
        assert_eq!(connection.db(), 0);
        let reply = connection.run(&build_argv(&["PING"]), &mut wide_limits());
        assert_eq!(reply.unwrap(), Reply::Status("PONG".to_string()));
        drop(connection);
        assert_eq!(server.join().unwrap(), [build_argv(&["PING"])]);
    }
    #[test]
    fn reports_wrongpass_without_the_password() {
        let wrongpass = b"-WRONGPASS invalid username-password pair or user is disabled.\r\n";
        let (port, server) = spawn_fake_redis(vec![Some(wrongpass.to_vec())]);
        let options = build_options(port, Some("tester"), TEST_PASSWORD, 0);
        let error = Connection::connect(&options).unwrap_err();
        assert!(matches!(&error, RedisError::Auth(text) if text.starts_with("WRONGPASS")));
        assert!(!error.to_string().contains(TEST_PASSWORD));
        let debug_text = format!("{options:?}");
        assert!(debug_text.contains("password: \"<redacted>\""));
        assert!(!debug_text.contains(TEST_PASSWORD));
        server.join().unwrap();
    }
    #[test]
    fn flags_timeout_and_drops_the_connection() {
        let (port, server) = spawn_fake_redis(vec![None]);
        let mut connection = Connection::connect(&build_options(port, None, "", 0)).unwrap();
        connection
            .set_io_timeout(Duration::from_millis(200))
            .unwrap();
        let error = connection
            .run(&build_argv(&["BLPOP", "k", "9"]), &mut wide_limits())
            .unwrap_err();
        assert!(error.is_timeout(), "{error}");
        assert!(!connection.is_reusable());
        let refused = connection.run(&build_argv(&["PING"]), &mut wide_limits());
        assert!(matches!(refused, Err(RedisError::Protocol(_))));
        drop(connection);
        server.join().unwrap();

        // 시간 초과 판별
        assert!(RedisError::Io(ErrorKind::WouldBlock.into()).is_timeout());
        assert!(!RedisError::Io(ErrorKind::ConnectionReset.into()).is_timeout());
        assert!(!RedisError::Protocol(String::new()).is_timeout());
    }
    #[test]
    fn keeps_sync_after_cut_and_drops_after_discard_limit() {
        let replies = vec![
            Some(b"$10\r\n0123456789\r\n".to_vec()),
            Some(b":7\r\n".to_vec()),
            Some(b"$73400320\r\nabcd".to_vec()),
            Some(b"!bad\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let mut connection = Connection::connect(&build_options(port, None, "", 0)).unwrap();

        // 1. 잘린 bulk 뒤 동기 유지
        let mut limits = ReplyLimits {
            max_items: 10,
            max_bytes: 4,
        };
        let reply = connection.run(&build_argv(&["GET", "a"]), &mut limits);
        assert_eq!(
            reply.unwrap(),
            Reply::BulkCut {
                prefix: b"0123".to_vec(),
                total: 10,
            }
        );
        assert!(connection.is_reusable());
        let reply = connection.run(&build_argv(&["INCR", "n"]), &mut limits);
        assert_eq!(reply.unwrap(), Reply::Integer(7));

        // 2. 폐기 한도 초과 뒤 재사용 불가
        let mut limits = ReplyLimits {
            max_items: 10,
            max_bytes: 4,
        };
        let reply = connection.run(&build_argv(&["GET", "big"]), &mut limits);
        assert_eq!(
            reply.unwrap(),
            Reply::BulkCut {
                prefix: b"abcd".to_vec(),
                total: 73_400_320,
            }
        );
        assert!(!connection.is_reusable());
        drop(connection);
        server.join().unwrap();

        // 3. 프로토콜 오류 뒤 재사용 불가
        let (port, server) = spawn_fake_redis(vec![Some(b"!bad\r\n".to_vec())]);
        let mut connection = Connection::connect(&build_options(port, None, "", 0)).unwrap();
        let error = connection
            .run(&build_argv(&["PING"]), &mut wide_limits())
            .unwrap_err();
        assert_eq!(error.to_string(), "unexpected reply type byte 0x21");
        assert!(!connection.is_reusable());
        drop(connection);
        server.join().unwrap();
    }
}
