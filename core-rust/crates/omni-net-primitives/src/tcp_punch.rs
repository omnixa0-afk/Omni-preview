//! Sockets for the TCP hole punch: the door's listener, and every socket that must leave from the
//! door's port — the STUN query that learns the door's external address, the pinhole-opening SYN of
//! the listening side, and the dialing side's connect. See docs/decisions/tcp-hole-punch.md.
//!
//! All of them share one local port, so all of them carry `SO_REUSEADDR` (and `SO_REUSEPORT` on
//! unix), the listener included: on Windows a socket with the flag does not bind next to a listener
//! without it.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::stun::{build_binding_request, fresh_txid, parse_mapped_address, StunProbe};

/// Backlog of the door's listener.
const DOOR_LISTEN_BACKLOG: i32 = 128;
/// Largest STUN response read back (header + attributes); a real one is under 100 bytes.
const MAX_STUN_RESPONSE: usize = 548;

fn reusable_tcp_socket() -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    Ok(socket)
}

fn any_v4(port: u16) -> SockAddr {
    SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
}

/// Whether `port` is free for an ordinary bind — the door's own check before it binds with
/// `SO_REUSEADDR`, which on Windows (and `SO_REUSEPORT` on unix) would let it share a port another
/// process (a second instance on the same host) is already listening on, splitting that door's
/// connections between the two.
fn port_is_free(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

/// The door's listener on `0.0.0.0:port` (`0` — any free port), ready for the sockets that leave from
/// the same port. A specific `port` that something else holds is an error, as an ordinary bind would
/// be. Non-blocking, for `tokio::net::TcpListener::from_std`.
pub fn bind_door_listener(port: u16) -> io::Result<TcpListener> {
    if port != 0 && !port_is_free(port) {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("TCP port {port} is in use")));
    }
    let socket = reusable_tcp_socket()?;
    socket.bind(&any_v4(port))?;
    socket.listen(DOOR_LISTEN_BACKLOG)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// A TCP socket bound to the door's `port`, not connected, non-blocking — the source of every
/// connection that has to come from the door's external address. `ttl` sets the IP TTL of what it
/// sends (the pinhole SYN; `None` keeps the system's). Closing it resets the connection instead of
/// leaving it in TIME_WAIT, so the same four-tuple can be used again at once.
pub fn door_port_socket(port: u16, ttl: Option<u32>) -> io::Result<Socket> {
    let socket = reusable_tcp_socket()?;
    if let Some(ttl) = ttl {
        socket.set_ttl(ttl)?;
    }
    socket.set_linger(Some(Duration::ZERO))?;
    socket.bind(&any_v4(port))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// Connect from the door's `port` to `to`, within `limit`. The stream a punch dialer hands to the
/// door tunnel.
pub async fn connect_from_door_port(
    port: u16,
    to: SocketAddr,
    limit: Duration,
) -> io::Result<tokio::net::TcpStream> {
    let socket = door_port_socket(port, None)?;
    let socket = tokio::net::TcpSocket::from_std_stream(std::net::TcpStream::from(socket));
    match tokio::time::timeout(limit, socket.connect(to)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, format!("tcp connect {to} timed out"))),
    }
}

/// Send one SYN from the door's `port` towards `to` with IP TTL `ttl`, wait `hold` and drop the
/// socket. The SYN makes our own NAT hold a mapping for `to`, so its connect to our door gets through;
/// the low TTL keeps it from reaching `to`'s NAT, which would answer it with a reset. `Ok(true)` if the
/// socket connected anyway (the peer's SYN crossed ours: a simultaneous open, which this pair's NATs
/// do not carry data over, so the connection is closed, not used).
pub async fn send_pinhole_syn(port: u16, to: SocketAddr, ttl: u32, hold: Duration) -> io::Result<bool> {
    let socket = door_port_socket(port, Some(ttl))?;
    let socket = tokio::net::TcpSocket::from_std_stream(std::net::TcpStream::from(socket));
    Ok(matches!(tokio::time::timeout(hold, socket.connect(to)).await, Ok(Ok(_))))
}

