//! resp.rs
//! core::redis::resp
//!
//! RESP2 framing: argv encoding and a bounded reply parser.
//! Limits: nesting 32, line 64 KiB, bulk 512 MiB, discarded bytes 64 MiB per reply.
//! Past the discard limit the parser stops reading and reports the stream out of sync.
//!

use super::connection::RedisError;
use std::io::{self, BufRead, ErrorKind, Read};

const MAX_DEPTH: usize = 32;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_BULK_BYTES: usize = 512 * 1024 * 1024;
const MAX_DISCARD_BYTES: u64 = 64 * 1024 * 1024;

// 1. Reply types --------------------------------------------------------------
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    Status(String),
    // 서버 오류 응답 (연결 정상)
    Error(String),
    Integer(i64),
    Bulk(Vec<u8>),
    BulkCut { prefix: Vec<u8>, total: usize },
    Nil,
    Array(Vec<Reply>),
    ArrayCut { items: Vec<Reply>, total: usize },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplyLimits {
    // 배열 한 단계 보관 요소 상한
    pub max_items: usize,
    // 남은 보관 바이트 (읽을 때 차감)
    pub max_bytes: usize,
}
#[derive(Debug)]
pub(super) struct ParsedReply {
    pub reply: Reply,
    // false = 폐기 한도 초과 중단
    pub in_sync: bool,
}
struct ReplyReader<'a, R: BufRead> {
    reader: &'a mut R,
    limits: &'a mut ReplyLimits,
    discarded: u64,
    stopped: bool,
}
impl<R: BufRead> ReplyReader<'_, R> {
    fn read_value(&mut self, depth: usize) -> Result<Reply, RedisError> {
        let (kind, body) = self.read_header()?;
        match kind {
            b'+' | b'-' => {
                self.limits.max_bytes = self.limits.max_bytes.saturating_sub(body.len());
                let text = String::from_utf8_lossy(&body).into_owned();
                Ok(if kind == b'+' {
                    Reply::Status(text)
                } else {
                    Reply::Error(text)
                })
            }
            b':' => std::str::from_utf8(&body)
                .ok()
                .and_then(|text| text.parse::<i64>().ok())
                .map(Reply::Integer)
                .ok_or_else(|| RedisError::Protocol("invalid integer reply".to_string())),
            b'$' => self.read_bulk(&body),
            b'*' => self.read_array(&body, depth),
            other => Err(unexpected_type(other)),
        }
    }
    fn read_bulk(&mut self, body: &[u8]) -> Result<Reply, RedisError> {
        // 1. 길이와 보관분
        let Some(total) = parse_length(body, MAX_BULK_BYTES)? else {
            return Ok(Reply::Nil);
        };
        let keep = total.min(self.limits.max_bytes);
        let mut prefix = vec![0; keep];
        self.reader.read_exact(&mut prefix)?;
        self.limits.max_bytes -= keep;
        if keep == total {
            let mut tail = [0u8; 2];
            self.reader.read_exact(&mut tail)?;
            if tail != *b"\r\n" {
                return Err(RedisError::Protocol(
                    "bulk reply does not end with CRLF".to_string(),
                ));
            }
            return Ok(Reply::Bulk(prefix));
        }

        // 2. 나머지 폐기 (한도 초과 시 중단)
        let skip = (total - keep + 2) as u64;
        self.discarded += skip;
        let cut = Reply::BulkCut { prefix, total };
        if self.discarded > MAX_DISCARD_BYTES {
            self.stopped = true;
            return Ok(cut);
        }
        let copied = io::copy(&mut (&mut *self.reader).take(skip), &mut io::sink())?;
        if copied < skip {
            return Err(RedisError::Io(ErrorKind::UnexpectedEof.into()));
        }
        Ok(cut)
    }
    fn read_array(&mut self, body: &[u8], depth: usize) -> Result<Reply, RedisError> {
        // 1. 길이와 깊이
        let Some(total) = parse_length(body, usize::MAX)? else {
            return Ok(Reply::Nil);
        };
        if depth == MAX_DEPTH {
            return Err(RedisError::Protocol(format!(
                "reply nesting is deeper than {MAX_DEPTH} levels"
            )));
        }

        // 2. 보관 요소
        let mut items = Vec::new();
        let mut read_count = 0;
        while read_count < total
            && !self.stopped
            && items.len() < self.limits.max_items
            && self.limits.max_bytes > 0
        {
            items.push(self.read_value(depth + 1)?);
            read_count += 1;
        }

        // 3. 남은 요소 폐기
        if read_count < total && !self.stopped {
            self.skip_values((total - read_count) as u64)?;
        }
        if read_count == total {
            return Ok(Reply::Array(items));
        }
        Ok(Reply::ArrayCut { items, total })
    }
    // 보관 없이 값 건너뛰기 (반복, 재귀 없음)
    fn skip_values(&mut self, count: u64) -> Result<(), RedisError> {
        let mut pending = count;
        while pending > 0 {
            pending -= 1;
            let (kind, body) = self.read_header()?;
            let body_bytes = match kind {
                b'+' | b'-' | b':' => 0,
                b'$' => parse_length(&body, MAX_BULK_BYTES)?.map_or(0, |len| len as u64 + 2),
                b'*' => {
                    let extra = parse_length(&body, usize::MAX)?.unwrap_or(0) as u64;
                    pending = pending.checked_add(extra).ok_or_else(|| {
                        RedisError::Protocol("array reply is too long".to_string())
                    })?;
                    0
                }
                other => return Err(unexpected_type(other)),
            };
            self.discarded += body.len() as u64 + 3 + body_bytes;
            if self.discarded > MAX_DISCARD_BYTES {
                self.stopped = true;
                return Ok(());
            }
            let copied = io::copy(&mut (&mut *self.reader).take(body_bytes), &mut io::sink())?;
            if copied < body_bytes {
                return Err(RedisError::Io(ErrorKind::UnexpectedEof.into()));
            }
        }
        Ok(())
    }
    // 형 바이트와 CRLF 제외 본문
    fn read_header(&mut self) -> Result<(u8, Vec<u8>), RedisError> {
        let limit = (MAX_LINE_BYTES + 2) as u64;
        let mut line = Vec::new();
        (&mut *self.reader)
            .take(limit)
            .read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") && line.len() as u64 == limit {
            return Err(RedisError::Protocol(format!(
                "reply line is longer than {MAX_LINE_BYTES} bytes"
            )));
        }
        if !line.ends_with(b"\n") {
            return Err(RedisError::Io(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the server closed the connection mid-reply",
            )));
        }
        if line.len() < 3 || !line.ends_with(b"\r\n") {
            return Err(RedisError::Protocol("malformed reply line".to_string()));
        }
        line.truncate(line.len() - 2);
        let kind = line.remove(0);
        Ok((kind, line))
    }
}

