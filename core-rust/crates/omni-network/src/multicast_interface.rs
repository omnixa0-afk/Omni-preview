// LAN multicast Interface implementation. См. networking-overlay.md
// §«Interface (link-layer) abstraction».
//
// Цель: peer discovery в LAN без infrastructure. Два узла Omni на одной
// сети должны увидеть друг друга через несколько секунд после старта.
//
// Подход: admin-scoped UDP multicast group + broadcast announce'ов.
// Не используем full mDNS чтобы:
// (а) не конфликтовать с системным mDNS (Bonjour/avahi)
// (б) не парсить SRV/PTR records — наш announce уже структура
//
// Группа: 239.42.42.42 (admin-scoped 239.0.0.0/8), port 5354.
// «239.42.42.42» легко запомнить, «5354» — не конфликтует с известными
// IANA assignments.
//
// Loopback: по умолчанию OS не доставляет multicast обратно отправителю.
// Для in-process тестов мы это переключаем через IP_MULTICAST_LOOP=1.

use crate::interface::{Interface, InterfaceId, InterfaceKind, NeighborId};
use async_trait::async_trait;
use omni_net_primitives::{MulticastSocket, MulticastSocketConfig};
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

/// Omni-specific multicast group (admin-scoped, 239.0.0.0/8).
pub const OMNI_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 42, 42, 42);

/// Omni-specific multicast port.
pub const OMNI_MULTICAST_PORT: u16 = 5354;

/// Maximum bytes per multicast frame. UDP fragmentation работает но
/// плохо для multicast — держимся в одном datagram'е.
pub const MULTICAST_MTU: usize = 1200;

/// LAN multicast interface для discovery + broadcast announce'ов.
///
/// Тонкий wrapper над [`MulticastSocket`] (omni-net-primitives): добавляет
/// Interface trait semantics (kind/mtu/MTU-guard) поверх identity-agnostic
/// socket primitive. Сам socket-setup (SO_REUSEADDR / join / loopback / TTL)
/// живёт в primitive'е и шарится с omni-transport discovery.
pub struct MulticastInterface {
    id: InterfaceId,
    socket: Arc<MulticastSocket>,
}

impl std::fmt::Debug for MulticastInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MulticastInterface")
            .field("id", &self.id)
            .field("socket", &self.socket)
            .finish()
    }
}

/// Конфиг для MulticastInterface. Production использует defaults;
/// в тестах enable loopback чтобы два instance'а в одном процессе
/// видели друг друга.
#[derive(Clone, Debug)]
pub struct MulticastConfig {
    /// Multicast group address.
    pub group: Ipv4Addr,
    /// Port.
    pub port: u16,
    /// Какой local interface использовать для multicast (по IP'у).
    /// `Ipv4Addr::UNSPECIFIED` = на выбор ОС: маршрут 224.0.0.0/4 с наименьшей метрикой, не
    /// обязательно LAN (VPN-адаптер может отвергнуть join с 10065).
    pub local_iface: Ipv4Addr,
    /// IP_MULTICAST_LOOP — true для in-process тестов, false для production
    /// (зачем своему же hosts'у возвращать свой же broadcast).
    pub loopback: bool,
    /// IP_MULTICAST_TTL — 1 = только local LAN segment (no router crossing).
    pub ttl: u32,
}

impl Default for MulticastConfig {
    fn default() -> Self {
        Self {
            group: OMNI_MULTICAST_GROUP,
            port: OMNI_MULTICAST_PORT,
            local_iface: Ipv4Addr::UNSPECIFIED,
            loopback: false,
            ttl: 1,
        }
    }
}

impl MulticastInterface {
    pub fn bind(config: MulticastConfig) -> io::Result<Arc<Self>> {
        // Socket-setup (SO_REUSEADDR / join / loopback / TTL) делегируем
        // primitive'у — он шарится с omni-transport discovery.
        let id = InterfaceId(format!("multicast-{}:{}", config.group, config.port));
        let socket = MulticastSocket::bind(&MulticastSocketConfig {
            group: config.group,
            port: config.port,
            local_iface: config.local_iface,
            loopback: config.loopback,
            ttl: config.ttl,
        })?;

        Ok(Arc::new(Self {
            id,
            socket: Arc::new(socket),
        }))
    }