/// The external address a STUN server over TCP sees for a connection from the door's `port`.
pub async fn tcp_stun_from_port(port: u16, server: SocketAddr, limit: Duration) -> io::Result<SocketAddr> {
    let query = async {
        let mut stream = connect_from_door_port(port, server, limit).await?;
        let txid = fresh_txid();
        stream.write_all(&build_binding_request(&txid)).await?;
        let mut header = [0u8; 20];
        stream.read_exact(&mut header).await?;
        let body_len = u16::from_be_bytes([header[2], header[3]]) as usize;
        if 20 + body_len > MAX_STUN_RESPONSE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "STUN response too long"));
        }
        let mut response = header.to_vec();
        response.resize(20 + body_len, 0);
        stream.read_exact(&mut response[20..]).await?;
        parse_mapped_address(&response, &txid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no mapped address in STUN response"))
    };
    match tokio::time::timeout(limit, query).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, format!("STUN over TCP to {server} timed out"))),
    }
}

/// Ask each of `servers` (resolved IPv4 addresses) over TCP from the door's `port`, one after another
/// (the servers must see the same local socket for the answers to be comparable). Stops once two
/// answers agree exactly — that is all [`crate::stun::tcp_punch_consensus`] needs.
pub async fn tcp_stun_probe_from_port(port: u16, servers: &[SocketAddr], limit: Duration) -> StunProbe {
    let mut probe = StunProbe { asked: 0, answers: Vec::new() };
    for server in servers {
        probe.asked += 1;
        if let Ok(answer) = tcp_stun_from_port(port, *server, limit).await {
            probe.answers.push(answer);
            if crate::stun::tcp_punch_consensus(&probe.answers).is_some() {
                break;
            }
        }
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the punch rests on: with the door listening on its port, a socket from the same
    /// port connects out, and the listener still accepts.
    #[tokio::test]
    async fn a_connection_leaves_from_the_door_port_while_the_door_listens() {
        let door = tokio::net::TcpListener::from_std(bind_door_listener(0).unwrap()).unwrap();
        let door_port = door.local_addr().unwrap().port();
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = elsewhere.local_addr().unwrap();

        let (outgoing, accepted) = tokio::join!(
            connect_from_door_port(door_port, target, Duration::from_secs(5)),
            elsewhere.accept()
        );
        let outgoing = outgoing.expect("connect from the door's port");
        assert_eq!(outgoing.local_addr().unwrap().port(), door_port, "left from the door's port");
        assert_eq!(accepted.unwrap().1.port(), door_port, "the far end sees the door's port");

        let door_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, door_port));
        let (dialed, accepted) = tokio::join!(tokio::net::TcpStream::connect(door_addr), door.accept());
        dialed.expect("the door still accepts");
        accepted.expect("accepted on the door");
    }

    /// A second door on a port another listener holds is refused, as an ordinary bind is — so the
    /// runtime's fallback to an ephemeral port still happens for a second instance on one host.
    #[test]
    fn a_second_door_on_a_held_port_is_refused() {
        let first = bind_door_listener(0).unwrap();
        let port = first.local_addr().unwrap().port();
        let error = bind_door_listener(port).expect_err("the port is held");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        assert!(bind_door_listener(port).is_ok(), "free again once the first door closes");
    }

    /// STUN over TCP from the door's port reads the mapped address back (a local server that answers
    /// like a real one, with the caller's own address).
    #[tokio::test]
    async fn stun_over_tcp_reads_the_mapped_address() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, from) = server.accept().await.unwrap();
            let mut request = [0u8; 20];
            stream.read_exact(&mut request).await.unwrap();
            let SocketAddr::V4(from) = from else { panic!("v4") };
            let port = from.port() ^ 0x2112;
            let ip = u32::from(*from.ip()) ^ crate::stun::STUN_MAGIC;
            let mut response = vec![0x01, 0x01, 0x00, 0x0C];
            response.extend_from_slice(&request[4..20]);
            response.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            response.extend_from_slice(&port.to_be_bytes());
            response.extend_from_slice(&ip.to_be_bytes());
            stream.write_all(&response).await.unwrap();
        });
        let door = bind_door_listener(0).unwrap();
        let door_port = door.local_addr().unwrap().port();
        let seen = tcp_stun_from_port(door_port, server_addr, Duration::from_secs(5)).await.unwrap();
        assert_eq!(seen, SocketAddr::from((Ipv4Addr::LOCALHOST, door_port)));
    }
}
