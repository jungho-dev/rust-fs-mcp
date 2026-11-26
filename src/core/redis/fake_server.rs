//! fake_server.rs
//! core::redis::fake_server
//!
//! Test-only loopback Redis server: replays prepared replies and records each received argv.
//! A `None` reply sends nothing for that command; the server runs until the client closes.
//!

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

// 1. Fake Redis server --------------------------------------------------------
pub(crate) fn spawn_fake_redis(
    replies: Vec<Option<Vec<u8>>>,
) -> (u16, JoinHandle<Vec<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        // 1. 명령 한 건 읽기 (EOF, 형식 오류 = None)
        let read_command = |reader: &mut BufReader<TcpStream>| -> Option<Vec<Vec<u8>>> {
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            let count: usize = line.trim_end().strip_prefix('*')?.parse().ok()?;
            let mut argv = Vec::with_capacity(count);
            for _ in 0..count {
                line.clear();
                reader.read_line(&mut line).ok()?;
                let len: usize = line.trim_end().strip_prefix('$')?.parse().ok()?;
                let mut arg = vec![0; len + 2];
                reader.read_exact(&mut arg).ok()?;
                arg.truncate(len);
                argv.push(arg);
            }
            Some(argv)
        };

        // 2. 수신 기록 및 응답 재생
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut replies = replies.into_iter();
        let mut commands = Vec::new();
        while let Some(argv) = read_command(&mut reader) {
            commands.push(argv);
            if let Some(Some(reply)) = replies.next()
                && writer.write_all(&reply).is_err()
            {
                break;
            }
        }
        commands
    });
    (port, server)
}