    /// Удобный конструктор с default config'ом.
    pub fn bind_default() -> io::Result<Arc<Self>> {
        Self::bind(MulticastConfig::default())
    }
}

#[async_trait]
impl Interface for MulticastInterface {
    fn id(&self) -> InterfaceId {
        self.id.clone()
    }

    fn kind(&self) -> InterfaceKind {
        InterfaceKind::Mdns
    }

    fn mtu(&self) -> usize {
        MULTICAST_MTU
    }

    fn bandwidth_estimate(&self) -> u64 {
        // LAN multicast — обычно сотни Mbps в современных сетях,
        // но мы используем его только для discovery (rare announce'ы),
        // не для bulk data. Conservative estimate.
        10_000_000
    }

    fn latency_estimate(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn reliable(&self) -> bool {
        // UDP multicast — best-effort, нет retry.
        false
    }

    fn broadcast_capable(&self) -> bool {
        true
    }

    async fn send_frame(&self, neighbor: &NeighborId, frame: &[u8]) -> io::Result<()> {
        // Multicast — не unicast. Каждый "send to specific neighbor" сводится
        // к broadcast'у. Caller обычно использует broadcast_frame напрямую.
        // Если уж зовут send_frame — тоже broadcast'им (neighbor — лишь
        // signaling для логирования).
        let _ = neighbor;
        self.broadcast_frame(frame).await
    }

    async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
        let (from, frame) = self.socket.recv(MULTICAST_MTU).await?;
        // Neighbor = source IP:port от которого пришёл multicast frame.
        // Это даёт нам next-hop info для path table'а.
        Ok((NeighborId(from.to_string()), frame))
    }

