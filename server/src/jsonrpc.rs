//! JSON-RPC framing, and the smallest message type a proxy can get away with.
//!
//! Messages stay [`serde_json::Value`] rather than becoming typed structs. That
//! is deliberate: most of what passes through here is forwarded to or from a
//! stock luau-lsp, and forwarding opaquely is what keeps us decoupled from its
//! protocol surface. Deserialising into our own structs would silently drop
//! every field we did not think of.

use serde_json::Value;
use std::io::{self, BufRead, Read, Write};

/// One JSON-RPC message, classified only as far as routing needs.
#[derive(Debug, Clone)]
pub enum Message {
    Request { id: Value, method: String, params: Value },
    Response { id: Value, body: Value },
    Notification { method: String, params: Value },
}

impl Message {
    pub fn from_value(value: Value) -> Option<Self> {
        let object = value.as_object()?;
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();

        Some(match (method, id) {
            (Some(method), Some(id)) => Message::Request {
                id,
                method: method.to_string(),
                params: object.get("params").cloned().unwrap_or(Value::Null),
            },
            (Some(method), None) => Message::Notification {
                method: method.to_string(),
                params: object.get("params").cloned().unwrap_or(Value::Null),
            },
            (None, Some(id)) => Message::Response { id, body: value },
            // No method and no id is not addressable in either direction.
            (None, None) => return None,
        })
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Message::Request { method, .. } | Message::Notification { method, .. } => Some(method),
            Message::Response { .. } => None,
        }
    }
}

/// Reads `Content-Length`-framed messages from a stream.
pub struct Reader<R: BufRead> {
    inner: R,
}

impl<R: BufRead> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// The next message, or `None` at end of stream.
    ///
    /// A malformed frame is an error rather than a skip: once the length header
    /// is wrong the stream is no longer parseable, and pretending otherwise
    /// turns one bad message into an infinite loop.
    pub fn read(&mut self) -> io::Result<Option<Value>> {
        let mut length: Option<usize> = None;

        loop {
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                return Ok(None);
            }

            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                break;
            }

            if let Some(value) = header(line, "content-length") {
                length = value.trim().parse().ok();
            }
        }

        let Some(length) = length else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message header carried no Content-Length",
            ));
        };

        let mut body = vec![0u8; length];
        self.inner.read_exact(&mut body)?;

        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn header<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (key, value) = line.split_once(':')?;
    key.trim().eq_ignore_ascii_case(name).then_some(value)
}

/// Writes `Content-Length`-framed messages to a stream.
pub struct Writer<W: Write> {
    inner: W,
}

impl<W: Write> Writer<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn write(&mut self, message: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(message)?;
        write!(self.inner, "Content-Length: {}\r\n\r\n", body.len())?;
        self.inner.write_all(&body)?;
        self.inner.flush()
    }
}

/// Convenience constructors for the messages we originate.
pub mod build {
    use serde_json::{json, Value};

    pub fn request(id: Value, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    pub fn notification(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    }

    pub fn result(id: Value, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    pub fn error(id: Value, code: i64, message: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
    }

    /// JSON-RPC "method not found". Sent when a request reaches us that neither
    /// we nor the child can answer.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// LSP "content modified" — the right answer when a document changed out
    /// from under an in-flight request.
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// JSON-RPC "internal error". Ours, when a handler panics.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Reads a `Read` that is not buffered, for callers holding raw pipes.
pub fn reader<R: Read>(inner: R) -> Reader<io::BufReader<R>> {
    Reader::new(io::BufReader::new(inner))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(value: Value) -> Value {
        let mut buffer = Vec::new();
        Writer::new(&mut buffer).write(&value).expect("write");
        Reader::new(buffer.as_slice()).read().expect("read").expect("a message")
    }

    #[test]
    fn frames_and_unframes_a_message() {
        let value = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
        assert_eq!(round_trip(value.clone()), value);
    }

    #[test]
    fn counts_bytes_not_characters() {
        // A Content-Length in characters truncates the frame and desynchronises
        // everything after it.
        let value = json!({ "text": "héllo — ünïcode 😀" });
        assert_eq!(round_trip(value.clone()), value);
    }

    #[test]
    fn reads_consecutive_messages() {
        let mut buffer = Vec::new();
        let mut writer = Writer::new(&mut buffer);
        writer.write(&json!({ "id": 1 })).expect("write");
        writer.write(&json!({ "id": 2 })).expect("write");

        let mut reader = Reader::new(buffer.as_slice());
        assert_eq!(reader.read().expect("read").expect("first")["id"], json!(1));
        assert_eq!(reader.read().expect("read").expect("second")["id"], json!(2));
        assert!(reader.read().expect("read").is_none());
    }

    #[test]
    fn tolerates_extra_headers_and_casing() {
        let body = br#"{"id":7}"#;
        let framed = format!(
            "Content-Type: application/vscode-jsonrpc\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );

        let value = Reader::new(framed.as_bytes()).read().expect("read").expect("a message");
        assert_eq!(value["id"], json!(7));
    }

    #[test]
    fn classifies_the_three_shapes() {
        let request = Message::from_value(json!({ "id": 1, "method": "a" })).expect("request");
        assert!(matches!(request, Message::Request { .. }));

        let notification = Message::from_value(json!({ "method": "a" })).expect("notification");
        assert!(matches!(notification, Message::Notification { .. }));

        let response = Message::from_value(json!({ "id": 1, "result": null })).expect("response");
        assert!(matches!(response, Message::Response { .. }));

        // A null id with a method is still a request — `id: null` is legal JSON-RPC.
        let null_id = Message::from_value(json!({ "id": null, "method": "a" })).expect("request");
        assert!(matches!(null_id, Message::Request { .. }));
    }
}
