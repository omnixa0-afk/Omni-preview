// Identity-agnostic LAN multicast socket. Extracted из
// omni-network/multicast_interface.rs (2026-05-28, Strategy B) чтобы
// и mesh discovery, и (eventually) omni-transport discovery_runtime
// reuse'или один socket-setup код.
//
// Что здесь: bind multicast UDP socket с join_multicast_v4 + loopback
// control + TTL control. Frame send/recv поверх него.
// Чего здесь НЕТ: announce semantics, policy, peer tracking — это app layer.

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::net::UdpSocket;

/// Configuration for a multicast socket.
#[derive(Clone, Debug)]
pub struct MulticastSocketConfig {
    /// Multicast group address (admin-scoped 239.0.0.0/8 recommended).
    pub group: Ipv4Addr,
    /// Port.
    pub port: u16,
    /// Local interface for BOTH halves: the group is joined on it (IP_ADD_MEMBERSHIP) and
    /// outgoing frames leave through it (IP_MULTICAST_IF). UNSPECIFIED = let the OS route.
    pub local_iface: Ipv4Addr,
    /// IP_MULTICAST_LOOP — true для in-process тестов, false для production.
    pub loopback: bool,
    /// IP_MULTICAST_TTL — 1 = только local LAN segment.
    pub ttl: u32,
}

/// A bound multicast socket ready for send/recv of raw frames.
pub struct MulticastSocket {
    socket: Arc<UdpSocket>,
    target: SocketAddrV4,
}

impl std::fmt::Debug for MulticastSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MulticastSocket")
            .field("target", &self.target)
            .finish()
    }
}

impl MulticastSocket {
    /// Bind + join the multicast group. socket2 нужен для тонкого контроля
    /// над SO_REUSEADDR / IP_MULTICAST_LOOP / IP_ADD_MEMBERSHIP до bind'а.
    pub fn bind(config: &MulticastSocketConfig) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;

        let bind_addr: SocketAddr = format!("0.0.0.0:{}", config.port).parse().unwrap();
        socket.bind(&bind_addr.into())?;
        socket.join_multicast_v4(&config.group, &config.local_iface)?;
        // СЛУШАТЬ И СЛАТЬ НАДО ОДНИМ И ТЕМ ЖЕ ИНТЕРФЕЙСОМ.
        //
        // join_multicast_v4 задаёт интерфейс только для ПРИЁМА. Без IP_MULTICAST_IF исходящие
        // датаграммы уходят туда, куда их отправит таблица маршрутов, и на многодомной машине
        // это обычно НЕ тот интерфейс, на котором мы слушаем. Тогда узел исправно объявляет
        // себя — и не слышит никого, а сосед не слышит его.
        //
        // Замер 2026-08-27: два экземпляра приложения на одной машине с
        // OMNI_MESH_MCAST_IFACE=192.168.7.113 не спаривались 90 с. Слушатель, присоединённый
        // ко ВСЕМ интерфейсам сразу, показал, что объявления идут — и идут с 127.0.0.1, тогда
        // как оба узла присоединились к группе на Wi-Fi. Симптом на другом конце:
        // «no reachable address for destination Addr(...) (no verified announce?)».
        //
        // Тот же дефект уже находили в обнаружении omni-transport (docs/bugs/
        // lan-discovery-never-connects.md, «Анонс во все joined-интерфейсы»), но правка тогда не
        // дошла до этого примитива — а на нём стоит меш.
        //
        // UNSPECIFIED здесь остаётся «как было»: обе половины отдаются ОС, и они согласованы
        // между собой по построению.
        socket.set_multicast_if_v4(&config.local_iface)?;
        socket.set_multicast_loop_v4(config.loopback)?;
        socket.set_multicast_ttl_v4(config.ttl)?;
        socket.set_nonblocking(true)?;