    async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()> {
        if frame.len() > MULTICAST_MTU {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame {} > MTU {}", frame.len(), MULTICAST_MTU),
            ));
        }
        self.socket.broadcast(frame).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::time::timeout;

    /// Интерфейс маршрута по умолчанию — настоящий адаптер: на Windows IP_MULTICAST_LOOP=1
    /// работает только через него, не через 127.0.0.1. НЕ UNSPECIFIED: членство «на любом»
    /// Windows вешает на маршрут 224.0.0.0/4 с наименьшей метрикой, а на хосте с VPN-адаптером
    /// (Radmin, 2026-10-16) тот отвечает на join WSAEHOSTUNREACH (10065) — три теста падали, а
    /// тесты доставки молча пропускались. Боевой путь выбирает интерфейс явно
    /// (`detect_primary_lan_ipv4`). Без сети — 127.0.0.1 (тесты доставки тогда пропустятся).
    fn test_iface() -> Ipv4Addr {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|probe| {
            probe.connect("192.0.2.1:9")?; // TEST-NET-1: только выбор маршрута, пакетов нет
            probe.local_addr()
        });
        match probe {
            Ok(SocketAddr::V4(local)) if !local.ip().is_unspecified() => *local.ip(),
            _ => Ipv4Addr::LOCALHOST,
        }
    }

    fn loopback_config(port: u16) -> MulticastConfig {
        MulticastConfig {
            group: OMNI_MULTICAST_GROUP,
            port,
            local_iface: test_iface(),
            loopback: true,
            ttl: 1,
        }
    }

    /// Helper: skip test gracefully если хост не имеет работающего
    /// non-loopback network interface'а (sandbox/CI без сети).
    /// Возвращает Some(()) если можно продолжать, None для skip.
    /// Должен вызываться из async теста (мы уже в tokio runtime).
    async fn require_multicast_capable_host(probe_port: u16) -> Option<()> {
        // Probe: bind ad-hoc multicast и сразу broadcast/recv. Если
        // в течение 500мс получили свой пакет — environment ok.
        let Ok(iface) = MulticastInterface::bind(loopback_config(probe_port)) else {
            eprintln!("SKIP: multicast bind failed — нет network adapter'а?");
            return None;
        };

        iface.broadcast_frame(b"probe").await.ok()?;
        match timeout(Duration::from_millis(500), iface.recv_frame()).await {
            Ok(Ok(_)) => Some(()),
            _ => {
                eprintln!("SKIP: multicast loopback не работает на этом хосте \
                    (нет non-loopback interface'а или firewall блокирует)");
                None
            }
        }
    }

    /// Pick unused port range to avoid collisions с другими тестами.
    /// Multicast tests должны bind'ить unique port per test чтобы
    /// instance'ы разных тестов не пересекались.
    fn pick_port(seed: u16) -> u16 {
        // Из ephemeral диапазона, выше реалистичных well-known.
        50000 + seed
    }

    #[tokio::test]
    async fn bind_succeeds() {
        // H4 (basic): multicast bind работает на loopback'е.
        let iface = MulticastInterface::bind(loopback_config(pick_port(1)))
            .expect("multicast bind должен работать");
        assert_eq!(iface.kind(), InterfaceKind::Mdns);
        assert!(iface.broadcast_capable());
        assert!(!iface.reliable());
    }

    #[tokio::test]
    async fn two_instances_can_bind_same_port_with_reuseaddr() {
        // SO_REUSEADDR делает возможным два multicast listener'а на одном
        // port'у. Это базовое требование для тестов одного процесса.
        let port = pick_port(2);
        let _iface1 = MulticastInterface::bind(loopback_config(port))
            .expect("first bind ok");
        let _iface2 = MulticastInterface::bind(loopback_config(port))
            .expect("second bind на тот же port должен работать благодаря SO_REUSEADDR");
    }

    #[tokio::test]
    async fn broadcast_loops_back_to_same_instance() {
        if require_multicast_capable_host(49999).await.is_none() {
            return;
        }
        // С loopback=true, наш собственный broadcast должен прийти к нам же.
        // Это полезно для health-check'а multicast подсистемы.
        let port = pick_port(3);
        let iface = MulticastInterface::bind(loopback_config(port)).unwrap();

        iface.broadcast_frame(b"hello from self").await.unwrap();

        let (_from, frame) = timeout(Duration::from_secs(2), iface.recv_frame())
            .await
            .expect("должны получить наш broadcast в течение 2с")
            .expect("recv не должен fail'нуть");
        assert_eq!(frame, b"hello from self");
    }

    #[tokio::test]
    async fn two_instances_see_each_others_broadcast() {
        if require_multicast_capable_host(49999).await.is_none() {
            return;
        }
        // H4 (the main thing): два MulticastInterface'а в одном процессе
        // видят broadcast'ы друг друга. Это и есть LAN discovery в
        // miniature.
        let port = pick_port(4);
        let iface_a = MulticastInterface::bind(loopback_config(port)).unwrap();
        let iface_b = MulticastInterface::bind(loopback_config(port)).unwrap();

        // A broadcast'ит, B принимает.
        iface_a.broadcast_frame(b"hello from A").await.unwrap();
        let recv_task = tokio::spawn({
            let iface_b = Arc::clone(&iface_b);
            async move {
                // Возможно получим свой собственный (если loopback),
                // фильтруем по содержанию.
                for _ in 0..3 {
                    let (_, frame) = timeout(Duration::from_secs(2), iface_b.recv_frame())
                        .await
                        .expect("timeout ждали broadcast от A")
                        .expect("recv");
                    if frame == b"hello from A" {
                        return frame;
                    }
                }
                panic!("за 3 попытки не получили broadcast от A");
            }
        });

        let frame = recv_task.await.unwrap();
        assert_eq!(frame, b"hello from A");
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let iface = MulticastInterface::bind(loopback_config(pick_port(5))).unwrap();
        let too_big = vec![0u8; MULTICAST_MTU + 1];
        let err = iface
            .broadcast_frame(&too_big)
            .await
            .expect_err("должен отвергнуть frame больше MTU");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn recv_returns_source_address_as_neighbor() {
        if require_multicast_capable_host(49999).await.is_none() {
            return;
        }
        // Neighbor ID = source IP:port from which the multicast frame came.
        // Это даёт path table'у info "via какого next-hop'а пришёл этот announce".
        let port = pick_port(6);
        let iface = MulticastInterface::bind(loopback_config(port)).unwrap();
        iface.broadcast_frame(b"check source").await.unwrap();

        let (neighbor, _frame) = timeout(Duration::from_secs(2), iface.recv_frame())
            .await
            .unwrap()
            .unwrap();
        // Должен быть IP:port — exact format зависит от network interface
        // (loopback при no-net, иначе LAN IP). Просто валидируем что parsable.
        let _parsed: SocketAddr = neighbor.0.parse().expect("neighbor.0 должен быть SocketAddr");
    }
}
