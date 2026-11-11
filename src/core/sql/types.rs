//! types.rs
//! core::sql::types
//!
//! Public data types of the MySQL client: connect options, row limits, result sets, and statement summaries.
//! Result values stay raw bytes (`None` = SQL NULL); the caller maps them with `column_type` and the flags.
//!

use std::fmt;
use std::time::Duration;

// 1. Public types -------------------------------------------------------------
#[derive(Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: Option<String>,
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
            .field("database", &self.database)
            .field("connect_timeout", &self.connect_timeout)
            .field("io_timeout", &self.io_timeout)
            .finish()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowLimits {
    pub max_rows: usize,
    // 보관 값 바이트 합계 상한
    pub max_bytes: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub column_type: u8,
    pub flags: u16,
    pub charset: u16,
    pub decimals: u8,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    // None = SQL NULL
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    // 상한 도달 후 잔여 행 존재
    pub truncated: bool,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatementSummary {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub warnings: u16,
    pub info: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatementOutcome {
    Rows(ResultSet),
    Done(StatementSummary),
}

// 2. Column metadata constants ------------------------------------------------
pub mod column_type {
    pub const DECIMAL: u8 = 0x00;
    pub const TINY: u8 = 0x01;
    pub const SHORT: u8 = 0x02;
    pub const LONG: u8 = 0x03;
    pub const FLOAT: u8 = 0x04;
    pub const DOUBLE: u8 = 0x05;
    pub const NULL: u8 = 0x06;
    pub const TIMESTAMP: u8 = 0x07;
    pub const LONGLONG: u8 = 0x08;
    pub const INT24: u8 = 0x09;
    pub const DATE: u8 = 0x0A;
    pub const TIME: u8 = 0x0B;
    pub const DATETIME: u8 = 0x0C;
    pub const YEAR: u8 = 0x0D;
    pub const NEWDATE: u8 = 0x0E;
    pub const VARCHAR: u8 = 0x0F;
    pub const BIT: u8 = 0x10;
    pub const TIMESTAMP2: u8 = 0x11;
    pub const DATETIME2: u8 = 0x12;
    pub const TIME2: u8 = 0x13;
    pub const TYPED_ARRAY: u8 = 0x14;
    pub const VECTOR: u8 = 0xF2;
    pub const INVALID: u8 = 0xF3;
    pub const BOOL: u8 = 0xF4;
    pub const JSON: u8 = 0xF5;
    pub const NEWDECIMAL: u8 = 0xF6;
    pub const ENUM: u8 = 0xF7;
    pub const SET: u8 = 0xF8;
    pub const TINY_BLOB: u8 = 0xF9;
    pub const MEDIUM_BLOB: u8 = 0xFA;
    pub const LONG_BLOB: u8 = 0xFB;
    pub const BLOB: u8 = 0xFC;
    pub const VAR_STRING: u8 = 0xFD;
    pub const STRING: u8 = 0xFE;
    pub const GEOMETRY: u8 = 0xFF;
}
pub const BINARY_CHARSET: u16 = 63;
pub const FLAG_UNSIGNED: u16 = 0x20;
pub const FLAG_BINARY: u16 = 0x80;