        let std_socket: std::net::UdpSocket = socket.into();
        let tokio_socket = UdpSocket::from_std(std_socket)?;
        Ok(Self {
            socket: Arc::new(tokio_socket),
            target: SocketAddrV4::new(config.group, config.port),
        })
    }

    /// Broadcast a frame to the multicast group. Returns the source
    /// address that recv_from would report for our own loopback'нутые
    /// frames (target group:port).
    pub async fn broadcast(&self, frame: &[u8]) -> io::Result<()> {
        let target: SocketAddr = self.target.into();
        let sent = self.socket.send_to(frame, target).await?;
        if sent != frame.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short multicast send: {sent} of {}", frame.len()),
            ));
        }
        Ok(())
    }

    /// Интерфейс, через который УХОДЯТ датаграммы (IP_MULTICAST_IF).
    ///
    /// Существует ради проверки, которую иначе не написать: приём задаётся членством в группе,
    /// отправка — этой опцией, и разъехаться они могут молча. Живьём это стоило нам пейринга
    /// (см. комментарий в `bind`), а поймать разъезд round-trip'ом в одном процессе нельзя:
    /// IP_MULTICAST_LOOP возвращает копию самому отправителю мимо всякой маршрутизации, и такая
    /// проверка зелёная в обоих случаях.
    pub fn send_iface(&self) -> io::Result<Ipv4Addr> {
        socket2::SockRef::from(self.socket.as_ref()).multicast_if_v4()
    }

    /// Receive next frame. Returns (source_addr, bytes). max_len bounds
    /// the receive buffer.
    pub async fn recv(&self, max_len: usize) -> io::Result<(SocketAddr, Vec<u8>)> {
        let mut buf = vec![0u8; max_len];
        let (len, from) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(len);
        Ok((from, buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn loopback_config(port: u16) -> MulticastSocketConfig {
        MulticastSocketConfig {
            group: Ipv4Addr::new(239, 42, 42, 43), // test group, distinct from prod
            port,
            local_iface: Ipv4Addr::UNSPECIFIED,
            loopback: true,
            ttl: 1,
        }
    }

    async fn host_supports_multicast(port: u16) -> bool {
        let Ok(sock) = MulticastSocket::bind(&loopback_config(port)) else {
            return false;
        };
        if sock.broadcast(b"probe").await.is_err() {
            return false;
        }
        matches!(
            timeout(Duration::from_millis(500), sock.recv(1200)).await,
            Ok(Ok(_))
        )
    }

    #[tokio::test]
    async fn bind_succeeds() {
        let _sock = MulticastSocket::bind(&loopback_config(53001))
            .expect("multicast bind должен работать");
    }

    #[tokio::test]
    async fn two_sockets_same_port_via_reuseaddr() {
        let _a = MulticastSocket::bind(&loopback_config(53002)).unwrap();
        let _b = MulticastSocket::bind(&loopback_config(53002))
            .expect("SO_REUSEADDR позволяет второй bind");
    }

    /// Адрес интерфейса, который ОС выберет для выхода наружу. Пакетов не шлёт: `connect` на
    /// UDP только фиксирует маршрут, после чего локальный адрес сокета и есть тот интерфейс.
    fn default_route_ipv4() -> Option<Ipv4Addr> {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        probe.connect("192.0.2.1:9").ok()?; // TEST-NET-1, маршрутизируется, но не существует
        match probe.local_addr().ok()? {
            SocketAddr::V4(addr) if !addr.ip().is_loopback() => Some(*addr.ip()),
            _ => None,
        }
    }

    /// ЯВНО НАЗВАННЫЙ ИНТЕРФЕЙС ОБЯЗАН РАБОТАТЬ В ОБЕ СТОРОНЫ.
    ///
    /// Регрессия 2026-08-27: `join_multicast_v4` задаёт интерфейс только для приёма, а
    /// IP_MULTICAST_IF не выставлялся — исходящие уходили по таблице маршрутов. На многодомной
    /// машине это разные интерфейсы, и два узла на одном хосте не спарились: объявления шли с
    /// петли, а слушали оба на Wi-Fi. Здесь мы называем интерфейс явно и требуем, чтобы кадр
    /// вернулся: до правки этот круг не замыкался.
    #[tokio::test]
    async fn explicit_iface_is_used_for_sending_too() {
        if !host_supports_multicast(53005).await {
            eprintln!("SKIP: multicast loopback недоступен");
            return;
        }
        let Some(iface) = default_route_ipv4() else {
            eprintln!("SKIP: на хосте нет не-петлевого IPv4");
            return;
        };
        // ПРОВЕРЯЕМ ОПЦИЮ, А НЕ КРУГ ПО ПЕТЛЕ — ИНАЧЕ ПРОВЕРКА НЕ УМЕЕТ ПАДАТЬ.
        //
        // Две версии этого теста подряд были зелёными ДАЖЕ С ОТКЛЮЧЁННОЙ ПРАВКОЙ: и один сокет,
        // и два сокета в одном процессе получают копию через IP_MULTICAST_LOOP независимо от
        // того, каким интерфейсом кадр ушёл. Разъезд виден только между ПРОЦЕССАМИ, а поднимать
        // второй процесс из модульного теста — не его работа.
        //
        // Замер, который это установил (два процесса PowerShell, группа 239.42.42.42:5354,
        // членство на 192.168.7.113): без IP_MULTICAST_IF сосед получил 0 из 3 кадров, и они
        // уходили с 127.0.0.1; с IP_MULTICAST_IF — 3 из 3 с 192.168.7.113.
        //
        // Поэтому здесь сравнивается ровно то, что разъезжается: названный интерфейс и тот,
        // который реально стоит на сокете для отправки.
        let config = MulticastSocketConfig {
            local_iface: iface,
            ..loopback_config(53006)
        };
        let sock = MulticastSocket::bind(&config).expect("bind на явном интерфейсе");
        assert_eq!(
            sock.send_iface().expect("IP_MULTICAST_IF читается"),
            iface,
            "исходящие обязаны уходить тем же интерфейсом, на котором мы слушаем"
        );
    }

    #[tokio::test]
    async fn broadcast_recv_round_trip() {
        if !host_supports_multicast(53003).await {
            eprintln!("SKIP: multicast loopback недоступен");
            return;
        }
        let sock = MulticastSocket::bind(&loopback_config(53004)).unwrap();
        sock.broadcast(b"hello mesh").await.unwrap();
        let (_from, frame) = timeout(Duration::from_secs(2), sock.recv(1200))
            .await
            .expect("recv в течение 2с")
            .expect("recv ok");
        assert_eq!(frame, b"hello mesh");
    }
}
