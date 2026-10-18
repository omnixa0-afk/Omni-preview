//! The public pkarr relays — HTTPS gateways into Mainline that pkarr (Pubky) runs for clients that
//! cannot speak UDP to the swarm. A relay takes a signed BEP44 record and puts it into the DHT, and
//! answers a read from the DHT (and its cache): `PUT`/`GET https://<relay>/<z-base-32 key>`, the body
//! the signature (64 bytes), `seq` (8 bytes, big-endian) and the value — the same record our own engine
//! puts (`pkarr_shape.rs`). To the network it is plain HTTPS to a web host; KRPC itself cannot be
//! hidden, its peers must parse it. The last way in, for a network that drops DHT traffic with no
//! friend to go through (docs/decisions/dht-over-pkarr-relays.md).
//!
//! What a relay learns: the slot key (one per slot and time bucket, unlinkable to the account) and our
//! IP — what ntfy or a Nostr relay learns of a beacon. It cannot read the value (AEAD-sealed) nor forge
//! one: a record is signed by the slot's key, and we check that signature.

use std::time::Duration;

use mainline::{MutableItem, SigningKey};

/// Live 2026-10-10: both serve records of our shape (docs/decisions/libp2p-kad-vs-mainline.md).
const DEFAULT_RELAYS: &[&str] = &["https://relay.pkarr.org", "https://pkarr.pubky.app"];

/// A relay that has not answered in this long is not answering.
const RELAY_TIMEOUT: Duration = Duration::from_secs(8);

/// The relays to use: `OMNI_PKARR_RELAYS` (comma-separated base URLs), else the public ones.
pub(crate) fn relays() -> Vec<String> {
    match std::env::var("OMNI_PKARR_RELAYS") {
        Ok(list) if !list.trim().is_empty() => list
            .split(',')
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .collect(),
        _ => DEFAULT_RELAYS.iter().map(|url| url.to_string()).collect(),
    }
}

/// What the relays hold for a key.
pub(crate) enum Fetched {
    /// The freshest record any of them had, its signature checked.
    Item(MutableItem),
    /// They answered and had none (or only records that are not ours).
    Empty,
    /// None answered.
    Unreachable,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(RELAY_TIMEOUT)
        .timeout(RELAY_TIMEOUT)
        .build()
}

fn url(relay: &str, public_key: &[u8; 32]) -> String {
    format!("{relay}/{}", crate::pkarr_shape::z32(public_key))
}

/// The relay body of a record: signature, `seq` big-endian, value.
pub(crate) fn body(item: &MutableItem) -> Vec<u8> {
    let mut body = Vec::with_capacity(72 + item.value().len());
    body.extend_from_slice(item.signature());
    body.extend_from_slice(&(item.seq() as u64).to_be_bytes());
    body.extend_from_slice(item.value());
    body
}

/// A relay body as a record of `signer`'s slot — `None` unless that key signed it. ed25519 signs
/// deterministically and we hold the slot's key, so signing `(seq, value)` again must give the very
/// same signature.
pub(crate) fn parse(signer: &SigningKey, body: &[u8]) -> Option<MutableItem> {
    if body.len() < 72 || body.len() > 72 + crate::pkarr_shape::MAX_PACKET {
        return None;
    }
    let seq = i64::try_from(u64::from_be_bytes(body[64..72].try_into().ok()?)).ok()?;
    let item = MutableItem::new(signer.clone(), &body[72..], seq, None);
    (item.signature()[..] == body[..64]).then_some(item)
}

/// The freshest record of `signer`'s slot at any of `relays`, asked at once.
pub(crate) fn fetch(relays: &[String], signer: &SigningKey) -> Fetched {
    let public_key = signer.verifying_key().to_bytes();
    let answers: Vec<Option<Option<MutableItem>>> = std::thread::scope(|scope| {
        let asks: Vec<_> = relays
            .iter()
            .map(|relay| {
                let url = url(relay, &public_key);
                scope.spawn(move || match agent().get(&url).call() {
                    Ok(response) => {
                        let mut bytes = Vec::new();
                        use std::io::Read as _;
                        let read = response
                            .into_reader()
                            .take(72 + crate::pkarr_shape::MAX_PACKET as u64 + 1)
                            .read_to_end(&mut bytes);
                        Some(read.ok().and_then(|_| parse(signer, &bytes)))
                    }
                    // Not there: an answer all the same.
                    Err(ureq::Error::Status(404, _)) => Some(None),
                    Err(_) => None,
                })
            })
            .collect();
        asks.into_iter().map(|ask| ask.join().ok().flatten()).collect()
    });
    if answers.iter().all(Option::is_none) {
        return Fetched::Unreachable;
    }
    answers
        .into_iter()
        .flatten()
        .flatten()
        .max_by_key(MutableItem::seq)
        .map_or(Fetched::Empty, Fetched::Item)
}

