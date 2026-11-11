//! statement.rs
//! core::sql::statement
//!
//! `COM_QUERY` text protocol: statement execution, result-set reading under row limits, and ERR packet parsing.
//! Multi-result and LOCAL INFILE responses are refused; a truncated result leaves the connection not reusable.
//!

use super::connection::Connection;
use super::error::MysqlError;
use super::packet::{take_bytes, take_lenenc_int, take_uint};
use super::types::{ColumnMeta, ResultSet, RowLimits, StatementOutcome, StatementSummary};

const SERVER_MORE_RESULTS_EXISTS: u64 = 0x0008;
const COM_QUERY: u8 = 0x03;
const MAX_COLUMNS: u64 = 4096;

// 1. Statements ---------------------------------------------------------------
impl Connection {
    pub fn run_statement(
        &mut self,
        sql: &str,
        limits: &RowLimits,
    ) -> Result<StatementOutcome, MysqlError> {
        // 1. 재사용 불가 연결
        if !self.reusable {
            return Err(MysqlError::Protocol("connection is not reusable".into()));
        }

        // 2. COM_QUERY 전송 및 응답 수신
        self.sequence = 0;
        let outcome = self
            .write_packet(&[COM_QUERY], sql.as_bytes())
            .and_then(|()| self.read_statement_outcome(limits));

        // 3. 입출력·프로토콜 오류 반영
        if matches!(outcome, Err(MysqlError::Io(_) | MysqlError::Protocol(_))) {
            self.reusable = false;
        }
        outcome
    }
    fn read_statement_outcome(
        &mut self,
        limits: &RowLimits,
    ) -> Result<StatementOutcome, MysqlError> {
        let payload = self.read_packet()?;
        let mut input = payload.as_slice();
        match take_uint(&mut input, 1)? {
            // 1. OK 패킷
            0x00 => {
                let affected_rows = take_lenenc_int(&mut input)?;
                let last_insert_id = take_lenenc_int(&mut input)?;
                let status = take_uint(&mut input, 2)?;
                let warnings = take_uint(&mut input, 2)? as u16;
                if status & SERVER_MORE_RESULTS_EXISTS != 0 {
                    return Err(MysqlError::Protocol(
                        "multiple result sets are not supported".into(),
                    ));
                }
                // 길이 접두 선택적
                let mut info = input;
                let mut prefixed = input;
                if let Ok(info_len) = take_lenenc_int(&mut prefixed)
                    && info_len == prefixed.len() as u64
                {
                    info = prefixed;
                }
                Ok(StatementOutcome::Done(StatementSummary {
                    affected_rows,
                    last_insert_id,
                    warnings,
                    info: String::from_utf8_lossy(info).into_owned(),
                }))
            }
            // 2. 서버 오류
            0xFF => Err(parse_server_error(&payload)),
            // 3. LOCAL INFILE 요청 거부
            0xFB => Err(MysqlError::Protocol(
                "server requested LOCAL INFILE; refused".into(),
            )),
            // 4. 결과 집합
            _ => {
                let mut header = payload.as_slice();
                let column_count = take_lenenc_int(&mut header)?;
                if !header.is_empty() || column_count == 0 || column_count > MAX_COLUMNS {
                    return Err(MysqlError::Protocol(
                        "invalid result set column count".into(),
                    ));
                }
                self.read_result_set(column_count as usize, limits)
                    .map(StatementOutcome::Rows)
            }
        }
    }
    fn read_result_set(
        &mut self,
        column_count: usize,
        limits: &RowLimits,
    ) -> Result<ResultSet, MysqlError> {
        // 1. 컬럼 정의
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let payload = self.read_packet()?;
            let mut input = payload.as_slice();
            let mut name = String::new();
            // catalog, schema, table, org_table, name, org_name
            for field_index in 0..6 {
                let field_len = take_lenenc_int(&mut input)?;
                let field = take_bytes(&mut input, field_len)?;
                if field_index == 4 {
                    name = String::from_utf8_lossy(field).into_owned();
                }
            }
            take_lenenc_int(&mut input)?;
            let charset = take_uint(&mut input, 2)? as u16;
            take_bytes(&mut input, 4)?;
            let column_type = take_uint(&mut input, 1)? as u8;
            let flags = take_uint(&mut input, 2)? as u16;
            let decimals = take_uint(&mut input, 1)? as u8;
            columns.push(ColumnMeta {
                name,
                column_type,
                flags,
                charset,
                decimals,
            });
        }

        // 2. 컬럼 종료 EOF
        let payload = self.read_packet()?;
        if payload.first() != Some(&0xFE) || payload.len() >= 9 {
            return Err(MysqlError::Protocol(
                "missing EOF after column definitions".into(),
            ));
        }

