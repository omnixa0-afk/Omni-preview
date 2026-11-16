//! Whether an address is a public, internet-routable unicast one (V-22).
//!
//! What a third party names — a relay directory entry, a contact's courier — is used only if it
//! points out to the internet, never into the user's own network or host. An address itself is
//! judged by `omni_net_primitives::is_public_ip`.

use std::net::IpAddr;

use omni_net_primitives::is_public_ip;

/// Ports a courier someone else named may use: the web's own, and their usual alternates.
const PEER_COURIER_PORTS: [u16; 4] = [80, 443, 8080, 8443];

/// Whether a courier URL that someone else named (a contact's courier set, V-13) points out to
/// the internet: an `http(s)://` or `ws(s)://` URL with no user part, a host that is a public
/// address or a name that is not a local one (`localhost`, a single label, `.local`, `.lan`,
/// `.internal`, `.home.arpa`…, or a name ending in a number, which URL parsers read as IPv4),
/// and one of [`PEER_COURIER_PORTS`] or none; or `dht://mainline`. A name is checked again where
/// it is resolved — the helper takes only public addresses for such a courier.
pub fn is_public_courier_url(url: &str) -> bool {
    let url = url.trim();
    if url == "dht://mainline" {
        return true;
    }
    let Some(rest) = ["https://", "http://", "wss://", "ws://"].iter().find_map(|scheme| url.strip_prefix(scheme)) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') || authority.contains('\\') {
        return false;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((v6, after)) => (v6, after.strip_prefix(':')),
            None => return false,
        },
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if let Some(port) = port {
        if !port.parse::<u16>().is_ok_and(|port| PEER_COURIER_PORTS.contains(&port)) {
            return false;
        }
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(ip);
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let last = labels.last().copied().unwrap_or("");
    let valid = !host.is_empty()
        && labels.iter().all(|label| !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let local_suffix = ["localhost", "local", "lan", "home", "internal", "intranet", "corp", "localdomain", "private"]
        .contains(&last)
        || host.ends_with(".home.arpa");
    let numeric_last = last.chars().all(|c| c.is_ascii_digit()) || last.starts_with("0x");
    valid && labels.len() >= 2 && !local_suffix && !numeric_last
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V-13: a contact's courier may point at a public host only — not at the victim's router,
    /// loopback or a LAN name, however the URL dresses it up.
    #[test]
    fn a_courier_url_from_someone_else_must_point_out_to_the_internet() {
        for url in [
            "https://ntfy.sh", "https://ntfy.example.org/", "wss://nos.lol", "wss://relay.example.com:443/path",
            "https://8.8.8.8:8443", "http://ntfy.example.org:8080", "https://[2606:4700::1]/", "dht://mainline",
        ] {
            assert!(is_public_courier_url(url), "{url}");
        }
        for url in [
            "http://192.168.1.1/", "https://127.0.0.1", "wss://[::1]:443", "https://[::ffff:10.0.0.1]",
            "http://localhost", "https://router.localhost", "https://printer.local", "https://nas.lan",
            "https://intranet", "https://box.home.arpa", "http://2130706433", "http://0x7f.1", "http://127.1",
            "https://user@ntfy.sh", "https://ntfy.sh@192.168.1.1", "https://ntfy.sh:22", "https://ntfy.sh:6379",
            "ftp://ntfy.sh", "dht://192.168.1.1", "https://", "https://[::1", "https://bad_host.example.org",
        ] {
            assert!(!is_public_courier_url(url), "{url}");
        }
    }
}