/// Hand `item` to every relay at once; `true` if any took it.
pub(crate) fn publish(relays: &[String], item: &MutableItem) -> bool {
    let body = body(item);
    let url_key = item.key();
    std::thread::scope(|scope| {
        let puts: Vec<_> = relays
            .iter()
            .map(|relay| {
                let url = url(relay, url_key);
                let body = &body;
                scope.spawn(move || {
                    agent()
                        .put(&url)
                        .set("Content-Type", "application/pkarr.org/relays#payload")
                        .send_bytes(body)
                        .is_ok()
                })
            })
            .collect();
        puts.into_iter().map(|put| put.join().unwrap_or(false)).fold(false, |any, ok| any | ok)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_body_is_taken_back_only_when_the_slot_key_signed_it() {
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let item = MutableItem::new(signer.clone(), b"a pkarr packet", 1_791_000_000_000_000, None);
        let wire = body(&item);
        assert_eq!(wire.len(), 64 + 8 + 14);
        assert_eq!(&wire[64..72], &1_791_000_000_000_000u64.to_be_bytes(), "seq big-endian after the signature");

        let back = parse(&signer, &wire).expect("our own record");
        assert_eq!((back.seq(), back.value()), (item.seq(), item.value()));

        let mut changed = wire.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(parse(&signer, &changed).is_none(), "a changed value");
        let mut reseq = wire.clone();
        reseq[71] ^= 1;
        assert!(parse(&signer, &reseq).is_none(), "a changed seq");
        assert!(parse(&SigningKey::from_bytes(&[8u8; 32]), &wire).is_none(), "another slot's key");
        assert!(parse(&signer, &wire[..60]).is_none(), "cut short");
    }

    #[test]
    fn relays_come_from_the_environment_or_are_the_public_ones() {
        // One test owns the variable: tests run in parallel.
        std::env::remove_var("OMNI_PKARR_RELAYS");
        assert_eq!(relays(), vec!["https://relay.pkarr.org".to_string(), "https://pkarr.pubky.app".to_string()]);
        std::env::set_var("OMNI_PKARR_RELAYS", "http://127.0.0.1:6881/, https://r.example ");
        assert_eq!(relays(), vec!["http://127.0.0.1:6881".to_string(), "https://r.example".to_string()]);
        std::env::remove_var("OMNI_PKARR_RELAYS");
    }

    /// A relay on loopback that keeps what is PUT under its path and serves it back.
    fn fake_relay() -> String {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut kept: std::collections::HashMap<String, Vec<u8>> = Default::default();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut parts = request.split_whitespace();
                let (method, path) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let reply = match (method.as_str(), kept.get(&path)) {
                    ("PUT", _) => {
                        kept.insert(path, body);
                        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec()
                    }
                    ("GET", Some(stored)) => {
                        let mut reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", stored.len()).into_bytes();
                        reply.extend_from_slice(stored);
                        reply
                    }
                    _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = stream.write_all(&reply);
            }
        });
        url
    }

    /// The record goes up to a relay and comes back from it as the same signed item; a slot nobody
    /// wrote is empty, not unreachable.
    #[test]
    fn a_record_round_trips_through_a_relay() {
        let relays = vec![fake_relay()];
        let signer = SigningKey::from_bytes(&[5u8; 32]);
        assert!(matches!(fetch(&relays, &signer), Fetched::Empty), "nothing there yet");
        let item = MutableItem::new(signer.clone(), b"packet", 42, None);
        assert!(publish(&relays, &item));
        let Fetched::Item(back) = fetch(&relays, &signer) else { panic!("the relay has it") };
        assert_eq!((back.seq(), back.value()), (42, &b"packet"[..]));
        // A relay that is down next to one that is up: the answer of the one that is up.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let down = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        assert!(matches!(fetch(&[down, relays[0].clone()], &signer), Fetched::Item(_)));
    }

    /// A relay that is down is "unreachable", not "empty" — the caller must not take an outage for a
    /// free slot.
    #[test]
    fn no_relay_answering_is_unreachable() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        assert!(matches!(fetch(std::slice::from_ref(&url), &signer), Fetched::Unreachable));
        let item = MutableItem::new(signer, b"x", 1, None);
        assert!(!publish(&[url], &item));
    }
}
