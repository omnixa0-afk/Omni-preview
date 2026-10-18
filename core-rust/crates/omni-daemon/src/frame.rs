//! Length-prefixed JSON framing for the daemon's framed protocol mode.
//!
//! The default protocol is one request line in → one `OK ...` / `ERR ...` line out, with
//! a separate `watch` connection for server→client pushes. That line protocol can't carry
//! a request id (so a client can't pipeline requests on one connection and match replies),
//! can't carry a body with newlines unambiguously, and needs a second connection for events.
//!
//! Framed mode fixes all three: a connection sends the line `frame` (handshake), the server
//! replies `OK framed`, and from then on the connection speaks length-prefixed JSON frames:
//! a 4-byte big-endian length prefix followed by that many UTF-8 JSON bytes.
//!
//!   * client→server **request**:  `{"id": <u64>, "cmd": "<command>", "arg": "<rest>"}`
//!   * server→client **response**: `{"id": <u64>, "ok": <bool>, "text": "<reply>"}`  (id echoes the request)
//!   * server→client **event**:    `{"event": "message"|"linkstate"|"call", ...}`     (push; no id — sub-increment B)
//!
//! A client distinguishes a response from a pushed event by the field present (`ok` vs
//! `event`), and correlates responses to requests by `id`. Both directions ride one
//! connection. See docs/decisions/daemon-ipc-phase2.md §Protocol.

use std::io::{self, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

/// Hard cap on a single frame's JSON body, so a malformed/hostile length prefix can't make
/// the daemon allocate unbounded memory. 1 MiB is far above any real command/response.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// A client→server command request. `arg` is everything after the command word (may be
/// empty) — same split as the line protocol's `cmd`/`rest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub arg: String,
}

/// A server→client response. `id` echoes the request's `id`; `text` is the full handler
/// reply string (e.g. `"OK b7a3…"` / `"ERR no account"`); `ok` is the convenience boolean
/// `!text.starts_with("ERR")`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: u64,
    pub ok: bool,
    pub text: String,
}

/// A server→client PUSH (no `id`) — the events plane on a framed connection. A client tells
/// a response from a push by the field present: a response has `ok`, an event has `event`.
/// The variant tag is the `event` field (`"message"`/`"linkstate"`/`"call"`/`"ping"`); this
/// is the structured twin of the line protocol's `EVENT ...` lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum EventFrame {
    Message { dialog: String, id: String, body: String },
    Linkstate { peer: String, up: bool },
    Call { dialog: String, call_id: String, state: String },
    /// A group's membership, name or our invitations changed.
    Group { group_id: String },
    /// Heartbeat on idle, so a dead framed watcher is detected on write (like the line
    /// protocol's `EVENT ping`).
    Ping,
}

/// Write one length-prefixed JSON frame (4-byte BE length + UTF-8 body) and flush.
fn write_json_frame<W: Write>(writer: &mut W, json: &str) -> io::Result<()> {
    let bytes = json.as_bytes();
    if bytes.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(ErrorKind::InvalidData, "frame too large to send"));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

/// Read one length-prefixed JSON frame. `Ok(None)` is a clean EOF at a frame boundary
/// (peer closed the connection); any other short read or oversize prefix is an error.
/// Public so a watch client can print raw frame JSON without typing every variant.
pub fn read_json_frame<R: Read>(reader: &mut R) -> io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        // EOF exactly at a frame boundary = peer hung up cleanly.
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(ErrorKind::InvalidData, "frame length exceeds cap"));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    String::from_utf8(body)
        .map(Some)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "frame body is not UTF-8"))
}

/// Client side: send one request frame.
pub fn write_request<W: Write>(writer: &mut W, request: &RequestFrame) -> io::Result<()> {
    let json = serde_json::to_string(request).map_err(io::Error::other)?;
    write_json_frame(writer, &json)
}

/// Server side: read one request frame (`Ok(None)` = clean EOF).
pub fn read_request<R: Read>(reader: &mut R) -> io::Result<Option<RequestFrame>> {
    match read_json_frame(reader)? {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error)),
        None => Ok(None),
    }
}

