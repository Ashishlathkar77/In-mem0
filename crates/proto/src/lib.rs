//! `inmem-proto` — RESP2/RESP3 wire protocol (ADR-001 D7).
//!
//! - [`parse_command`] reads a client request (an array of bulk strings, or an inline command)
//!   from a byte buffer, returning the argument vector and the number of bytes consumed.
//! - [`Reply`] is a server reply value; [`Reply::encode`] serializes it as RESP2 or RESP3.
//!
//! The parser is incremental: it returns `Ok(None)` when the buffer holds only a partial frame,
//! so the server can keep reading without losing position.

mod resp;

pub use resp::{
    parse_command, parse_command_ranges, write_bulk, write_error, write_int, write_null,
    write_simple, Command, ProtocolError, Reply,
};