// 2. Framing ------------------------------------------------------------------
pub(super) fn encode_command(argv: &[Vec<u8>]) -> Vec<u8> {
    let mut frame = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        frame.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        frame.extend_from_slice(arg);
        frame.extend_from_slice(b"\r\n");
    }
    frame
}
pub(super) fn read_reply<R: BufRead>(
    reader: &mut R,
    limits: &mut ReplyLimits,
) -> Result<ParsedReply, RedisError> {
    let mut reply_reader = ReplyReader {
        reader,
        limits,
        discarded: 0,
        stopped: false,
    };
    let reply = reply_reader.read_value(0)?;
    Ok(ParsedReply {
        reply,
        in_sync: !reply_reader.stopped,
    })
}

// 3. (내부) Length and type checks --------------------------------------------
// None = nil (-1)
fn parse_length(body: &[u8], cap: usize) -> Result<Option<usize>, RedisError> {
    let length = std::str::from_utf8(body)
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .ok_or_else(|| RedisError::Protocol("invalid reply length".to_string()))?;
    if length == -1 {
        return Ok(None);
    }
    let Ok(length) = usize::try_from(length) else {
        return Err(RedisError::Protocol(format!(
            "invalid reply length {length}"
        )));
    };
    if length > cap {
        return Err(RedisError::Protocol(format!(
            "reply length {length} is past the limit of {cap} bytes"
        )));
    }
    Ok(Some(length))
}
fn unexpected_type(kind: u8) -> RedisError {
    RedisError::Protocol(format!("unexpected reply type byte 0x{kind:02X}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const WIDE: ReplyLimits = ReplyLimits {
        max_items: usize::MAX,
        max_bytes: usize::MAX,
    };

    fn read_one(stream: &mut Cursor<Vec<u8>>, limits: &mut ReplyLimits) -> ParsedReply {
        read_reply(stream, limits).unwrap()
    }
    fn protocol_detail(input: &[u8]) -> String {
        let mut limits = WIDE;
        match read_reply(&mut Cursor::new(input.to_vec()), &mut limits) {
            Err(RedisError::Protocol(detail)) => detail,
            other => panic!("protocol error expected, got {other:?}"),
        }
    }
    #[test]
    fn encodes_argv_as_bulk_array() {
        let argv = vec![b"SET".to_vec(), b"k".to_vec(), "한".as_bytes().to_vec()];
        assert_eq!(
            encode_command(&argv),
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\n\xed\x95\x9c\r\n"
        );
    }
    #[test]
    fn parses_every_resp2_type_in_sequence() {
        let input = b"+OK\r\n-ERR wrong\r\n:-42\r\n$3\r\nabc\r\n$0\r\n\r\n$-1\r\n*-1\r\n*2\r\n:1\r\n*1\r\n$1\r\na\r\n*0\r\n";
        let mut stream = Cursor::new(input.to_vec());
        let mut limits = WIDE;
        let expected = [
            Reply::Status("OK".to_string()),
            Reply::Error("ERR wrong".to_string()),
            Reply::Integer(-42),
            Reply::Bulk(b"abc".to_vec()),
            Reply::Bulk(Vec::new()),
            Reply::Nil,
            Reply::Nil,
            Reply::Array(vec![
                Reply::Integer(1),
                Reply::Array(vec![Reply::Bulk(b"a".to_vec())]),
            ]),
            Reply::Array(Vec::new()),
        ];
        for reply in expected {
            let parsed = read_one(&mut stream, &mut limits);
            assert_eq!(parsed.reply, reply);
            assert!(parsed.in_sync);
        }
        assert_eq!(stream.position() as usize, input.len());
    }
    #[test]
    fn cut_bulk_keeps_stream_in_sync() {
        let mut stream = Cursor::new(b"$5\r\nhello\r\n+NEXT\r\n".to_vec());
        let mut limits = ReplyLimits {
            max_items: 10,
            max_bytes: 2,
        };
        let parsed = read_one(&mut stream, &mut limits);
        assert_eq!(
            parsed.reply,
            Reply::BulkCut {
                prefix: b"he".to_vec(),
                total: 5,
            }
        );
        assert!(parsed.in_sync);
        assert_eq!(limits.max_bytes, 0);
        let next = read_one(&mut stream, &mut limits);
        assert_eq!(next.reply, Reply::Status("NEXT".to_string()));
    }
    #[test]
    fn cut_array_keeps_stream_in_sync() {
        let input = b"*4\r\n:1\r\n:2\r\n*2\r\n:3\r\n$-1\r\n$2\r\nzz\r\n+NEXT\r\n";
        let mut stream = Cursor::new(input.to_vec());
        let mut limits = ReplyLimits {
            max_items: 2,
            max_bytes: 100,
        };
        let parsed = read_one(&mut stream, &mut limits);
        assert_eq!(
            parsed.reply,
            Reply::ArrayCut {
                items: vec![Reply::Integer(1), Reply::Integer(2)],
                total: 4,
            }
        );
        assert!(parsed.in_sync);
        let next = read_one(&mut stream, &mut limits);
        assert_eq!(next.reply, Reply::Status("NEXT".to_string()));
    }
    #[test]
    fn rejects_unexpected_type_bytes() {
        assert_eq!(
            protocol_detail(b"%1\r\n+a\r\n+b\r\n"),
            "unexpected reply type byte 0x25"
        );
        assert_eq!(
            protocol_detail(b"*1\r\n!3\r\n"),
            "unexpected reply type byte 0x21"
        );
        let mut limits = ReplyLimits {
            max_items: 0,
            max_bytes: 100,
        };
        let skipped = read_reply(&mut Cursor::new(b"*1\r\n_\r\n".to_vec()), &mut limits);
        assert!(matches!(skipped, Err(RedisError::Protocol(_))));
    }
    #[test]
    fn limits_nesting_to_32_levels() {
        let mut nested = "*1\r\n".repeat(32).into_bytes();
        nested.extend_from_slice(b":1\r\n");
        let mut limits = WIDE;
        assert!(read_reply(&mut Cursor::new(nested), &mut limits).is_ok());
        let mut too_deep = "*1\r\n".repeat(33).into_bytes();
        too_deep.extend_from_slice(b":1\r\n");
        assert_eq!(
            protocol_detail(&too_deep),
            "reply nesting is deeper than 32 levels"
        );
    }
    #[test]
    fn caps_reply_lines_at_64_kib() {
        let mut line = b"+".to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES - 1));
        line.extend_from_slice(b"\r\n");
        let mut limits = WIDE;
        assert!(read_reply(&mut Cursor::new(line), &mut limits).is_ok());
        let mut long_line = b"+".to_vec();
        long_line.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES));
        long_line.extend_from_slice(b"\r\n");
        assert_eq!(
            protocol_detail(&long_line),
            "reply line is longer than 65536 bytes"
        );
    }
    #[test]
    fn rejects_bulk_past_512_mib_and_bad_lengths() {
        assert!(protocol_detail(b"$536870913\r\n").contains("past the limit"));
        assert_eq!(protocol_detail(b"$-2\r\n"), "invalid reply length -2");
        assert_eq!(protocol_detail(b"$x\r\n"), "invalid reply length");
        assert_eq!(protocol_detail(b":1.5\r\n"), "invalid integer reply");
    }
    #[test]
    fn stops_past_the_64_mib_discard_limit() {
        // 1. 한도 이내 폐기
        let mut input = b"$1048576\r\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', 1_048_576));
        input.extend_from_slice(b"\r\n+NEXT\r\n");
        let mut stream = Cursor::new(input);
        let mut limits = ReplyLimits {
            max_items: 10,
            max_bytes: 0,
        };
        let parsed = read_one(&mut stream, &mut limits);
        assert!(parsed.in_sync);
        assert_eq!(
            parsed.reply,
            Reply::BulkCut {
                prefix: Vec::new(),
                total: 1_048_576,
            }
        );
        let next = read_one(&mut stream, &mut limits);
        assert_eq!(next.reply, Reply::Status("NEXT".to_string()));

        // 2. 단일 bulk 한도 초과
        let mut stream = Cursor::new(b"$70000000\r\nab".to_vec());
        let mut limits = ReplyLimits {
            max_items: 10,
            max_bytes: 2,
        };
        let parsed = read_one(&mut stream, &mut limits);
        assert!(!parsed.in_sync);
        assert_eq!(
            parsed.reply,
            Reply::BulkCut {
                prefix: b"ab".to_vec(),
                total: 70_000_000,
            }
        );

        // 3. 건너뛴 배열 요소 한도 초과
        let mut stream = Cursor::new(b"*2\r\n:1\r\n$70000000\r\n".to_vec());
        let mut limits = ReplyLimits {
            max_items: 1,
            max_bytes: 10,
        };
        let parsed = read_one(&mut stream, &mut limits);
        assert!(!parsed.in_sync);
        assert_eq!(
            parsed.reply,
            Reply::ArrayCut {
                items: vec![Reply::Integer(1)],
                total: 2,
            }
        );
    }
}