/// Server side: send one response frame.
pub fn write_response<W: Write>(writer: &mut W, id: u64, text: &str) -> io::Result<()> {
    let frame = ResponseFrame {
        id,
        ok: !text.starts_with("ERR"),
        text: text.to_string(),
    };
    let json = serde_json::to_string(&frame).map_err(io::Error::other)?;
    write_json_frame(writer, &json)
}

/// Client side: read one response frame (`Ok(None)` = clean EOF).
pub fn read_response<R: Read>(reader: &mut R) -> io::Result<Option<ResponseFrame>> {
    match read_json_frame(reader)? {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error)),
        None => Ok(None),
    }
}

/// Server side: push one event frame (server→client, no id).
pub fn write_event<W: Write>(writer: &mut W, event: &EventFrame) -> io::Result<()> {
    let json = serde_json::to_string(event).map_err(io::Error::other)?;
    write_json_frame(writer, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_the_codec() {
        let mut buf: Vec<u8> = Vec::new();
        write_request(
            &mut buf,
            &RequestFrame { id: 7, cmd: "send".into(), arg: "acctB hello world".into() },
        )
        .unwrap();
        let got = read_request(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(got.id, 7);
        assert_eq!(got.cmd, "send");
        assert_eq!(got.arg, "acctB hello world");
    }

    #[test]
    fn response_round_trips_and_ok_tracks_err_prefix() {
        let mut buf: Vec<u8> = Vec::new();
        write_response(&mut buf, 7, "OK b7a3").unwrap();
        write_response(&mut buf, 8, "ERR no account").unwrap();
        let mut cursor = &buf[..];
        let first = read_response(&mut cursor).unwrap().unwrap();
        let second = read_response(&mut cursor).unwrap().unwrap();
        assert_eq!((first.id, first.ok, first.text.as_str()), (7, true, "OK b7a3"));
        assert_eq!((second.id, second.ok, second.text.as_str()), (8, false, "ERR no account"));
    }

    #[test]
    fn missing_arg_defaults_to_empty() {
        // A request frame without an `arg` field decodes with arg == "".
        let json = br#"{"id":1,"cmd":"account-id"}"#;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&(json.len() as u32).to_be_bytes());
        buf.extend_from_slice(json);
        let got = read_request(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(got.cmd, "account-id");
        assert_eq!(got.arg, "");
    }

    #[test]
    fn clean_eof_at_frame_boundary_is_none() {
        let empty: &[u8] = &[];
        assert!(read_request(&mut &empty[..]).unwrap().is_none());
        // ...even after a complete frame was consumed.
        let mut buf: Vec<u8> = Vec::new();
        write_request(&mut buf, &RequestFrame { id: 1, cmd: "quit".into(), arg: String::new() }).unwrap();
        let mut cursor = &buf[..];
        assert!(read_request(&mut cursor).unwrap().is_some());
        assert!(read_request(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn oversize_length_prefix_is_rejected_without_allocating() {
        // A 4-byte prefix claiming > MAX_FRAME_LEN must error, not try to allocate it.
        let big = (MAX_FRAME_LEN as u32) + 1;
        let mut buf: Vec<u8> = big.to_be_bytes().to_vec();
        buf.push(0); // a stray byte; we should never get far enough to read it
        let err = read_request(&mut &buf[..]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_body_is_an_error_not_a_partial_frame() {
        // Prefix says 10 bytes, only 3 follow → UnexpectedEof bubbles as an error.
        let mut buf: Vec<u8> = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"abc");
        assert!(read_request(&mut &buf[..]).is_err());
    }

    #[test]
    fn event_frame_wire_shape_is_tagged_by_lowercase_event() {
        // The push wire shape is pinned: an `event` tag + the kind's fields, and it carries
        // no `id`/`ok` (that's how a client tells a push from a response).
        let json = serde_json::to_string(&EventFrame::Message {
            dialog: "d".into(),
            id: "m1".into(),
            body: "hi".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"event":"message","dialog":"d","id":"m1","body":"hi"}"#);
        assert_eq!(
            serde_json::to_string(&EventFrame::Linkstate { peer: "p".into(), up: true }).unwrap(),
            r#"{"event":"linkstate","peer":"p","up":true}"#
        );
        assert_eq!(serde_json::to_string(&EventFrame::Ping).unwrap(), r#"{"event":"ping"}"#);
    }
}
