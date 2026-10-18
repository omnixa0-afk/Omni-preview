// Persisted last-good DIRECT endpoints per mesh NodeAddress — the warm-rejoin cache (G3 / ADR D7).
//
// On start the app re-dials these immediately (runtime.dial_at) instead of re-discovering addresses
// cold via LAN announce / UPnP map / rendezvous beacon. Endpoints are HINTS, never trust: the mutual
// ed25519 challenge on every dial is the only authority, so a stale/poisoned entry can at worst waste
// one dial. Refreshed by a live snapshot of the verified DIRECT peers (Network::snapshot_peer_book);
// TTL'd so a moved endpoint (CGNAT remap) ages out. Not profile-scoped — the db file is the scope,
// matching the sibling mesh_dialog_route. See docs/decisions/omni-network-model.md.

/// How long a cached endpoint stays dialable before it must be re-observed (48 h). A direct LAN /
/// public endpoint is fairly stable; a stale one just wastes a challenge-gated dial, so this errs
/// generous. Reflexive/relay endpoints are NOT stored here (the snapshot keeps only direct paths).
pub const MESH_PEER_BOOK_TTL_MS: i64 = 48 * 60 * 60 * 1000;

/// Cap on cached entries loaded at start, newest-first — bounds warm-dial fan-out + table growth on a
/// very social profile (the snapshot stores ~1 direct endpoint per peer, so this is generous).
/// Сколько записей отдаёт загрузка книги — И ЭТО ПРЕДЕЛ ВРЕМЕНИ, А НЕ ПАМЯТИ.
///
/// Потребитель один: тёплое переподключение при старте меша. Оно идёт по этим записям
/// ПОСЛЕДОВАТЕЛЬНО и БЛОКИРУЮЩЕ, и каждый мёртвый адрес стоит полного таймаута дозвона — замер
/// 2026-08-30 дал 30 секунд на такую попытку. При прежнем пределе в 512 это до четырёх часов
/// работы потока, в которых пир со свежим адресом ждёт в очереди за сотнями мёртвых.
///
/// Мёртвых там много по построению: срок годности записи — двое суток, а порт за NAT меняется при
/// каждом перезапуске пира. За день перезапусков накапливается десяток адресов на пира, живой из
/// которых один.
///
/// Тёплое переподключение — УСКОРИТЕЛЬ, а не механизм: всё, до чего оно не дотянулось, поднимут
/// объявления в локальной сети, рандеву и обычный автодозвон. Поэтому предел выбран по тому,
/// сколько мы готовы потратить ВРЕМЕНИ, а не по тому, сколько влезет в память.
const MESH_PEER_BOOK_LOAD_CAP: i64 = 32;

/// Сколько адресов берём У ОДНОГО пира. Живой из них ровно один — самый свежий; остальные это
/// прошлые отображения NAT. Без этого предела один часто перезапускающийся пир вытеснял бы из
/// общей выборки всех остальных, и тёплое переподключение занималось бы только им.
const MESH_PEER_BOOK_PER_PEER_LOAD_CAP: i64 = 3;

impl Storage {
    /// Record (or refresh) one peer's last-good DIRECT endpoint — stamped `observed_at = now`,
    /// `expires_at = now + TTL`. Upsert (not wholesale replace) on purpose: a peer that is offline
    /// RIGHT NOW must keep its cached endpoint (the whole point of warm rejoin is to remember peers
    /// across disconnects/restarts), so only the currently-live peers are refreshed and the rest age
    /// out by TTL. Idempotent per `(node_address, dial_addr)`.
    pub fn upsert_mesh_peer_endpoint(
        &self,
        node_address: &[u8],
        dial_addr: &str,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let expires_at = now_unix_ms.saturating_add(MESH_PEER_BOOK_TTL_MS);
        self.connection
            .execute(
                "
                INSERT INTO mesh_peer_book \
                    (node_address, dial_addr, observed_at_unix_ms, expires_at_unix_ms) \
                VALUES (?1, ?2, ?3, ?4) \
                ON CONFLICT(node_address, dial_addr) DO UPDATE SET \
                    observed_at_unix_ms = excluded.observed_at_unix_ms, \
                    expires_at_unix_ms = excluded.expires_at_unix_ms
                ",
                params![node_address, dial_addr, now_unix_ms, expires_at],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Непросроченные `(node_address, dial_addr)`, свежайшие первыми, ограниченные ДВАЖДЫ: не
    /// больше [`MESH_PEER_BOOK_PER_PEER_LOAD_CAP`] на пира и не больше
    /// [`MESH_PEER_BOOK_LOAD_CAP`] всего. Оба предела — про ВРЕМЯ старта, см. их описания.
    /// Питает быстрый дозвон тёплого переподключения при старте меша.
    pub fn load_mesh_peer_book(&self, now_unix_ms: i64) -> OmniResult<Vec<(Vec<u8>, String)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT node_address, dial_addr FROM ( \
                     SELECT node_address, dial_addr, observed_at_unix_ms, \
                            ROW_NUMBER() OVER ( \
                                PARTITION BY node_address ORDER BY observed_at_unix_ms DESC \
                            ) AS freshness_rank \
                     FROM mesh_peer_book \
                     WHERE expires_at_unix_ms > ?1 \
                 ) WHERE freshness_rank <= ?2 \
                 ORDER BY observed_at_unix_ms DESC LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    now_unix_ms,
                    MESH_PEER_BOOK_PER_PEER_LOAD_CAP,
                    MESH_PEER_BOOK_LOAD_CAP
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Drop the whole peer book (sign-out / profile-delete cleanup).
    pub fn clear_mesh_peer_book(&self) -> OmniResult<()> {
        self.connection
            .execute("DELETE FROM mesh_peer_book", [])
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}
