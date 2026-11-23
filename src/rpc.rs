// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The framing of LSP messages: a `Content-Length` header, then a JSON body.

use serde_json::Value;
use std::io::{self, BufRead, Write};

/// A body larger than this is refused: the frame is lost, the stream must stop.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

/// Reads the next message, null for a body that is not JSON, or `Ok(None)`
/// at the end of the stream.  An error means the frame boundary is lost and
/// the caller must stop reading.
pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length
        .filter(|length| *length <= MAX_BODY)
        .ok_or_else(|| io::Error::other("no Content-Length, or one above the limit"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// Writes BODY as one frame and flushes it.
pub fn write_body(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

/// Writes MESSAGE as one frame.
pub fn write_value(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    write_body(writer, &serde_json::to_vec(message)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn reads_headers_in_any_case_and_order() {
        let raw = "content-type: application/vscode-jsonrpc\r\ncontent-LENGTH: 2\r\n\r\n{}";
        assert_eq!(read_frame(&mut Cursor::new(raw)).unwrap(), Some(json!({})));
    }

    #[test]
    fn passes_a_body_that_is_not_json() {
        let raw = "Content-Length: 3\r\n\r\nnopContent-Length: 2\r\n\r\n{}";
        let mut reader = Cursor::new(raw);
        assert_eq!(read_frame(&mut reader).unwrap(), Some(Value::Null));
        assert_eq!(read_frame(&mut reader).unwrap(), Some(json!({})));
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn refuses_a_malformed_or_huge_length() {
        assert!(read_frame(&mut Cursor::new("Content-Length: x\r\n\r\n{}")).is_err());
        let huge = format!("Content-Length: {}\r\n\r\n{{}}", MAX_BODY + 1);
        assert!(read_frame(&mut Cursor::new(huge)).is_err());
    }

    #[test]
    fn reports_a_truncated_body() {
        let error = read_frame(&mut Cursor::new("Content-Length: 9\r\n\r\n{}")).err();
        assert_eq!(error.map(|e| e.kind()), Some(io::ErrorKind::UnexpectedEof));
    }
}