        // 3. 행 수집
        let mut result_set = ResultSet {
            columns,
            rows: Vec::new(),
            truncated: false,
        };
        let mut kept_bytes = 0usize;
        loop {
            let payload = self.read_packet()?;
            let mut input = payload.as_slice();
            match payload.first() {
                // 3-1. 종료 EOF
                Some(0xFE) if payload.len() < 9 => {
                    take_bytes(&mut input, 3)?;
                    if take_uint(&mut input, 2)? & SERVER_MORE_RESULTS_EXISTS != 0 {
                        return Err(MysqlError::Protocol(
                            "multiple result sets are not supported".into(),
                        ));
                    }
                    return Ok(result_set);
                }
                // 3-2. 전송 중 서버 오류
                Some(0xFF) => return Err(parse_server_error(&payload)),
                _ => {}
            }

            // 3-3. 상한 도달 후 잔여 행
            if result_set.rows.len() >= limits.max_rows || kept_bytes >= limits.max_bytes {
                result_set.truncated = true;
                self.reusable = false;
                return Ok(result_set);
            }

            // 3-4. 행 값
            let mut row = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                if let Some((0xFB, rest)) = input.split_first() {
                    input = rest;
                    row.push(None);
                    continue;
                }
                let cell_len = take_lenenc_int(&mut input)?;
                let cell = take_bytes(&mut input, cell_len)?;
                kept_bytes += cell.len();
                row.push(Some(cell.to_vec()));
            }
            if !input.is_empty() {
                return Err(MysqlError::Protocol("row packet has trailing bytes".into()));
            }
            result_set.rows.push(row);
        }
    }
}
pub(super) fn parse_server_error(payload: &[u8]) -> MysqlError {
    let mut input = payload.get(1..).unwrap_or_default();
    let Ok(code) = take_uint(&mut input, 2) else {
        return MysqlError::Protocol("malformed error packet".into());
    };
    // SQLSTATE 표식 선택적
    let mut sql_state = String::new();
    if let Some((b'#', marked)) = input.split_first()
        && let Some((state, message)) = marked.split_at_checked(5)
    {
        sql_state = String::from_utf8_lossy(state).into_owned();
        input = message;
    }
    MysqlError::Server {
        code: code as u16,
        sql_state,
        message: String::from_utf8_lossy(input).into_owned(),
    }
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::{OK_PAYLOAD, UNLIMITED, build_greeting, build_result_payloads};
    use super::super::fake_server::{frame_packets, spawn_fake_server};
    use super::super::handshake::NATIVE_PLUGIN;
    use super::*;

    #[test]
    fn marks_truncation_only_when_rows_remain() {
        let rows: [Vec<Option<&[u8]>>; 3] =
            [vec![Some(b"aa")], vec![Some(b"bb")], vec![Some(b"cc")]];
        // (max_rows, max_bytes, 보관 행 수, truncated)
        let cases = [
            (3, usize::MAX, 3, false),
            (2, usize::MAX, 2, true),
            (0, usize::MAX, 0, true),
            (usize::MAX, 6, 3, false),
            (usize::MAX, 5, 3, false),
            (usize::MAX, 3, 2, true),
            (usize::MAX, 0, 0, true),
        ];
        for (max_rows, max_bytes, kept_rows, truncated) in cases {
            let replies = vec![
                frame_packets(2, &[OK_PAYLOAD.to_vec()]),
                frame_packets(1, &build_result_payloads(&["code"], &rows, 2)),
            ];
            let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
            let mut connection = Connection::connect(&options).unwrap();
            let limits = RowLimits {
                max_rows,
                max_bytes,
            };
            let outcome = connection.run_statement("SELECT code", &limits).unwrap();
            let StatementOutcome::Rows(result_set) = outcome else {
                panic!("rows expected");
            };
            assert_eq!(result_set.rows.len(), kept_rows, "{limits:?}");
            assert_eq!(result_set.truncated, truncated, "{limits:?}");
            assert_eq!(connection.is_reusable(), !truncated, "{limits:?}");
            if truncated {
                assert!(matches!(
                    connection.run_statement("SELECT 1", &UNLIMITED),
                    Err(MysqlError::Protocol(_))
                ));
            }
            drop(connection);
            server.join().unwrap();
        }
    }
    #[test]
    fn refuses_unsafe_or_malformed_responses() {
        let rows: [Vec<Option<&[u8]>>; 1] = [vec![Some(b"aa")]];
        let mut failed_stream = build_result_payloads(&["code"], &rows, 2);
        failed_stream.pop();
        failed_stream.push(b"\xFF\x25\x05#70100interrupted".to_vec());
        // (응답, Protocol 오류 여부)
        let cases = [
            // LOCAL INFILE 요청
            (frame_packets(1, &[b"\xFB/etc/hosts".to_vec()]), true),
            // OK 패킷의 추가 결과 플래그
            (frame_packets(1, &[vec![0x00, 0, 0, 0x0A, 0, 0, 0]]), true),
            // 종료 EOF 의 추가 결과 플래그
            (
                frame_packets(1, &build_result_payloads(&["code"], &rows, 0x0A)),
                true,
            ),
            // 순번 불일치
            (frame_packets(5, &[OK_PAYLOAD.to_vec()]), true),
            // 잘린 컬럼 정의
            (frame_packets(1, &[vec![1], vec![3, b'd']]), true),
            // 값 뒤 잉여 바이트
            (
                frame_packets(
                    1,
                    &[
                        build_result_payloads(&["code"], &[], 2).remove(0),
                        build_result_payloads(&["code"], &[], 2).remove(1),
                        vec![0xFE, 0, 0, 2, 0],
                        vec![1, b'a', b'b'],
                    ],
                ),
                true,
            ),
            // 전송 중 서버 오류
            (frame_packets(1, &failed_stream), false),
        ];
        for (reply, protocol_error) in cases {
            let replies = vec![frame_packets(2, &[OK_PAYLOAD.to_vec()]), reply];
            let (options, server) = spawn_fake_server(build_greeting(NATIVE_PLUGIN), replies);
            let mut connection = Connection::connect(&options).unwrap();
            let error = connection
                .run_statement("SELECT code", &UNLIMITED)
                .unwrap_err();
            assert_eq!(
                matches!(error, MysqlError::Protocol(_)),
                protocol_error,
                "{error}"
            );
            assert_eq!(
                matches!(error, MysqlError::Server { code: 1317, .. }),
                !protocol_error,
                "{error}"
            );
            assert_eq!(connection.is_reusable(), !protocol_error, "{error}");
            drop(connection);
            server.join().unwrap();
        }
    }
}
