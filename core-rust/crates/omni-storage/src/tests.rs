#[cfg(test)]
mod tests {
    /// Media key material of a test attachment offer (every offer is end-to-end encrypted).
    const TEST_MEDIA_KEY_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TEST_MEDIA_NONCE_SEED_HEX: &str = "22222222222222222222222222222222";

    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::Storage;
    use super::{
        account_device_scoped_tables, device_scoped_tables,
        global_install_scoped_tables, ownership_category_for_table, profile_delete_tables,
        profile_owned_tables, StorageOwnershipCategory,
    };
    use omni_crypto::{
        bootstrap_initiator_session, build_prekey_bundle, establish_session,
        generate_local_key_hierarchy, SessionAddress, CRYPTO_SUITE_VERSION_CURRENT,
    };
    use omni_identity::{
        AccountId, AddContactInput, ContactId, ContactRelationshipState, ContactRequestDirection,
        DangerousActionKind, DatabaseBootstrapStateKind, DeviceId, LocalProfileState,
        RecoveryConflictCode, RecoveryFlowKind, RecoveryMnemonicUsage,
        ACCOUNT_REPRESENTATION_VERSION_V2,
    };
    use omni_messaging::{MessageEncryptionState, SecurityPayloadKind};
    use omni_types::{
        dialog_key_from_instance_id, ChatDirection, MessageStatus,
    };

    static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn open_migrate_and_healthcheck_create_database() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .healthcheck()
            .expect("healthcheck should succeed after migration");

        assert!(db_path.exists());
    }

    /// The "friend" connectivity tier is gone: a database that still has `contacts.is_friend` loses
    /// the column on migrate, and a fresh one never gets it (docs/decisions/no-friend-tier.md).
    #[test]
    fn migrate_drops_the_legacy_is_friend_column() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-tests")
            .join(format!("no-friend-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let has_column = |storage: &Storage| -> bool {
            let mut statement = storage.connection.prepare("PRAGMA table_info(contacts)").unwrap();
            let names: Vec<String> = statement.query_map([], |row| row.get(1)).unwrap().map(|r| r.unwrap()).collect();
            names.iter().any(|n| n == "is_friend")
        };
        assert!(!has_column(&storage), "a fresh database never gets the column");

        // An older database: the column is there (negative half — migrate has something to do).
        storage
            .connection
            .execute_batch("ALTER TABLE contacts ADD COLUMN is_friend INTEGER NOT NULL DEFAULT 0;")
            .unwrap();
        assert!(has_column(&storage));
        storage.migrate().expect("migrations should succeed on the older schema");
        assert!(!has_column(&storage), "migrate drops it");
        storage.healthcheck().expect("healthcheck after the drop");
    }

    #[test]
    fn migrate_drops_the_legacy_direct_route_hints_table() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-tests")
            .join(format!("no-route-hints-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let has_table = |storage: &Storage| -> bool {
            storage
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'direct_route_hints'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                > 0
        };
        assert!(!has_table(&storage), "a fresh database never gets the table");

        // An older database still has it (negative half — migrate has something to do).
        storage
            .connection
            .execute_batch("CREATE TABLE direct_route_hints (hint_id TEXT PRIMARY KEY NOT NULL);")
            .unwrap();
        assert!(has_table(&storage));
        storage.migrate().expect("migrations should succeed on the older schema");
        assert!(!has_table(&storage), "migrate drops it");
        storage.healthcheck().expect("healthcheck after the drop");
    }

    #[test]
    fn durable_frames_replace_load_and_survive_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-durable-frames-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        // Empty initially.
        assert!(storage.load_durable_frames().unwrap().is_empty());

        // Persist two opaque wire-blob frames.
        let frames = vec![
            (vec![1u8; 16], 1i64, vec![10u8, 20, 30]),
            (vec![2u8; 16], 5i64, vec![40u8, 50]),
        ];
        assert_eq!(storage.replace_durable_frames(&frames).unwrap(), 2);
        let loaded = storage.load_durable_frames().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&vec![10u8, 20, 30]));
        assert!(loaded.contains(&vec![40u8, 50]));

        // Replace clears the previous set.
        assert_eq!(
            storage
                .replace_durable_frames(&[(vec![3u8; 16], 9, vec![99u8])])
                .unwrap(),
            1
        );
        assert_eq!(storage.load_durable_frames().unwrap(), vec![vec![99u8]]);

        // Survives a reopen (the point of persistence).
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        assert_eq!(reopened.load_durable_frames().unwrap(), vec![vec![99u8]]);
    }

    #[test]
    fn mesh_dialog_route_persists_and_overwrites() {
        let db_path = std::env::temp_dir()
            .join("omni-mesh-route-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        let node_a = vec![0xAAu8; 20];
        let node_b = vec![0xBBu8; 20];
        let dialog_of = |s: &Storage, node: &[u8]| -> Option<String> {
            s.load_mesh_dialog_routes()
                .unwrap()
                .into_iter()
                .find(|(stored, _)| stored.as_slice() == node)
                .map(|(_, dialog)| dialog)
        };
        assert_eq!(dialog_of(&storage, &node_a), None);

        storage.set_mesh_dialog_route(&node_a, "dialog-a").unwrap();
        storage.set_mesh_dialog_route(&node_b, "dialog-b").unwrap();
        assert_eq!(dialog_of(&storage, &node_a), Some("dialog-a".to_string()));

        // A re-pair / instance change overwrites the binding for the same node.
        storage.set_mesh_dialog_route(&node_a, "dialog-a2").unwrap();
        assert_eq!(dialog_of(&storage, &node_a), Some("dialog-a2".to_string()));

        // Survives reopen — the binding must outlive a restart so it can repopulate
        // the in-memory dialog map for never-online-together durable delivery.
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        let mut routes = reopened.load_mesh_dialog_routes().unwrap();
        routes.sort();
        assert_eq!(
            routes,
            vec![
                (node_a, "dialog-a2".to_string()),
                (node_b, "dialog-b".to_string()),
            ]
        );
    }

    #[test]
    fn mesh_peer_book_upserts_loads_expires_and_clears() {
        let db_path = std::env::temp_dir()
            .join("omni-mesh-peer-book-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        let node_a = vec![0xAAu8; 20];
        let node_b = vec![0xBBu8; 20];
        let now = 1_000_000i64;

        assert!(storage.load_mesh_peer_book(now).unwrap().is_empty(), "empty before any upsert");

        // Two live direct peers.
        storage.upsert_mesh_peer_endpoint(&node_a, "203.0.113.7:5000", now).unwrap();
        storage.upsert_mesh_peer_endpoint(&node_b, "198.51.100.9:7000", now).unwrap();
        let mut loaded = storage.load_mesh_peer_book(now).unwrap();
        loaded.sort();
        assert_eq!(
            loaded,
            vec![
                (node_a.clone(), "203.0.113.7:5000".to_string()),
                (node_b.clone(), "198.51.100.9:7000".to_string()),
            ]
        );

        // Refreshing the SAME endpoint is idempotent (no duplicate row) — just bumps observed/expires.
        storage.upsert_mesh_peer_endpoint(&node_a, "203.0.113.7:5000", now + 1_000).unwrap();
        assert_eq!(storage.load_mesh_peer_book(now).unwrap().len(), 2, "no duplicate on refresh");

        // Upserting only A does NOT remove B — an offline peer keeps its cached endpoint (the point
        // of warm rejoin is to remember peers across disconnects, not just the currently-live ones).
        let mut after = storage.load_mesh_peer_book(now).unwrap();
        after.sort();
        assert!(after.iter().any(|(n, _)| n == &node_b), "offline peer B is not wiped");

        // Expired entries are filtered at load (a query past observed_at + TTL).
        let past_ttl = now + 1_000 + crate::MESH_PEER_BOOK_TTL_MS + 1;
        assert!(
            storage.load_mesh_peer_book(past_ttl).unwrap().is_empty(),
            "stale cached endpoints age out"
        );

        // Survives reopen — the cache must outlive a restart (that's the whole point: warm rejoin).
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        assert_eq!(reopened.load_mesh_peer_book(now).unwrap().len(), 2, "cache outlives restart");

        // Clear empties it (sign-out / delete cleanup).
        reopened.clear_mesh_peer_book().unwrap();
        assert!(reopened.load_mesh_peer_book(now).unwrap().is_empty());
    }

    #[test]
    fn peer_book_load_is_bounded_by_time_not_by_memory() {
        // Предел загрузки книги — это ВРЕМЯ СТАРТА, а не размер памяти. Потребитель один: тёплое
        // переподключение, и оно дозванивается по этим записям последовательно и блокирующе, по
        // полному таймауту на каждый мёртвый адрес (замер 2026-08-30: 30 с на попытку).
        //
        // Мёртвых там много по построению: срок годности двое суток, а порт за NAT меняется при
        // каждом перезапуске пира. Один часто перезапускающийся пир не должен вытеснять всех
        // остальных из выборки — иначе тёплое переподключение занимается только им.
        let db_path = std::env::temp_dir()
            .join("omni-peer-book-caps")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let now = 1_700_000_000_000_i64;

        // Пир, перезапускавшийся пять раз: пять адресов, живой — самый свежий.
        let restarted = vec![9u8; 20];
        for i in 0..5i64 {
            storage
                .upsert_mesh_peer_endpoint(&restarted, &format!("203.0.113.7:{}", 5000 + i), now + i)
                .unwrap();
        }
        // Спокойный пир с одним адресом.
        let steady = vec![1u8; 20];
        storage.upsert_mesh_peer_endpoint(&steady, "198.51.100.9:7000", now).unwrap();

        let loaded = storage.load_mesh_peer_book(now + 10).unwrap();
        let from_restarted: Vec<&String> = loaded
            .iter()
            .filter(|(node, _)| node == &restarted)
            .map(|(_, addr)| addr)
            .collect();
        assert_eq!(
            from_restarted.len(),
            3,
            "у одного пира берём только несколько свежайших адресов, а не всю его историю NAT"
        );
        assert_eq!(
            from_restarted,
            vec!["203.0.113.7:5004", "203.0.113.7:5003", "203.0.113.7:5002"],
            "и это именно СВЕЖАЙШИЕ: живой адрес у перезапустившегося пира — последний"
        );
        assert!(
            loaded.iter().any(|(node, _)| node == &steady),
            "шумный пир не вытесняет спокойного — ради этого предел и на пира тоже"
        );

        // Общий предел: сорок пиров по одному адресу не превращают старт в марафон.
        for i in 0..40u8 {
            storage
                .upsert_mesh_peer_endpoint(&[100 + i; 20], "192.0.2.1:6000", now + 100)
                .unwrap();
        }
        assert!(
            storage.load_mesh_peer_book(now + 200).unwrap().len() <= 32,
            "общая выборка ограничена тем, сколько дозвонов мы готовы оплатить временем"
        );
    }

    #[test]
    fn network_nodes_add_list_remove_survive_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-network-nodes-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        // migrate() seeds the base node; this test exercises raw CRUD, so start from a clean slate (the
        // seed behaviour itself is covered by the omni-app network_nodes test).
        storage.clear_network_nodes().unwrap();

        assert!(storage.list_network_nodes().unwrap().is_empty(), "empty after clear");

        // One seeded base node (with a TCP-carrier endpoint + SPKI pin) + two manual relays (one by
        // endpoint, one by node address).
        let spki_pin = vec![0xABu8; 32];
        let base_id = storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("198.51.100.213:47100"),
                label: "Базовый узел",
                source: crate::NETWORK_NODE_SOURCE_BASE,
                added_at_unix_ms: 1_000,
                tcp_endpoint: Some("198.51.100.213:8443"),
                cert_spki_pin: Some(&spki_pin),
                ..Default::default()
            })
            .unwrap();
        storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("203.0.113.7:5000"),
                label: "Друг-релей",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 2_000,
                ..Default::default()
            })
            .unwrap();
        let node_addr = vec![0xCDu8; 20];
        storage
            .add_network_node(&crate::NewNetworkNode {
                node_address: Some(&node_addr),
                label: "По адресу узла",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 3_000,
                ..Default::default()
            })
            .unwrap();

        // Base sorts first; endpoint/node_address/tcp_endpoint/SPKI-pin round-trip; dup-endpoint works.
        let listed = storage.list_network_nodes().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].source, crate::NETWORK_NODE_SOURCE_BASE);
        assert_eq!(listed[0].endpoint.as_deref(), Some("198.51.100.213:47100"));
        assert_eq!(listed[0].tcp_endpoint.as_deref(), Some("198.51.100.213:8443"));
        assert_eq!(listed[0].cert_spki_pin.as_deref(), Some(spki_pin.as_slice()));
        assert_eq!(listed[1].tcp_endpoint, None, "a UDP-only relay has no tcp_endpoint");
        assert_eq!(listed[2].node_address.as_deref(), Some(node_addr.as_slice()));
        assert!(storage.network_node_endpoint_exists("203.0.113.7:5000").unwrap());
        assert!(!storage.network_node_endpoint_exists("10.0.0.1:1").unwrap());

        // The base node is removable (the empty list must still connect) — remove it.
        assert_eq!(storage.remove_network_node(base_id).unwrap(), 1);
        assert_eq!(storage.remove_network_node(base_id).unwrap(), 0, "removing a gone id is a no-op");

        // Survives reopen — the registry is a durable user setting, base stays removed.
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        let after = reopened.list_network_nodes().unwrap();
        assert_eq!(after.len(), 2, "registry outlives restart, base stays removed");
        assert!(after.iter().all(|n| n.source == crate::NETWORK_NODE_SOURCE_MANUAL));

        reopened.clear_network_nodes().unwrap();
        assert!(reopened.list_network_nodes().unwrap().is_empty());
    }

    /// A passport kept for another account: the newest version replaces an older one and an older one
    /// arriving late changes nothing; everything kept for others together stays within the quota; a
    /// holder's late, older confirmation does not move the owner's record back.
    #[test]
    fn a_held_passport_keeps_the_newest_version_within_the_quota() {
        let (laptop, _desktop, _laptop_id, _desktop_id) = laptop_and_desktop_that_know_each_other("held-passports");
        let record = |owner: &str, version: i64, size: usize| crate::HeldPassportRecord {
            owner_account_id: AccountId(owner.into()),
            locator_hex: "ab".repeat(32),
            version,
            sealed: vec![version as u8; size],
            mark_secret_hex: "cd".repeat(32),
            stored_at_unix_ms: 1_000 + version,
        };
        assert_eq!(laptop.store_held_passport(&record("acc-a", 5, 1000)).unwrap(), crate::HeldPassportOutcome::Stored);
        assert_eq!(laptop.store_held_passport(&record("acc-a", 4, 1000)).unwrap(), crate::HeldPassportOutcome::AlreadyNewer);
        assert_eq!(laptop.store_held_passport(&record("acc-a", 5, 1000)).unwrap(), crate::HeldPassportOutcome::AlreadyNewer);
        assert_eq!(laptop.store_held_passport(&record("acc-a", 7, 2000)).unwrap(), crate::HeldPassportOutcome::Stored);
        assert_eq!(laptop.get_held_passport(&AccountId("acc-a".into())).unwrap().unwrap().version, 7);
        // A second account fills the rest; a third one no longer fits, the first may still grow.
        let rest = crate::HELD_PASSPORTS_MAX_TOTAL_BYTES - 2000;
        assert_eq!(laptop.store_held_passport(&record("acc-b", 1, rest)).unwrap(), crate::HeldPassportOutcome::Stored);
        assert_eq!(laptop.store_held_passport(&record("acc-c", 1, 10)).unwrap(), crate::HeldPassportOutcome::OverQuota);
        assert_eq!(laptop.store_held_passport(&record("acc-a", 8, 2000)).unwrap(), crate::HeldPassportOutcome::Stored);
        laptop.drop_held_passport(&AccountId("acc-b".into())).unwrap();
        assert_eq!(laptop.store_held_passport(&record("acc-c", 1, 10)).unwrap(), crate::HeldPassportOutcome::Stored);
        assert_eq!(laptop.list_held_passports().unwrap().len(), 2);

        let bob = AccountId("acc-bob".into());
        laptop.choose_passport_holder(&bob, 100).unwrap();
        laptop.choose_passport_holder(&bob, 200).unwrap();
        assert_eq!(laptop.list_passport_holders().unwrap()[0].chosen_at_unix_ms, 100, "chosen once");
        laptop.record_passport_sent(&bob, 9, 300).unwrap();
        assert!(laptop.record_passport_confirmed(&bob, 9, 310).unwrap());
        assert!(!laptop.record_passport_confirmed(&bob, 8, 320).unwrap(), "an older confirmation is late");
        let holder = laptop.list_passport_holders().unwrap().remove(0);
        assert_eq!((holder.sent_version, holder.confirmed_version, holder.confirmed_at_unix_ms), (9, 9, 310));
    }

    /// A profile still on the old default set (ntfy + offchain.pub + Mainline) gets the rest of the
    /// relay pool once; a profile whose set the user changed is left as the user left it.
    /// A device's courier set is kept per (account, device), replaced by the next one it tells us, and
    /// the URLs of all of them come out once each; a removed contact takes its sets along.
    #[test]
    fn a_peer_courier_set_is_kept_per_device_and_goes_with_its_contact() {
        let (laptop, _desktop, laptop_id, _desktop_id) = laptop_and_desktop_that_know_each_other("courier-sets");
        let _ = laptop_id;
        let bob = laptop.add_contact(&contact_input("acc-bob", "Bob"), 3_000).unwrap();
        let account = AccountId("acc-bob".into());
        let (phone, pc) = (DeviceId("bob-phone".into()), DeviceId("bob-pc".into()));
        let phone_set = crate::PeerCourierSet {
            publish: vec!["wss://nos.lol".into()],
            read: vec!["wss://nos.lol".into(), "https://ntfy.sh".into()],
        };
        assert!(laptop.record_peer_courier_set(&account, &phone, &phone_set, 3_100).unwrap());
        assert!(!laptop.record_peer_courier_set(&account, &phone, &phone_set, 3_200).unwrap(), "the same set: no change");
        let pc_set = crate::PeerCourierSet { publish: vec!["https://bob.example".into()], read: vec!["https://bob.example".into()] };
        laptop.record_peer_courier_set(&account, &pc, &pc_set, 3_300).unwrap();
        assert_eq!(laptop.get_peer_courier_set(&account, &phone).unwrap(), Some(phone_set));
        assert_eq!(
            laptop.list_peer_courier_urls().unwrap(),
            ["https://bob.example", "https://ntfy.sh", "wss://nos.lol"]
        );
        laptop.forget_contact(&bob.contact_id, 4_000).unwrap();
        assert_eq!(laptop.get_peer_courier_set(&account, &pc).unwrap(), None);
        assert!(laptop.list_peer_courier_urls().unwrap().is_empty());
    }

    #[test]
    fn the_old_default_courier_set_gets_the_relay_pool_once_and_an_edited_one_is_left_alone() {
        let open = |name: &str| {
            let db_path = std::env::temp_dir()
                .join("omni-rendezvous-providers-tests")
                .join(format!("{name}-{}.db", unique_suffix()));
            let storage = Storage::open(&db_path).expect("storage should open");
            storage.migrate().expect("migrations should succeed");
            (storage, db_path)
        };
        let reopen = |db_path: &std::path::Path| {
            let storage = Storage::open(db_path).unwrap();
            storage.migrate().unwrap();
            storage
        };
        let back_to_the_old_set = |storage: &Storage| {
            storage
                .connection
                .execute(
                    "DELETE FROM rendezvous_providers WHERE url NOT IN ('https://ntfy.sh', 'wss://offchain.pub', 'dht://mainline')",
                    [],
                )
                .unwrap();
        };
        let urls = |storage: &Storage| {
            let mut urls: Vec<String> = storage.list_rendezvous_providers().unwrap().into_iter().map(|p| p.url).collect();
            urls.sort();
            urls
        };
        // The seeded set (mirrors omni_rendezvous::DEFAULT_RENDEZVOUS_PROVIDERS; storage does not depend on it).
        let mut pool: Vec<String> = [
            "https://ntfy.sh", "wss://nos.lol", "wss://nostr.mom", "wss://nostr.oxtr.dev",
            "wss://relay.primal.net", "wss://relay.nostr.net", "wss://offchain.pub", "dht://mainline",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        pool.sort();

        let (old, old_path) = open("old-set");
        back_to_the_old_set(&old);
        drop(old);
        let old = reopen(&old_path);
        assert_eq!(urls(&old), pool, "the old set got the pool");
        let removed = old.list_rendezvous_providers().unwrap().into_iter().find(|p| p.url == "wss://nos.lol").unwrap();
        old.remove_rendezvous_provider(removed.id).unwrap();
        drop(old);
        let old = reopen(&old_path);
        assert!(!urls(&old).contains(&"wss://nos.lol".to_string()), "a relay removed later stays removed");

        let (edited, edited_path) = open("edited-set");
        back_to_the_old_set(&edited);
        edited.add_rendezvous_provider("wss://relay.example", crate::RENDEZVOUS_PROVIDER_SOURCE_MANUAL, 1).unwrap();
        drop(edited);
        let edited = reopen(&edited_path);
        assert_eq!(urls(&edited).len(), 4, "an edited set is the user's: {:?}", urls(&edited));
    }

    #[test]
    fn rendezvous_providers_seed_add_remove_survive_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-rendezvous-providers-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        // migrate() seeds the cross-protocol defaults (source 'base'): ntfy (HTTP), the Nostr relay pool
        // (WS), BT-DHT (dht://mainline) — mirrors omni_rendezvous::DEFAULT_RENDEZVOUS_PROVIDERS.
        let seeded = storage.list_rendezvous_providers().unwrap();
        assert_eq!(seeded.len(), 8, "ntfy + six pool relays + Mainline seeded");
        assert!(seeded.iter().all(|p| p.source == crate::RENDEZVOUS_PROVIDER_SOURCE_BASE));
        let seeded_urls: std::collections::HashSet<&str> =
            seeded.iter().map(|p| p.url.as_str()).collect();
        assert!(seeded_urls.contains("https://ntfy.sh"), "ntfy seeded");
        assert!(seeded_urls.contains("wss://offchain.pub"), "nostr seeded");
        assert!(seeded_urls.contains("dht://mainline"), "bt-dht seeded");

        // Add a manual + an invite-contributed provider; dup is an idempotent no-op; exists() reports.
        assert_eq!(
            storage
                .add_rendezvous_provider("wss://relay.example", crate::RENDEZVOUS_PROVIDER_SOURCE_MANUAL, 2_000)
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .add_rendezvous_provider("https://ntfy.sh", crate::RENDEZVOUS_PROVIDER_SOURCE_INVITE, 3_000)
                .unwrap(),
            0,
            "re-adding a known url is a no-op (UNIQUE)"
        );
        assert!(storage.rendezvous_provider_exists("wss://relay.example").unwrap());
        assert!(!storage.rendezvous_provider_exists("https://nope.example").unwrap());
        assert_eq!(storage.list_rendezvous_providers().unwrap().len(), 9);

        // A seeded default is removable (empty set falls back to the compiled default), and the removal
        // survives reopen.
        let ntfy_id = seeded.iter().find(|p| p.url == "https://ntfy.sh").unwrap().id;
        assert_eq!(storage.remove_rendezvous_provider(ntfy_id).unwrap(), 1);
        assert_eq!(storage.remove_rendezvous_provider(ntfy_id).unwrap(), 0, "removing a gone id is a no-op");

        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        let after = reopened.list_rendezvous_providers().unwrap();
        assert_eq!(after.len(), 8, "registry outlives restart, removed default stays removed");
        assert!(after.iter().all(|p| p.url != "https://ntfy.sh"));
    }

    #[test]
    fn bootstrap_invites_persist_filter_expiry_and_survive_reopen() {
        // Outstanding first-contact bootstrap invites persist across a restart (were in-memory only).
        // See docs/bugs/relay-less-first-contact-validate-before-advertise.md.
        let db_path = std::env::temp_dir()
            .join("omni-bootstrap-invites-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        let node = [0x7Au8; 20];
        // Importer-side invite (knows the inviter node) + an inviter-side one (node = None).
        storage.upsert_bootstrap_invite("aa".repeat(32).as_str(), Some(node), 10_000).unwrap();
        storage.upsert_bootstrap_invite("bb".repeat(32).as_str(), None, 5_000).unwrap();

        // Only non-expired invites come back, soonest-expiry first; the node round-trips both ways.
        let active = storage.list_active_bootstrap_invites(4_000).unwrap();
        assert_eq!(active.len(), 2, "both still active at t=4000");
        assert_eq!(active[0].nonce_hex, "bb".repeat(32), "soonest expiry first");
        assert_eq!(active[0].inviter_node, None, "inviter side has no node");
        assert_eq!(active[1].inviter_node, Some(node), "importer side node round-trips");

        // At t=6000 the bb invite has expired → filtered out of the active list.
        let active = storage.list_active_bootstrap_invites(6_000).unwrap();
        assert_eq!(active.len(), 1, "expired invite filtered");
        assert_eq!(active[0].nonce_hex, "aa".repeat(32));

        // Upsert is idempotent on the nonce (refresh node/expiry, no duplicate).
        storage.upsert_bootstrap_invite("aa".repeat(32).as_str(), Some(node), 20_000).unwrap();
        assert_eq!(storage.list_active_bootstrap_invites(15_000).unwrap().len(), 1, "refreshed, not dup'd");

        // Prune sweeps the expired bb row; explicit remove drops the aa one.
        assert_eq!(storage.prune_expired_bootstrap_invites(6_000).unwrap(), 1, "bb pruned");
        assert_eq!(storage.remove_bootstrap_invite(&"aa".repeat(32)).unwrap(), 1);

        // Re-add + reopen: the persisted invite outlives the restart.
        storage.upsert_bootstrap_invite("cc".repeat(32).as_str(), Some(node), 30_000).unwrap();
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        let after = reopened.list_active_bootstrap_invites(1_000).unwrap();
        assert_eq!(after.len(), 1, "invite outlives restart");
        assert_eq!(after[0].nonce_hex, "cc".repeat(32));
        assert_eq!(after[0].inviter_node, Some(node));

        // A completed pairing spends every invite aimed at that node — and only those.
        let other = [7u8; 20];
        reopened.upsert_bootstrap_invite("dd".repeat(32).as_str(), Some(other), 30_000).unwrap();
        assert_eq!(reopened.remove_bootstrap_invites_for_inviter(&node).unwrap(), 1);
        let left = reopened.list_active_bootstrap_invites(1_000).unwrap();
        assert_eq!(left.len(), 1, "another inviter's invite stays");
        assert_eq!(left[0].inviter_node, Some(other));
    }

    #[test]
    fn relay_bundle_tag_round_trips_and_a_newer_version_replaces_the_set() {
        // R1c: a relay imported from a signed `om3relay:` bundle carries (bundle_id, bundle_version);
        // rotating to a newer version REPLACES the same bundle_id's rows while leaving manual nodes and
        // other bundles untouched. See docs/decisions/r1c-rotatable-white-relay.md.
        let db_path = std::env::temp_dir()
            .join("omni-relay-bundle-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage.clear_network_nodes().unwrap();

        // No version yet for an unknown bundle.
        assert_eq!(storage.max_bundle_version("alice").unwrap(), None);

        // A manual node (untagged) co-exists with bundle "alice" v1 (two relays).
        storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("198.51.100.9:5000"),
                label: "Ручной релей",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 1_000,
                ..Default::default()
            })
            .unwrap();
        for (i, ep) in ["203.0.113.1:443", "203.0.113.2:443"].iter().enumerate() {
            storage
                .add_network_node(&crate::NewNetworkNode {
                    endpoint: Some(ep),
                    label: "alice-relay",
                    source: crate::NETWORK_NODE_SOURCE_MANUAL,
                    added_at_unix_ms: 2_000 + i as i64,
                    bundle_id: Some("alice"),
                    bundle_version: Some(1),
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(storage.max_bundle_version("alice").unwrap(), Some(1));
        assert_eq!(storage.list_network_nodes().unwrap().len(), 3);

        // Round-trip: the tags survive read-back.
        let alice_rows: Vec<_> = storage
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.bundle_id.as_deref() == Some("alice"))
            .collect();
        assert_eq!(alice_rows.len(), 2);
        assert!(alice_rows.iter().all(|n| n.bundle_version == Some(1)));

        // Rotation: publish alice v2 = wipe alice's prior rows, then insert the new (single) relay.
        // Manual node and any other bundle are untouched.
        assert_eq!(storage.remove_network_nodes_by_bundle("alice").unwrap(), 2);
        storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("203.0.113.9:443"),
                label: "alice-relay",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 3_000,
                bundle_id: Some("alice"),
                bundle_version: Some(2),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(storage.max_bundle_version("alice").unwrap(), Some(2));
        let after: Vec<_> = storage.list_network_nodes().unwrap();
        assert_eq!(after.len(), 2, "manual node + the single v2 alice relay");
        let alice_after: Vec<_> = after
            .iter()
            .filter(|n| n.bundle_id.as_deref() == Some("alice"))
            .collect();
        assert_eq!(alice_after.len(), 1);
        assert_eq!(alice_after[0].endpoint.as_deref(), Some("203.0.113.9:443"));
        assert_eq!(alice_after[0].bundle_version, Some(2));
        // The manual node survives the bundle rotation.
        assert!(after
            .iter()
            .any(|n| n.bundle_id.is_none() && n.endpoint.as_deref() == Some("198.51.100.9:5000")));

        // Removing a non-existent bundle is a no-op; an untagged registry reports no version.
        assert_eq!(storage.remove_network_nodes_by_bundle("nobody").unwrap(), 0);
    }

    #[test]
    fn shared_flag_marks_my_own_relays_and_survives_reopen() {
        // R1c increment 3b: the `shared` flag marks the relays I publish as MY OWN bundle. It defaults
        // off, toggles per row, is read back via list_shared_network_nodes, and persists across reopen
        // (it's the issuer side of beacon auto-propagation). See docs/decisions/r1c-rotatable-white-relay.md.
        let db_path = std::env::temp_dir()
            .join("omni-relay-shared-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage.clear_network_nodes().unwrap();

        let mine = storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("203.0.113.5:443"),
                label: "Мой релей",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 1_000,
                ..Default::default()
            })
            .unwrap();
        let other = storage
            .add_network_node(&crate::NewNetworkNode {
                endpoint: Some("198.51.100.9:443"),
                label: "Чужой релей",
                source: crate::NETWORK_NODE_SOURCE_MANUAL,
                added_at_unix_ms: 2_000,
                ..Default::default()
            })
            .unwrap();

        // Off by default — nothing is published as mine.
        assert!(storage.list_network_nodes().unwrap().iter().all(|n| !n.shared));
        assert!(storage.list_shared_network_nodes().unwrap().is_empty());

        // Flag one row → only it is shared; round-trips on read.
        assert_eq!(storage.set_network_node_shared(mine, true).unwrap(), 1);
        let shared = storage.list_shared_network_nodes().unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].endpoint.as_deref(), Some("203.0.113.5:443"));
        assert!(shared[0].shared);
        assert!(storage
            .list_network_nodes()
            .unwrap()
            .iter()
            .find(|n| n.id == other)
            .is_some_and(|n| !n.shared));

        // Survives reopen (durable user setting).
        drop(storage);
        let storage = Storage::open(&db_path).expect("storage reopens");
        assert_eq!(storage.list_shared_network_nodes().unwrap().len(), 1);

        // Unflag → no longer published; toggling an unknown id is a no-op.
        assert_eq!(storage.set_network_node_shared(mine, false).unwrap(), 1);
        assert!(storage.list_shared_network_nodes().unwrap().is_empty());
        assert_eq!(storage.set_network_node_shared(987654, true).unwrap(), 0);
    }

    #[test]
    fn durable_feed_seq_is_monotonic_per_peer_and_survives_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-durable-seq-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");

        let a = vec![1u8; 20];
        let b = vec![2u8; 20];
        assert_eq!(storage.next_durable_seq(&a).unwrap(), 1);
        assert_eq!(storage.next_durable_seq(&a).unwrap(), 2);
        assert_eq!(storage.next_durable_seq(&b).unwrap(), 1); // independent per peer
        assert_eq!(storage.next_durable_seq(&a).unwrap(), 3);

        // Survives reopen — the point: a restart must NOT reuse a seq (else the
        // recipient false-dedups the post-restart message against a held frame).
        drop(storage);
        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        assert_eq!(reopened.next_durable_seq(&a).unwrap(), 4);
        assert_eq!(reopened.next_durable_seq(&b).unwrap(), 2);
    }

    #[test]
    fn local_instance_id_is_stable_across_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let first = storage
            .get_or_create_local_instance_id()
            .expect("instance id should be created");

        drop(storage);

        let reopened = Storage::open(&db_path).expect("storage should reopen");
        reopened.migrate().expect("migrations should succeed");
        let second = reopened
            .get_or_create_local_instance_id()
            .expect("instance id should reload");

        assert_eq!(first, second);
    }

    #[test]
    fn delete_local_data_removes_database_attachments_and_known_sidecars() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("cleanup-{}.db", unique_suffix()));
        let app_directory = db_path.parent().expect("db path should have parent");
        let db_file_name = db_path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("db path should have a file name")
            .to_string();
        fs::create_dir_all(app_directory).expect("app directory should exist");
        fs::write(&db_path, "db").expect("db file should be written");
        fs::write(db_path.with_file_name(format!("{db_file_name}-wal")), "wal")
            .expect("wal should be written");
        fs::write(db_path.with_file_name(format!("{db_file_name}-shm")), "shm")
            .expect("shm should be written");
        fs::write(
            db_path.with_file_name(format!("{db_file_name}-journal")),
            "journal",
        )
        .expect("journal should be written");
        fs::write(app_directory.join("transport-mode.json"), "{}").expect("transport mode sidecar");
        fs::write(app_directory.join("node-mode.json"), "{}").expect("node mode sidecar");
        fs::write(app_directory.join("discovery-publish-mode.json"), "{}")
            .expect("publish mode sidecar");
        fs::write(app_directory.join("discovery-probe-mode.json"), "{}")
            .expect("probe mode sidecar");
        fs::write(app_directory.join("stun-servers.json"), "{}").expect("stun servers sidecar");
        fs::write(app_directory.join("transport-identity.key"), "legacy")
            .expect("legacy identity sidecar");
        fs::write(
            app_directory.join(format!("{db_file_name}.transport-identity.key")),
            "scoped-identity",
        )
        .expect("scoped identity sidecar");
        // Sidecars added after the wipe list was first written. Each one used to survive a "delete
        // local data" — the DHT cache in particular is a list of peers we had contacted. They are
        // covered by the prefix rule now, so this asserts the rule, not a longer list.
        for keyed in [
            "dht-nodes-cache.json",
            "dht-rendezvous.json",
            "discoverability-mode.json",
            "carrier-policy.json",
            "connection-privacy-mode.json",
        ] {
            fs::write(
                app_directory.join(format!("{db_file_name}.{keyed}")),
                "trace",
            )
            .expect("keyed sidecar");
        }
        // A sidecar of a DIFFERENT database in the same directory must be left alone.
        let other_db_sidecar =
            app_directory.join(format!("other-{db_file_name}.transport-identity.key"));
        fs::write(&other_db_sidecar, "not ours").expect("other db sidecar");
        fs::create_dir_all(app_directory.join("attachments").join("blobs"))
            .expect("attachments directory should exist");
        fs::write(
            app_directory
                .join("attachments")
                .join("blobs")
                .join("blob.bin"),
            "blob",
        )
        .expect("attachment blob should be written");

        Storage::delete_local_data_at_path(&db_path).expect("local data cleanup should succeed");

        for target in [
            db_path.clone(),
            db_path.with_file_name(format!("{db_file_name}-wal")),
            db_path.with_file_name(format!("{db_file_name}-shm")),
            db_path.with_file_name(format!("{db_file_name}-journal")),
            app_directory.join("transport-mode.json"),
            app_directory.join("node-mode.json"),
            app_directory.join("discovery-publish-mode.json"),
            app_directory.join("discovery-probe-mode.json"),
            app_directory.join("stun-servers.json"),
            app_directory.join("transport-identity.key"),
            app_directory.join(format!("{db_file_name}.transport-identity.key")),
            app_directory.join(format!("{db_file_name}.dht-nodes-cache.json")),
            app_directory.join(format!("{db_file_name}.dht-rendezvous.json")),
            app_directory.join(format!("{db_file_name}.discoverability-mode.json")),
            app_directory.join(format!("{db_file_name}.carrier-policy.json")),
            app_directory.join(format!("{db_file_name}.connection-privacy-mode.json")),
        ] {
            assert!(
                !target.exists(),
                "expected '{}' to be removed",
                target.display()
            );
        }
        assert!(
            !app_directory.join("attachments").exists(),
            "attachments directory should be removed"
        );
        assert!(
            other_db_sidecar.exists(),
            "another database's sidecar must not be swept up by the prefix rule"
        );
        let _ = fs::remove_file(&other_db_sidecar);
    }

    /// The last local profile is deleted while the db is still OPEN (the app needs it for the
    /// onboarding screen). Everything that identifies the device must go anyway — above all the mesh
    /// transport identity, or the next profile silently comes up as the same node.
    #[test]
    fn deleting_device_traces_keeps_the_open_database_but_removes_the_identity_and_caches() {
        // Per-test ISOLATED app directory (unique subdir), NOT the shared test dir. This test exercises
        // `delete_device_traces_keeping_open_database`, which wipes DIRECTORY-WIDE roots keyed off the db
        // path's PARENT — `profile-media/`, the attachments root, and the fixed-name settings sidecars —
        // not just db-name-keyed files. Sharing the parent with other concurrent tests raced their
        // media/sidecars (their `remove_dir_all` vs our writes and vice versa) — a real flake. A unique
        // parent confines every dir-wide op to this test.
        let app_directory = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("device-traces-{}", unique_suffix()));
        let db_path = app_directory.join("device.db");
        let db_file_name = "device.db".to_string();
        fs::create_dir_all(&app_directory).expect("app directory should exist");
        fs::write(&db_path, "db").expect("db file should be written");
        fs::write(db_path.with_file_name(format!("{db_file_name}-wal")), "wal")
            .expect("wal should be written");
        fs::write(db_path.with_file_name(format!("{db_file_name}-shm")), "shm")
            .expect("shm should be written");
        fs::write(
            app_directory.join(format!("{db_file_name}.transport-identity.key")),
            "mesh-identity",
        )
        .expect("identity sidecar");
        fs::write(
            app_directory.join(format!("{db_file_name}.dht-nodes-cache.json")),
            "peers",
        )
        .expect("dht cache sidecar");

        Storage::delete_device_traces_keeping_open_database(&db_path).expect("device trace wipe");

        assert!(db_path.exists(), "the open database must survive");
        assert!(
            db_path.with_file_name(format!("{db_file_name}-wal")).exists(),
            "the open database's WAL must survive"
        );
        assert!(
            db_path.with_file_name(format!("{db_file_name}-shm")).exists(),
            "the open database's SHM must survive"
        );
        assert!(
            !app_directory
                .join(format!("{db_file_name}.transport-identity.key"))
                .exists(),
            "the mesh transport identity must not outlive the last profile"
        );
        assert!(
            !app_directory
                .join(format!("{db_file_name}.dht-nodes-cache.json"))
                .exists(),
            "the DHT peer cache must not outlive the last profile"
        );

        let _ = fs::remove_dir_all(&app_directory);
    }

    /// Another device's profile change is taken when it is newer: an older one is ignored, the same
    /// one is no change, a different avatar waits for its picture, and a picture must match its hash.
    #[test]
    fn a_profile_change_from_our_other_device_is_taken_when_newer() {
        let dir = std::env::temp_dir().join("omni-stage-2-storage-tests");
        fs::create_dir_all(&dir).unwrap();
        let avatar_source_path = dir.join(format!("own-device-avatar-{}.png", unique_suffix()));
        fs::write(&avatar_source_path, tiny_png_bytes()).unwrap();
        // The other device: its avatar, as it hands it over.
        let other = Storage::open(dir.join(format!("own-device-other-{}.db", unique_suffix()))).unwrap();
        other.migrate().unwrap();
        other.create_local_account("Alice", "Laptop", 1_000).unwrap();
        other.update_local_public_profile("Alice", Some(&avatar_source_path), false, 2_000).unwrap();
        let picture = other.get_local_public_profile_state().unwrap();
        let (hash, mime, bytes) = (picture.avatar_hash_hex.unwrap(), picture.avatar_mime_type.unwrap(), picture.avatar_bytes.unwrap());

        let storage = Storage::open(dir.join(format!("own-device-{}.db", unique_suffix()))).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Desktop", 1_000).unwrap();
        storage.update_local_public_profile("Alice Desk", None, false, 1_500).unwrap();

        assert!(!storage.adopt_public_profile_from_own_device(Some("Old"), None, None, None, 1_400).unwrap());
        assert!(!storage.adopt_public_profile_from_own_device(Some("Alice Desk"), None, None, None, 1_500).unwrap());
        assert!(storage.adopt_public_profile_from_own_device(Some("Alice"), Some(&hash), Some(&mime), None, 2_000).unwrap());
        let state = storage.get_local_public_profile_state().unwrap();
        assert_eq!((state.display_name.as_str(), state.updated_at_unix_ms), ("Alice", 2_000));
        assert!(state.avatar_hash_hex.is_none(), "the avatar waits for its picture");

        assert!(storage
            .adopt_public_profile_from_own_device(None, Some(&hash), Some(&mime), Some(b"not it"), 2_000)
            .is_err());
        assert!(storage.adopt_public_profile_from_own_device(None, Some(&hash), Some(&mime), Some(&bytes), 2_000).unwrap());
        let state = storage.get_local_public_profile_state().unwrap();
        assert_eq!(state.avatar_hash_hex.as_deref(), Some(hash.as_str()));
        assert_eq!(state.avatar_bytes.as_deref(), Some(bytes.as_slice()));
        assert!(!storage.adopt_public_profile_from_own_device(None, Some(&hash), Some(&mime), Some(&bytes), 2_000).unwrap());

        // A descriptor without an avatar, newer: the avatar goes.
        assert!(storage.adopt_public_profile_from_own_device(Some("Alice"), None, None, None, 3_000).unwrap());
        assert!(storage.get_local_public_profile_state().unwrap().avatar_hash_hex.is_none());
    }

    #[test]
    fn updating_local_public_profile_persists_avatar_and_can_clear_it() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("public-profile-{}.db", unique_suffix()));
        let avatar_source_path = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("public-profile-avatar-{}.png", unique_suffix()));

        fs::create_dir_all(
            avatar_source_path
                .parent()
                .expect("avatar path should have parent"),
        )
        .expect("avatar directory should exist");
        fs::write(&avatar_source_path, tiny_png_bytes()).expect("avatar file should be written");

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let created = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let profile_id = created
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let updated = storage
            .update_local_public_profile("Alice Cooper", Some(&avatar_source_path), false, 1_100)
            .expect("public profile should update");
        assert_eq!(updated.descriptor.display_name, "Alice Cooper");
        assert!(updated.public_avatar_hash_hex.is_some());
        assert_eq!(
            updated.public_avatar_mime_type.as_deref(),
            Some("image/jpeg")
        );
        let avatar_image_path = updated
            .public_avatar_image_path
            .clone()
            .expect("normalized avatar path should exist");
        assert!(
            avatar_image_path.contains("\\profile-media\\p\\"),
            "new avatar path should use compact media layout: {avatar_image_path}"
        );
        assert!(
            Path::new(&avatar_image_path).exists(),
            "normalized avatar file should exist"
        );

        let state = storage
            .get_local_public_profile_state()
            .expect("local public profile state should load");
        assert_eq!(state.display_name, "Alice Cooper");
        assert!(state.avatar_bytes.is_some());
        assert_eq!(state.avatar_hash_hex, updated.public_avatar_hash_hex);

        let cleared = storage
            .update_local_public_profile("Alice Cooper", None, true, 1_200)
            .expect("avatar should clear");
        assert_eq!(cleared.descriptor.display_name, "Alice Cooper");
        assert!(cleared.public_avatar_hash_hex.is_none());
        assert!(cleared.public_avatar_mime_type.is_none());
        assert!(cleared.public_avatar_image_path.is_none());
        assert!(
            !Path::new(&avatar_image_path).exists(),
            "avatar file should be removed after clear"
        );

        let reopened = storage
            .get_local_account_by_profile_id(&profile_id)
            .expect("local account should reload")
            .expect("local account should still exist");
        assert!(reopened.public_avatar_hash_hex.is_none());
        assert!(reopened.public_avatar_image_path.is_none());
    }

    #[test]
    fn contact_avatar_resolution_falls_back_to_legacy_media_layout() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-2-storage-tests")
            .join(format!("legacy-contact-avatar-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let created = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let profile_id = created
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        let avatar_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let remote_account_id = "acct-bob";
        let legacy_avatar_path = db_path
            .parent()
            .expect("db path should have parent")
            .join("profile-media")
            .join(&profile_id.0)
            .join("contacts")
            .join(format!("{remote_account_id}-{avatar_hash}.jpg"));
        fs::create_dir_all(
            legacy_avatar_path
                .parent()
                .expect("legacy avatar path should have parent"),
        )
        .expect("legacy avatar directory should exist");
        fs::write(&legacy_avatar_path, tiny_png_bytes()).expect("legacy avatar should be written");
        storage
            .connection
            .execute(
                "
                INSERT INTO contacts (
                    contact_id,
                    petname,
                    remote_account_id,
                    public_display_name,
                    public_avatar_hash_hex,
                    public_avatar_mime_type,
                    public_profile_updated_at_unix_ms,
                    remote_instance_id,
                    relationship_state,
                    has_met,
                    is_blocked,
                    removed_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'approved', 1, 0, NULL, ?9, ?10, ?11)
                ",
                rusqlite::params![
                    "contact-bob",
                    "Bob",
                    remote_account_id,
                    "Bob",
                    avatar_hash,
                    "image/jpeg",
                    1_100,
                    "inst-bob",
                    1_050,
                    1_100,
                    profile_id.0.as_str(),
                ],
            )
            .expect("contact should insert");

        let contact = storage
            .get_contact(&ContactId("contact-bob".to_string()))
            .expect("contact should load");

        assert_eq!(
            contact.public_avatar_image_path.as_deref(),
            Some(legacy_avatar_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn message_storage_round_trip_works() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let outgoing = storage
            .insert_outgoing_message(
                "outgoing-1",
                &dialog_key_from_instance_id("peer-1"),
                "hello from sender",
                MessageStatus::Queued,
                10,
            )
            .expect("outgoing message should be inserted");
        assert_eq!(outgoing.dialog_key, dialog_key_from_instance_id("peer-1"));
        assert_eq!(outgoing.direction, ChatDirection::Outgoing);
        assert_eq!(outgoing.status, MessageStatus::Queued);

        let updated = storage
            .update_message_status("outgoing-1", MessageStatus::Sent, 11)
            .expect("message status should update");
        assert_eq!(updated.status, MessageStatus::Sent);
        assert_eq!(updated.updated_at_unix_ms, 11);

        let incoming = storage
            .insert_incoming_message(
                "incoming-1",
                &dialog_key_from_instance_id("peer-1"),
                "hello back",
                12,
            )
            .expect("incoming message should be inserted");
        assert_eq!(incoming.dialog_key, dialog_key_from_instance_id("peer-1"));
        assert_eq!(incoming.direction, ChatDirection::Incoming);
        assert_eq!(incoming.status, MessageStatus::Delivered);

        let messages = storage
            .list_messages_for_dialog(&dialog_key_from_instance_id("peer-1"))
            .expect("messages should be listed by dialog key");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, "outgoing-1");
        assert_eq!(messages[1].id, "incoming-1");
    }

    #[test]
    fn message_paging_uses_exclusive_cursor_and_returns_ascending_pages() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("messages-page-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let dialog_key = dialog_key_from_instance_id("peer-page");
        for (message_id, created_at_unix_ms) in [
            ("msg-1", 100),
            ("msg-2", 100),
            ("msg-3", 100),
            ("msg-4", 200),
        ] {
            storage
                .insert_outgoing_message(
                    message_id,
                    &dialog_key,
                    message_id,
                    MessageStatus::Queued,
                    created_at_unix_ms,
                )
                .expect("message should be inserted");
        }

        let first_page = storage
            .list_messages_page_for_dialog(&dialog_key, None, None, 2)
            .expect("first page should load");
        assert_eq!(first_page.dialog_key, dialog_key);
        assert!(first_page.has_older);
        assert_eq!(first_page.messages.len(), 2);
        assert_eq!(first_page.messages[0].id, "msg-3");
        assert_eq!(first_page.messages[1].id, "msg-4");
        assert_eq!(first_page.oldest_loaded_created_at_unix_ms, Some(100));
        assert_eq!(
            first_page.oldest_loaded_message_id.as_deref(),
            Some("msg-3")
        );

        let second_page = storage
            .list_messages_page_for_dialog(
                &dialog_key,
                first_page.oldest_loaded_created_at_unix_ms,
                first_page.oldest_loaded_message_id.as_deref(),
                2,
            )
            .expect("second page should load");
        assert!(!second_page.has_older);
        assert_eq!(second_page.messages.len(), 2);
        assert_eq!(second_page.messages[0].id, "msg-1");
        assert_eq!(second_page.messages[1].id, "msg-2");
        assert_eq!(second_page.oldest_loaded_created_at_unix_ms, Some(100));
        assert_eq!(
            second_page.oldest_loaded_message_id.as_deref(),
            Some("msg-1")
        );

        let partial_cursor_error = storage
            .list_messages_page_for_dialog(&dialog_key, Some(100), None, 2)
            .expect_err("partial cursor should be rejected");
        assert!(partial_cursor_error.to_string().contains(
            "before cursor must include both before_created_at_unix_ms and before_message_id"
        ));

        let zero_limit_error = storage
            .list_messages_page_for_dialog(&dialog_key, None, None, 0)
            .expect_err("zero limit should be rejected");
        assert!(zero_limit_error
            .to_string()
            .contains("limit must be greater than zero"));
    }

    #[test]
    fn forgetting_contact_removes_dialog_history_and_queue_rows() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("forget-dialog-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("local profile summary should exist")
            .profile_id
            .clone();

        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("contact should be added");

        let dialog_key = contact
            .primary_dialog_key
            .clone()
            .expect("contact should have a primary dialog key");

        storage
            .insert_outgoing_message(
                "forget-outgoing-1",
                &dialog_key,
                "hello from sender",
                MessageStatus::Queued,
                1_120,
            )
            .expect("outgoing message should be inserted");
        storage
            .insert_incoming_message(
                "forget-incoming-1",
                &dialog_key,
                "hello back",
                1_130,
            )
            .expect("incoming message should be inserted");
        storage
            .upsert_message_security_metadata(
                "forget-outgoing-1",
                &dialog_key,
                Some(&contact.contact_id),
                None,
                contact.remote_account_id.as_ref(),
                None,
                "encrypted_pending",
                None,
                Some("chat"),
                1_120,
                1_140,
            )
            .expect("message security metadata should be inserted");

        let removed = storage
            .forget_contact(&contact.contact_id, 1_200)
            .expect("contact should be forgotten");
        assert_eq!(removed.contact_id, contact.contact_id);

        assert_eq!(
            storage.count_profile_rows("messages", &profile_id).unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("outbound_queue", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("inbound_dedup", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("delivery_receipts", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("message_security_metadata", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("contact_dialog_links", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage.count_profile_rows("contacts", &profile_id).unwrap(),
            0
        );
    }

    #[test]
    fn outgoing_pending_refresh_does_not_mark_approved_contact_as_pending_direction() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("approved-pending-direction-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let remote_account_id = AccountId("acct-bob".to_string());
        let approved = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(remote_account_id.clone()),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("approved contact should be created");
        assert_eq!(
            approved.relationship_state,
            ContactRelationshipState::Approved
        );
        assert_eq!(approved.pending_request_direction, None);

        let refreshed = storage
            .add_outgoing_pending_contact(
                &AddContactInput {
                    petname: remote_account_id.0.clone(),
                    remote_account_id: Some(remote_account_id.clone()),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_200,
            )
            .expect("refreshing an approved contact should be idempotent");

        assert_eq!(refreshed.contact_id, approved.contact_id);
        assert_eq!(
            refreshed.relationship_state,
            ContactRelationshipState::Approved
        );
        assert_eq!(refreshed.pending_request_direction, None);

        let loaded = storage
            .find_contact_by_remote_account_id(&remote_account_id)
            .expect("lookup should succeed")
            .expect("contact should exist");
        assert_eq!(loaded.pending_request_direction, None);

        let incoming = storage
            .add_pending_contact(
                &AddContactInput {
                    petname: "Charlie".to_string(),
                    remote_account_id: Some(AccountId("acct-charlie".to_string())),
                    public_display_name: Some("Charlie".to_string()),
                    remote_instance_id: Some("inst-charlie".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-charlie")),
                    initial_observation: None,
                },
                1_300,
            )
            .expect("incoming pending contact should be created");
        assert_eq!(
            incoming.pending_request_direction,
            Some(ContactRequestDirection::Incoming)
        );
    }

    #[test]
    fn omnis_own_copy_of_a_file_is_encrypted_and_the_users_copy_is_plain() {
        let dir = std::env::temp_dir().join("omni-stage-1-storage-tests").join(format!("at-rest-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("omni.db");
        let source_path = dir.join("source.txt");
        let export_path = dir.join("export.txt");
        let text = "a file the user sends, which must not lie open in Omni's folder";
        fs::write(&source_path, text).unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage.create_local_account("Alice", "Office PC", 1_000).expect("account should be created");
        let prepared = storage
            .prepare_outgoing_attachment(
                "at-rest-message",
                &dialog_key_from_instance_id("inst-bob"),
                source_path.to_str().unwrap(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");
        let blob = prepared.local_blob_path.clone().expect("a managed copy");
        assert!(omni_crypto::is_file_at_rest(&blob), "Omni's own copy is encrypted");
        let raw = fs::read(&blob).unwrap();
        assert!(!raw.windows(text.len()).any(|window| window == text.as_bytes()));
        assert_eq!(storage.read_media_file(&blob).unwrap(), text.as_bytes());
        assert_eq!(prepared.attachment.file_size_bytes, text.len() as i64, "sizes are the plaintext's");

        storage.save_attachment_copy_to_path("at-rest-message", &export_path, 1_020).expect("saved");
        assert_eq!(fs::read_to_string(&export_path).unwrap(), text, "where the user chose, it is plain");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_files_left_in_omnis_storage_are_sealed_and_an_interrupted_sealing_recovers() {
        let dir = std::env::temp_dir().join("omni-stage-1-storage-tests").join(format!("seal-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("omni.db");
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let photos = dir.join("attachments").join("photos");
        fs::create_dir_all(&photos).unwrap();
        let old = photos.join("old.jpg");
        fs::write(&old, b"an old plain photo").unwrap();
        // A run cut short after the plaintext stepped aside: the sealed copy waits under `.sealing`.
        let cut = photos.join("cut.jpg");
        fs::write(photos.join("cut.jpg.plain-old"), b"cut short").unwrap();
        storage.write_media_file(&photos.join("cut.jpg.sealing"), b"cut short").unwrap();
        // A user's file outside Omni's storage is never touched.
        let outside = dir.join("outside.jpg");
        fs::write(&outside, b"the user's").unwrap();

        assert_eq!(storage.seal_plain_attachment_files().unwrap(), 1);
        assert!(omni_crypto::is_file_at_rest(&old));
        assert_eq!(storage.read_media_file(&old).unwrap(), b"an old plain photo");
        assert!(omni_crypto::is_file_at_rest(&cut));
        assert_eq!(storage.read_media_file(&cut).unwrap(), b"cut short");
        assert!(!photos.join("cut.jpg.plain-old").exists() && !photos.join("cut.jpg.sealing").exists());
        assert_eq!(fs::read(&outside).unwrap(), b"the user's");
        assert_eq!(storage.seal_plain_attachment_files().unwrap(), 0, "nothing left to seal");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_attachment_copy_can_be_saved_repeatedly_even_after_access_is_revoked() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-copy-{}.db", unique_suffix()));
        let source_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("source-{}.txt", unique_suffix()));
        let first_export_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("export-1-{}.txt", unique_suffix()));
        let second_export_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("export-2-{}.txt", unique_suffix()));

        fs::create_dir_all(
            source_path
                .parent()
                .expect("source path should have a parent"),
        )
        .unwrap();
        fs::write(&source_path, "omni attachment holder copy").unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let prepared = storage
            .prepare_outgoing_attachment(
                "attachment-message-1",
                &dialog_key_from_instance_id("inst-bob"),
                source_path
                    .to_str()
                    .expect("source path should be valid UTF-8"),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");
        let managed_blob_path = prepared
            .local_blob_path
            .clone()
            .expect("managed blob path should exist for outgoing attachment");
        assert!(managed_blob_path.exists());

        let first_saved = storage
            .save_attachment_copy_to_path("attachment-message-1", &first_export_path, 1_020)
            .expect("first local copy should be saved");
        assert!(first_saved.attachment.local_blob_available);
        assert_eq!(
            fs::read_to_string(&first_export_path).unwrap(),
            "omni attachment holder copy"
        );

        let revoked = storage
            .revoke_local_attachment_access("attachment-message-1", 1_030)
            .expect("local serving should be revoked");
        assert!(revoked.attachment.local_blob_available);
        assert!(!revoked.attachment.local_serving_enabled);
        assert!(!managed_blob_path.exists());

        let second_saved = storage
            .save_attachment_copy_to_path("attachment-message-1", &second_export_path, 1_040)
            .expect("second local copy should still be saved");
        assert!(second_saved.attachment.local_blob_available);
        assert!(!second_saved.attachment.local_serving_enabled);
        assert_eq!(
            fs::read_to_string(&second_export_path).unwrap(),
            "omni attachment holder copy"
        );
        assert_eq!(
            second_saved.attachment.local_saved_path.as_deref(),
            Some(
                second_export_path
                    .to_str()
                    .expect("export path should be valid UTF-8")
            )
        );
    }

    #[test]
    fn large_outgoing_attachment_uses_original_file_without_managed_copy() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("large-attachment-copy-{}.db", unique_suffix()));
        let source_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("large-source-{}.bin", unique_suffix()));

        fs::create_dir_all(
            source_path
                .parent()
                .expect("source path should have a parent"),
        )
        .unwrap();
        fs::write(&source_path, vec![0xA5_u8; 2048]).unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let prepared = storage
            .prepare_outgoing_attachment(
                "large-attachment-message-1",
                &dialog_key_from_instance_id("inst-bob"),
                source_path
                    .to_str()
                    .expect("source path should be valid UTF-8"),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");
        assert!(!prepared.local_blob_managed);
        assert_eq!(
            prepared.local_blob_path.as_deref(),
            Some(source_path.as_path())
        );

        storage
            .revoke_local_attachment_access("large-attachment-message-1", 1_020)
            .expect("external source serving should be revoked");
        assert!(
            source_path.exists(),
            "revoking access must not delete the user's original file"
        );
    }

    #[test]
    fn large_outgoing_attachment_source_change_blocks_serving() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("large-attachment-source-change-{}.db", unique_suffix()));
        let source_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("large-source-change-{}.bin", unique_suffix()));

        fs::create_dir_all(source_path.parent().expect("source path should have a parent")).unwrap();
        fs::write(&source_path, vec![0xA5_u8; 2048]).unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        storage
            .prepare_outgoing_attachment(
                "large-attachment-source-change-1",
                &dialog_key_from_instance_id("inst-bob"),
                source_path.to_str().expect("source path should be valid UTF-8"),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");

        fs::write(&source_path, vec![0x5A_u8; 4096]).unwrap();
        let error = storage
            .attachment_serving_blob_path("large-attachment-source-change-1")
            .expect_err("changed source file should not be served");
        assert!(error.to_string().contains("changed after it was offered"));
    }

    #[test]
    fn outgoing_attachment_preserves_explicit_display_file_name() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-display-name-{}.db", unique_suffix()));
        let source_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("display-name-source-{}.txt", unique_suffix()));

        fs::create_dir_all(
            source_path
                .parent()
                .expect("source path should have a parent"),
        )
        .unwrap();
        fs::write(&source_path, "omni attachment with explicit display name").unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let prepared = storage
            .prepare_outgoing_attachment(
                "attachment-message-display-name-1",
                &dialog_key_from_instance_id("inst-bob"),
                source_path
                    .to_str()
                    .expect("source path should be valid UTF-8"),
                Some("Quarterly report.txt"),
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");

        assert_eq!(prepared.attachment.file_name, "Quarterly report.txt");

        let runtime = storage
            .get_attachment_runtime_info("attachment-message-display-name-1")
            .expect("runtime info should load")
            .expect("runtime info should exist");
        assert_eq!(runtime.attachment.file_name, "Quarterly report.txt");
    }

    #[test]
    fn an_attachment_offer_without_end_to_end_encryption_is_refused() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("plain-offer-{}.db", unique_suffix()));
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let dialog = dialog_key_from_instance_id("inst-plain");
        for (mode, key, seed) in [
            (Some("transport_only_v1"), None, None),
            (Some("transport_only"), None, None),
            (None, None, None),
            (Some("chacha20_poly1305_v1"), None, None),
        ] {
            assert!(storage
                .store_incoming_attachment_offer(
                    "m-plain", &dialog, "a.txt", 10, Some("text/plain"), "sha", mode, key, seed, Some(16_384), 1_010,
                )
                .is_err(), "{mode:?}");
        }
        assert!(storage
            .store_incoming_attachment_offer(
                "m-sealed", &dialog, "a.txt", 10, Some("text/plain"), "sha",
                Some("chacha20_poly1305_v1"), Some(TEST_MEDIA_KEY_HEX), Some(TEST_MEDIA_NONCE_SEED_HEX), Some(16_384), 1_010,
            )
            .is_ok());
    }

    #[test]
    fn dialogs_with_pending_attachment_downloads_lists_every_chat_waiting_to_ask() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("pending-download-dialogs-{}.db", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("pending-download-save-{}.bin", unique_suffix()));
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let asked = dialog_key_from_instance_id("inst-asked");
        let idle = dialog_key_from_instance_id("inst-idle");
        for (message_id, dialog_key) in [("m-asked", &asked), ("m-idle", &idle)] {
            storage
                .store_incoming_attachment_offer(
                    message_id,
                    dialog_key,
                    "report.txt",
                    10,
                    Some("text/plain"),
                    "fbb550db14f7405255da5d0b80de9ac6ba0bfbe8e638d736cf6ef9127dd34139",
                    Some("chacha20_poly1305_v1"),
                    Some(TEST_MEDIA_KEY_HEX),
                    Some(TEST_MEDIA_NONCE_SEED_HEX),
                    Some(131_072),
                    1_010,
                )
                .unwrap();
        }
        // An offer nobody asked for yet is not waiting to ask.
        assert!(storage
            .dialogs_with_pending_attachment_downloads()
            .unwrap()
            .is_empty());

        storage
            .mark_attachment_download_requested("m-asked", &save_path, "transfer-asked", 1_020)
            .unwrap();
        assert_eq!(
            storage.dialogs_with_pending_attachment_downloads().unwrap(),
            vec![asked]
        );
    }

    #[test]
    fn incoming_attachment_download_saves_only_user_copy_without_managed_blob() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!(
                "incoming-attachment-download-{}.db",
                unique_suffix()
            ));
        let requested_save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("incoming-save-{}.txt", unique_suffix()));
        let second_export_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("incoming-export-{}.txt", unique_suffix()));

        fs::create_dir_all(db_path.parent().expect("db path should have a parent")).unwrap();
        fs::create_dir_all(
            requested_save_path
                .parent()
                .expect("requested save path should have a parent"),
        )
        .unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        storage
            .store_incoming_attachment_offer(
                "attachment-message-incoming-1",
                &dialog_key_from_instance_id("inst-bob"),
                "report.txt",
                "omni attachment incoming copy".len() as i64,
                Some("text/plain"),
                &content_id_hex_of(b"omni attachment incoming copy"),
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(131_072),
                1_010,
            )
            .expect("incoming attachment should be stored");
        storage
            .mark_attachment_download_requested(
                "attachment-message-incoming-1",
                &requested_save_path,
                "transfer-incoming-1",
                1_020,
            )
            .expect("download should be requested");

        let temp_download_path = storage
            .attachment_download_temp_path("attachment-message-incoming-1")
            .expect("temp path should load")
            .expect("temp path should exist in runtime");
        assert_eq!(temp_download_path.parent(), requested_save_path.parent());
        fs::write(&temp_download_path, "omni attachment incoming copy").unwrap();

        let downloaded = storage
            .complete_attachment_download_from_temp("attachment-message-incoming-1", 1_030)
            .expect("download should complete");
        assert!(!temp_download_path.exists());
        assert!(downloaded.attachment.local_blob_available);
        assert!(!downloaded.attachment.local_serving_enabled);
        assert_eq!(
            downloaded.attachment.local_saved_path.as_deref(),
            Some(
                requested_save_path
                    .to_str()
                    .expect("requested save path should be valid UTF-8")
            )
        );
        assert!(
            downloaded.local_blob_path.is_none(),
            "incoming downloads should not keep a managed blob"
        );
        assert_eq!(
            storage
                .attachment_local_blob_path("attachment-message-incoming-1")
                .expect("blob path should load"),
            None
        );
        assert_eq!(
            fs::read_to_string(&requested_save_path).unwrap(),
            "omni attachment incoming copy"
        );

        let second_saved = storage
            .save_attachment_copy_to_path(
                "attachment-message-incoming-1",
                &second_export_path,
                1_040,
            )
            .expect("saved local copy should be reusable without a managed blob");
        assert!(second_saved.attachment.local_blob_available);
        assert!(!second_saved.attachment.local_serving_enabled);
        assert_eq!(
            fs::read_to_string(&second_export_path).unwrap(),
            "omni attachment incoming copy"
        );
    }

    /// A download is a map of 16 MiB blocks (stage 3, attachment-content-tree.md), not a prefix:
    /// blocks land in any order, the request resumes at the first one not here, progress counts the
    /// blocks here — and only those the temp file still holds whole.
    #[test]
    fn a_download_resumes_at_its_first_block_not_here_as_its_temp_file_allows() {
        let root = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("block-map-{}", unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        let storage = Storage::open(root.join("omni.db")).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES;
        let size = 3 * block + 5; // four blocks, the last of five bytes
        storage
            .store_incoming_attachment_offer(
                "m-map",
                &dialog_key_from_instance_id("inst-bob"),
                "film.mkv",
                size as i64,
                None,
                &"ab".repeat(32),
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(1024 * 1024),
                1_010,
            )
            .unwrap();
        let fresh = storage.mark_attachment_download_requested("m-map", &root.join("film.mkv"), "t-1", 1_020).unwrap();
        assert_eq!(fresh.attachment.bytes_transferred, 0);
        let temp = storage.attachment_download_temp_path("m-map").unwrap().unwrap();
        let resume = |now| storage.attachment_download_resume("m-map", now).unwrap();
        assert_eq!(resume(1_021).1, 0);

        // Blocks 0, 1 and 3 here, block 2 not: resume at block 2, progress = three blocks.
        fs::OpenOptions::new().create(true).truncate(false).write(true).open(&temp).unwrap().set_len(size).unwrap();
        storage.mark_attachment_blocks_received("m-map", &[(3, [3; 32]), (0, [7; 32]), (1, [1; 32])]).unwrap();
        let (runtime, at) = resume(1_030);
        assert_eq!(at, (2 * block) as i64);
        assert_eq!(runtime.attachment.bytes_transferred, (2 * block + 5) as i64);
        assert_eq!(storage.attachment_download_resume_offset("m-map").unwrap(), (2 * block) as i64);

        // The file holds less than a block of the map (cut short by a crash, by a user): that block
        // is not here after all.
        fs::OpenOptions::new().write(true).open(&temp).unwrap().set_len(2 * block).unwrap();
        let (runtime, at) = resume(1_040);
        assert_eq!((at, runtime.attachment.bytes_transferred), ((2 * block) as i64, (2 * block) as i64));
        assert_eq!(storage.attachment_received_blocks("m-map").unwrap().have, vec![true, true, false, false]);

        // Every block here: nothing to resume, the whole file counted.
        fs::OpenOptions::new().write(true).open(&temp).unwrap().set_len(size).unwrap();
        storage.mark_attachment_blocks_received("m-map", &[(2, [2; 32]), (3, [3; 32])]).unwrap();
        let (runtime, at) = resume(1_050);
        assert_eq!((at, runtime.attachment.bytes_transferred), (size as i64, size as i64));
        // The hashes are not this file's tree: not complete, whatever the map says.
        assert!(storage.received_content_tree("m-map").unwrap().is_none());

        // The temp file gone: nothing is here.
        fs::remove_file(&temp).unwrap();
        let (runtime, at) = resume(1_060);
        assert_eq!((at, runtime.attachment.bytes_transferred), (0, 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn attachment_download_required_space_accounts_for_resume_offset() {
        assert_eq!(super::attachment_download_required_space_bytes(100, 0), 100);
        assert_eq!(super::attachment_download_required_space_bytes(100, 40), 60);
        assert_eq!(super::attachment_download_required_space_bytes(100, 150), 0);
        assert_eq!(super::attachment_download_required_space_bytes(-1, 0), 0);
        assert_eq!(
            super::attachment_download_required_space_bytes(100, -1),
            100
        );
    }

    #[cfg(windows)]
    #[test]
    fn attachment_download_preallocation_does_not_advance_temp_file_len() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-preallocation-{}.db", unique_suffix()));
        let requested_save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("preallocation-save-{}.bin", unique_suffix()));

        fs::create_dir_all(requested_save_path.parent().expect("save parent should exist")).unwrap();
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        storage
            .store_incoming_attachment_offer(
                "attachment-preallocation-1",
                &dialog_key_from_instance_id("inst-bob"),
                "preallocation.bin",
                16 * 1024,
                None,
                "306a5a00c1a4793dca5e7e382d9ef7f04e61bf927d051c5f71b5d013f7437a8c",
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(1024),
                1_010,
            )
            .expect("incoming attachment should be stored");
        storage
            .mark_attachment_download_requested(
                "attachment-preallocation-1",
                &requested_save_path,
                "transfer-preallocation-1",
                1_020,
            )
            .expect("download should be requested");
        let temp_download_path = storage
            .attachment_download_temp_path("attachment-preallocation-1")
            .expect("temp path should load")
            .expect("temp path should be set");
        assert_eq!(fs::metadata(&temp_download_path).unwrap().len(), 0);
    }

    #[test]
    fn failed_download_verification_marks_attachment_failed() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-verify-failed-{}.db", unique_suffix()));
        let requested_save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("verify-failed-save-{}.bin", unique_suffix()));

        fs::create_dir_all(requested_save_path.parent().expect("save parent should exist")).unwrap();
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        storage
            .store_incoming_attachment_offer(
                "attachment-verify-failed-1",
                &dialog_key_from_instance_id("inst-bob"),
                "verify-failed.bin",
                4,
                None,
                "fbb550db14f7405255da5d0b80de9ac6ba0bfbe8e638d736cf6ef9127dd34139",
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(1024),
                1_010,
            )
            .expect("incoming attachment should be stored");
        storage
            .mark_attachment_download_requested(
                "attachment-verify-failed-1",
                &requested_save_path,
                "transfer-verify-failed-1",
                1_020,
            )
            .expect("download should be requested");
        let temp_download_path = storage
            .attachment_download_temp_path("attachment-verify-failed-1")
            .expect("temp path should load")
            .expect("temp path should be set");
        fs::write(&temp_download_path, b"xxxx").unwrap();

        storage
            .complete_attachment_download_from_temp("attachment-verify-failed-1", 1_030)
            .expect_err("hash mismatch should fail verification");
        let runtime = storage
            .get_attachment_runtime_info("attachment-verify-failed-1")
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(
            runtime.attachment.state,
            omni_types::AttachmentTransferState::Failed
        );
        assert!(runtime.active_transfer_id.is_none());
        assert!(runtime
            .attachment
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("Проверка файла не прошла"));
    }

    #[test]
    fn the_same_offer_delivered_again_does_not_set_its_download_back() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-offer-again-{}.db", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("offer-again-save-{}.bin", unique_suffix()));
        fs::create_dir_all(save_path.parent().unwrap()).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let offer = |at: i64| {
            storage.store_incoming_attachment_offer(
                "attachment-offer-again-1",
                &dialog_key_from_instance_id("inst-bob"),
                "offer-again.bin",
                16 * 1024,
                None,
                "306a5a00c1a4793dca5e7e382d9ef7f04e61bf927d051c5f71b5d013f7437a8c",
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(1024),
                at,
            )
        };
        offer(1_010).unwrap();
        storage.mark_attachment_download_requested("attachment-offer-again-1", &save_path, "transfer-again-1", 1_020).unwrap();
        storage.mark_attachment_downloading("attachment-offer-again-1", 4096, 1_030).unwrap();

        // The sender's catch-up after a break hands the same offer over once more.
        let again = offer(1_010).unwrap();
        assert_eq!(again.attachment.state, omni_types::AttachmentTransferState::Downloading);
        assert_eq!(again.attachment.bytes_transferred, 4096);
        assert_eq!(again.active_transfer_id.as_deref(), Some("transfer-again-1"));
    }

    /// The queue watches the stall of every 1:1 download under way, not only the active dialog's:
    /// those are the dialogs it walks (a group's file has its own coordinator).
    #[test]
    fn the_dialogs_with_a_download_under_way_are_the_one_to_one_ones_downloading() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-downloading-dialogs-{}.db", unique_suffix()));
        let save_dir = std::env::temp_dir().join("omni-stage-1-storage-tests");
        fs::create_dir_all(&save_dir).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let bob = dialog_key_from_instance_id("inst-bob");
        let carol = dialog_key_from_instance_id("inst-carol");
        for (id, dialog) in [("from-bob", bob.as_str()), ("from-carol", carol.as_str()), ("in-group", "group:g1")] {
            storage
                .store_incoming_attachment_offer(
                    id,
                    dialog,
                    "file.bin",
                    16 * 1024,
                    None,
                    "306a5a00c1a4793dca5e7e382d9ef7f04e61bf927d051c5f71b5d013f7437a8c",
                    Some("chacha20_poly1305_v1"),
                    Some(TEST_MEDIA_KEY_HEX),
                    Some(TEST_MEDIA_NONCE_SEED_HEX),
                    Some(1024),
                    1_010,
                )
                .unwrap();
        }
        assert!(storage.dialogs_with_downloading_attachments().unwrap().is_empty(), "offered is not under way");
        for id in ["from-bob", "in-group"] {
            let save = save_dir.join(format!("{id}-{}.bin", unique_suffix()));
            storage.mark_attachment_download_requested(id, &save, &format!("transfer-{id}"), 1_020).unwrap();
            storage.mark_attachment_downloading(id, 4096, 1_030).unwrap();
        }
        assert_eq!(storage.dialogs_with_downloading_attachments().unwrap(), vec![bob]);
    }

    #[test]
    fn pausing_inflight_attachment_download_counts_only_the_blocks_in_its_map() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-pause-resume-{}.db", unique_suffix()));
        let requested_save_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("pause-resume-save-{}.bin", unique_suffix()));

        fs::create_dir_all(
            requested_save_path
                .parent()
                .expect("save parent should exist"),
        )
        .unwrap();
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        storage
            .store_incoming_attachment_offer(
                "attachment-pause-resume-1",
                &dialog_key_from_instance_id("inst-bob"),
                "pause-resume.bin",
                16 * 1024,
                None,
                "306a5a00c1a4793dca5e7e382d9ef7f04e61bf927d051c5f71b5d013f7437a8c",
                Some("chacha20_poly1305_v1"),
                Some(TEST_MEDIA_KEY_HEX),
                Some(TEST_MEDIA_NONCE_SEED_HEX),
                Some(1024),
                1_010,
            )
            .expect("incoming attachment should be stored");
        storage
            .mark_attachment_download_requested(
                "attachment-pause-resume-1",
                &requested_save_path,
                "transfer-pause-resume-1",
                1_020,
            )
            .expect("download should be requested");
        storage
            .mark_attachment_downloading("attachment-pause-resume-1", 0, 1_030)
            .expect("download should be marked active");
        let temp_download_path = storage
            .attachment_download_temp_path("attachment-pause-resume-1")
            .expect("temp path should load")
            .expect("temp path should be set");
        fs::write(&temp_download_path, vec![0x91_u8; 3 * 1024 + 777]).unwrap();
        // Leaves written, no block closed: progress shown, nothing in the map yet.
        storage
            .mark_attachment_downloading("attachment-pause-resume-1", 3 * 1024 + 777, 1_035)
            .unwrap();

        storage
            .pause_inflight_attachment_downloads(1_040, &|_| false)
            .expect("pause should persist durable resume offset");
        let paused = storage
            .get_attachment_runtime_info("attachment-pause-resume-1")
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(
            paused.attachment.state,
            omni_types::AttachmentTransferState::WaitingForReconnect
        );
        // As far as it is durable: the blocks in the map (none — the block is not closed).
        assert_eq!(paused.attachment.bytes_transferred, 0);
        // A pause runs beside a stream that may still be writing: it never cuts the file.
        assert_eq!(fs::metadata(&temp_download_path).unwrap().len(), 3 * 1024 + 777);
    }

    #[test]
    fn dialog_refresh_summary_tracks_attachment_only_updates() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-summary-{}.db", unique_suffix()));
        let source_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("attachment-summary-source-{}.txt", unique_suffix()));
        let dialog_key = dialog_key_from_instance_id("inst-bob");

        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        fs::write(&source_path, "omni attachment refresh summary").unwrap();

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        storage
            .prepare_outgoing_attachment(
                "attachment-message-2",
                &dialog_key,
                source_path
                    .to_str()
                    .expect("source path should be valid UTF-8"),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_010,
            )
            .expect("attachment should be prepared");

        let initial_summary = storage
            .get_dialog_message_refresh_summary(&dialog_key)
            .expect("initial summary should load");
        assert_eq!(initial_summary.message_count, 0);
        assert!(initial_summary.latest_message_id.is_none());
        assert!(initial_summary.last_message_updated_at_unix_ms.is_none());
        assert_eq!(
            initial_summary.last_attachment_updated_at_unix_ms,
            Some(1_010)
        );

        storage
            .revoke_local_attachment_access("attachment-message-2", 1_040)
            .expect("attachment access should be revoked");

        let updated_summary = storage
            .get_dialog_message_refresh_summary(&dialog_key)
            .expect("updated summary should load");
        assert_eq!(updated_summary.message_count, 0);
        assert!(updated_summary.latest_message_id.is_none());
        assert!(updated_summary.last_message_updated_at_unix_ms.is_none());
        assert_eq!(
            updated_summary.last_attachment_updated_at_unix_ms,
            Some(1_040)
        );
        assert_ne!(
            initial_summary.last_attachment_updated_at_unix_ms,
            updated_summary.last_attachment_updated_at_unix_ms
        );
    }

    #[test]
    fn dialog_delta_queries_use_updated_at_and_message_id_tie_breakers() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("dialog-delta-{}.db", unique_suffix()));
        let dialog_key = dialog_key_from_instance_id("inst-delta");

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        storage
            .insert_outgoing_message(
                "delta-msg-1",
                &dialog_key,
                "first",
                MessageStatus::Queued,
                1_100,
            )
            .expect("first message should be inserted");
        storage
            .upsert_message_security_metadata(
                "delta-msg-1",
                &dialog_key,
                None,
                None,
                None,
                None,
                "encrypted_pending",
                None,
                Some("text"),
                1_100,
                1_100,
            )
            .expect("first security details should be inserted");

        storage
            .insert_outgoing_message(
                "delta-msg-2",
                &dialog_key,
                "second",
                MessageStatus::Queued,
                1_100,
            )
            .expect("second message should be inserted");
        storage
            .upsert_message_security_metadata(
                "delta-msg-2",
                &dialog_key,
                None,
                None,
                None,
                None,
                "encrypted_pending",
                None,
                Some("text"),
                1_100,
                1_100,
            )
            .expect("second security details should be inserted");

        let changed_messages = storage
            .list_messages_for_dialog_after_cursor(
                &dialog_key,
                Some(1_100),
                Some("delta-msg-1"),
                None,
                None,
            )
            .expect("delta messages should load");
        assert_eq!(changed_messages.len(), 1);
        assert_eq!(changed_messages[0].id, "delta-msg-2");

        let changed_security = storage
            .get_message_security_details_delta(&dialog_key, Some(1_100), Some("delta-msg-1"))
            .expect("delta security details should load");
        assert_eq!(changed_security.len(), 1);
        assert_eq!(changed_security[0].message_id, "delta-msg-2");
    }

    #[test]
    fn full_dialog_scope_rebind_moves_history_queue_receipts_attachments_and_security_metadata() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("dialog-rebind-full-{}.db", unique_suffix()));
        let attachment_path = db_path.with_file_name(format!(
            "dialog-rebind-full-attachment-{}.bin",
            unique_suffix()
        ));
        let legacy_dialog_key = omni_types::dialog_key_from_account_id("acct-bob");
        let stable_dialog_key = dialog_key_from_instance_id("inst-bob");

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: Some(legacy_dialog_key.clone()),
                    initial_observation: None,
                },
                1_050,
            )
            .expect("contact should be created");

        storage
            .insert_outgoing_message(
                "rebind-outgoing-1",
                &legacy_dialog_key,
                "queued before bind",
                MessageStatus::Queued,
                1_100,
            )
            .expect("outgoing message should be inserted");
        storage
            .insert_incoming_message(
                "rebind-incoming-1",
                &legacy_dialog_key,
                "incoming before bind",
                1_110,
            )
            .expect("incoming message should be inserted");
        fs::write(&attachment_path, b"attachment-bytes").expect("attachment source should exist");
        storage
            .prepare_outgoing_attachment(
                "rebind-outgoing-1",
                &legacy_dialog_key,
                attachment_path
                    .to_str()
                    .expect("attachment path should be valid UTF-8"),
                Some("evidence.bin"),
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_120,
            )
            .expect("attachment metadata should be created");
        storage
            .upsert_message_security_metadata(
                "rebind-outgoing-1",
                &legacy_dialog_key,
                Some(&contact.contact_id),
                None,
                contact.remote_account_id.as_ref(),
                Some(&DeviceId("dev-bob-1".to_string())),
                MessageEncryptionState::EncryptedDelivered.as_db_value(),
                None,
                Some(SecurityPayloadKind::Text.as_db_value()),
                1_100,
                1_125,
            )
            .expect("security metadata should be created");
        storage
            .update_message_status("rebind-outgoing-1", MessageStatus::Delivered, 1_130)
            .expect("delivery receipt should be recorded");

        storage
            .rebind_dialog_scope(&legacy_dialog_key, &stable_dialog_key, 1_200)
            .expect("full dialog scope should rebind");
        storage
            .rebind_dialog_scope(&legacy_dialog_key, &stable_dialog_key, 1_210)
            .expect("second rebind should stay idempotent");

        let moved_messages = storage
            .list_messages_for_dialog(&stable_dialog_key)
            .expect("stable dialog history should load");
        assert_eq!(moved_messages.len(), 2);
        assert_eq!(moved_messages[0].id, "rebind-outgoing-1");
        assert_eq!(moved_messages[1].id, "rebind-incoming-1");
        assert!(storage
            .list_messages_for_dialog(&legacy_dialog_key)
            .expect("legacy dialog history should load")
            .is_empty());
        assert_eq!(
            storage
                .get_dialog_message_refresh_summary(&stable_dialog_key)
                .expect("refresh summary should load")
                .message_count,
            2
        );
        assert_eq!(
            storage
                .outbound_queue_snapshot()
                .expect("queue snapshot should load")
                .items[0]
                .dialog_key,
            stable_dialog_key
        );
        assert_eq!(
            storage
                .connection
                .query_row(
                    "SELECT dialog_key FROM inbound_dedup WHERE message_id = ?1 AND profile_id = ?2",
                    rusqlite::params!["rebind-incoming-1", profile_id.0],
                    |row| row.get::<_, String>(0),
                )
                .expect("inbound dedup row should exist"),
            stable_dialog_key
        );
        assert_eq!(
            storage
                .connection
                .query_row(
                    "SELECT dialog_key FROM delivery_receipts WHERE message_id = ?1 AND profile_id = ?2",
                    rusqlite::params!["rebind-outgoing-1", profile_id.0],
                    |row| row.get::<_, String>(0),
                )
                .expect("delivery receipt should exist"),
            stable_dialog_key
        );
        assert_eq!(
            storage
                .connection
                .query_row(
                    "SELECT dialog_key FROM message_attachments WHERE message_id = ?1 AND profile_id = ?2",
                    rusqlite::params!["rebind-outgoing-1", profile_id.0],
                    |row| row.get::<_, String>(0),
                )
                .expect("attachment row should exist"),
            stable_dialog_key
        );
        assert_eq!(
            storage
                .connection
                .query_row(
                    "SELECT dialog_key FROM message_security_metadata WHERE message_id = ?1 AND profile_id = ?2",
                    rusqlite::params!["rebind-outgoing-1", profile_id.0],
                    |row| row.get::<_, String>(0),
                )
                .expect("security metadata row should exist"),
            stable_dialog_key
        );
    }

    #[test]
    fn contact_revisions_change_after_mutations() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-6-storage-tests")
            .join(format!("detail-revisions-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("contact should be added");
        let initial_contact_revision = storage
            .get_contact_card_revision(&contact.contact_id)
            .expect("contact revision should load")
            .expect("contact revision should exist")
            .revision;
        let stable_contact_revision = storage
            .get_contact_card_revision(&contact.contact_id)
            .expect("stable contact revision should load")
            .expect("stable contact revision should exist")
            .revision;
        assert_eq!(stable_contact_revision, initial_contact_revision);

        storage
            .rename_contact(&contact.contact_id, "Bobby", 1_150)
            .expect("contact rename should succeed");
        let renamed_contact_revision = storage
            .get_contact_card_revision(&contact.contact_id)
            .expect("renamed contact revision should load")
            .expect("renamed contact revision should exist")
            .revision;
        assert!(renamed_contact_revision > initial_contact_revision);
    }

    #[test]
    fn first_presence_sent_upsert_uses_safe_defaults() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-11-storage-tests")
            .join(format!("presence-defaults-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("contact should be added");

        storage
            .record_contact_presence_sent(&contact.contact_id, 1_200)
            .expect("first presence heartbeat should create a row without null violations");

        let refreshed = storage
            .get_contact(&contact.contact_id)
            .expect("contact should reload");
        assert_eq!(
            refreshed.presence_state,
            omni_identity::ContactPresenceState::Offline
        );
        assert_eq!(refreshed.presence_last_sent_at_unix_ms, Some(1_200));
    }

    #[test]
    fn record_contact_presence_recently_sets_recent_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-11-storage-tests")
            .join(format!("presence-recently-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("contact should be added");

        storage
            .record_contact_presence_activity(&contact.contact_id, 1_200)
            .expect("presence activity should succeed");
        storage
            .record_contact_presence_recently(&contact.contact_id, 1_250)
            .expect("presence should move to recently");

        let refreshed = storage
            .get_contact(&contact.contact_id)
            .expect("contact should reload");
        assert_eq!(
            refreshed.presence_state,
            omni_identity::ContactPresenceState::Recently
        );
    }

    #[test]
    fn database_backup_export_creates_valid_copy() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("source-{}.db", unique_suffix()));
        let backup_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("backup-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let mnemonic = account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();

        storage
            .export_database_backup(&backup_path)
            .expect("backup export should succeed");

        assert!(backup_path.exists());

        let backup_preview = Storage::inspect_account_backup_manifest(&backup_path, &mnemonic)
            .expect("backup should validate with the mnemonic");
        assert_eq!(backup_preview.account_id, account.descriptor.account_id);
        assert_eq!(
            backup_preview.profile_id,
            account
                .local_profile_summary
                .as_ref()
                .expect("local profile summary should exist")
                .profile_id
        );

        let backup_storage = Storage::open(&backup_path).expect("backup should open");
        backup_storage
            .migrate()
            .expect("backup migration should succeed");
        let backup_account = backup_storage
            .get_local_account()
            .expect("backup account should load")
            .expect("backup account should exist");
        assert_eq!(
            backup_account.descriptor.account_id,
            account.descriptor.account_id
        );
        assert_eq!(
            backup_account.descriptor.display_name,
            account.descriptor.display_name
        );
    }

    #[test]
    fn duplicate_incoming_message_id_is_ignored() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        let first = storage
            .insert_incoming_message(
                "incoming-dup",
                &dialog_key_from_instance_id("peer-dup"),
                "hello once",
                100,
            )
            .expect("first insert should succeed");
        let second = storage
            .insert_incoming_message(
                "incoming-dup",
                &dialog_key_from_instance_id("peer-dup"),
                "hello twice",
                101,
            )
            .expect("duplicate insert should resolve to existing message");

        assert_eq!(first.id, second.id);
        assert_eq!(second.body, "hello once");
        assert_eq!(second.created_at_unix_ms, 100);

        let messages = storage
            .list_messages_for_dialog(&dialog_key_from_instance_id("peer-dup"))
            .expect("messages should be listed by dialog key");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "incoming-dup");
    }

    #[test]
    fn inbound_dedup_foundation_tracks_message_id_once() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");

        storage
            .insert_incoming_message(
                "incoming-dedup-1",
                &dialog_key_from_instance_id("peer-dedup"),
                "hello once",
                100,
            )
            .unwrap();
        storage
            .insert_incoming_message(
                "incoming-dedup-1",
                &dialog_key_from_instance_id("peer-dedup"),
                "hello twice",
                101,
            )
            .unwrap();

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        let dedup_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM inbound_dedup WHERE message_id = 'incoming-dedup-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dedup_count, 1);
    }

    #[test]
    fn unsupported_legacy_history_database_is_reported() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO messages (id, peer_addr, body, status, created_at_unix_ms, direction, updated_at_unix_ms)
                VALUES ('legacy-out-1', '127.0.0.1', 'legacy sent', 'delivered', 10, 'outgoing', 10);
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        let summary = storage.inspect_database_bootstrap_summary().unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            DatabaseBootstrapStateKind::UnsupportedDatabaseFormat
        );
        assert!(summary.reset_required);
        assert!(summary.can_create_new_account);
        assert!(summary.can_restore_from_mnemonic);
    }

    #[test]
    fn queue_schema_survives_restart() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        storage
            .insert_outgoing_message(
                "queued-1",
                &dialog_key_from_instance_id("peer-queue"),
                "queued body",
                MessageStatus::Queued,
                100,
            )
            .unwrap();
        drop(storage);

        let reopened = Storage::open(&db_path).unwrap();
        reopened.migrate().unwrap();
        let queue = reopened.outbound_queue_snapshot().unwrap();
        assert_eq!(queue.queued_count, 1);
        assert_eq!(queue.items.len(), 1);
        assert_eq!(
            queue.items[0].dialog_key,
            dialog_key_from_instance_id("peer-queue")
        );
    }

    #[test]
    fn attempt_count_accumulates_across_sending_sent_retrying_and_delivery() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        storage
            .insert_outgoing_message(
                "attempt-msg",
                &dialog_key_from_instance_id("peer-attempt"),
                "body",
                MessageStatus::Queued,
                100,
            )
            .unwrap();

        storage
            .mark_queue_attempt_started("attempt-msg", 110)
            .unwrap();
        assert_eq!(
            storage
                .queue_attempt_count_for_message("attempt-msg")
                .unwrap(),
            1
        );

        storage
            .update_message_status("attempt-msg", MessageStatus::Sent, 111)
            .unwrap();
        assert_eq!(
            storage
                .queue_attempt_count_for_message("attempt-msg")
                .unwrap(),
            1
        );

        storage
            .mark_queue_retry(
                "attempt-msg",
                omni_types::QueueFailureKind::AckTimeout,
                "retry",
                200,
                150,
            )
            .unwrap();
        assert_eq!(
            storage
                .queue_attempt_count_for_message("attempt-msg")
                .unwrap(),
            1
        );

        storage
            .mark_queue_attempt_started("attempt-msg", 210)
            .unwrap();
        assert_eq!(
            storage
                .queue_attempt_count_for_message("attempt-msg")
                .unwrap(),
            2
        );

        storage
            .record_delivery_receipt(
                "attempt-msg",
                &dialog_key_from_instance_id("peer-attempt"),
                220,
                221,
            )
            .unwrap();

        let queue = storage.outbound_queue_snapshot().unwrap();
        assert!(queue
            .items
            .iter()
            .all(|item| item.message_id != "attempt-msg"));
        let receipt_profile_id: Option<String> = storage
            .connection
            .query_row(
                "SELECT profile_id FROM delivery_receipts WHERE message_id = ?1",
                rusqlite::params!["attempt-msg"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipt_profile_id.as_deref(), Some(profile_id.0.as_str()));
        let message = storage.get_message("attempt-msg").unwrap();
        assert_eq!(message.status, MessageStatus::Delivered);
        assert_eq!(message.remote_delivered_at_unix_ms, Some(220));
    }

    #[test]
    fn dialogs_with_due_pending_queue_lists_every_chat_with_a_waiting_message() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("due-pending-dialogs-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let first = dialog_key_from_instance_id("peer-first");
        let second = dialog_key_from_instance_id("peer-second");
        let sent = dialog_key_from_instance_id("peer-sent");
        for (id, dialog, status) in [
            ("m-first", &first, MessageStatus::Queued),
            ("m-first-2", &first, MessageStatus::Queued),
            ("m-second", &second, MessageStatus::Queued),
            ("m-sent", &sent, MessageStatus::Sent),
        ] {
            storage
                .insert_outgoing_message(id, dialog, "hi", status, 1_100)
                .unwrap();
        }

        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(
            storage.dialogs_with_due_pending_queue(i64::MAX).unwrap(),
            expected
        );
    }

    #[test]
    fn pending_queue_for_dialog_is_profile_scoped() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("pending-queue-profile-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let shared_dialog_key = dialog_key_from_instance_id("peer-shared");

        let alice = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let alice_profile_id = alice
            .local_profile_summary
            .as_ref()
            .unwrap()
            .profile_id
            .clone();
        storage
            .insert_outgoing_message(
                "alice-queued",
                &shared_dialog_key,
                "alice",
                MessageStatus::Queued,
                1_100,
            )
            .unwrap();

        let bob = storage
            .create_local_account("Bob", "Laptop", 2_000)
            .unwrap();
        let bob_profile_id = bob
            .local_profile_summary
            .as_ref()
            .unwrap()
            .profile_id
            .clone();
        storage
            .insert_outgoing_message(
                "bob-queued",
                &shared_dialog_key,
                "bob",
                MessageStatus::Queued,
                2_100,
            )
            .unwrap();

        storage
            .set_active_profile_id(Some(&alice_profile_id))
            .unwrap();
        let alice_items = storage
            .pending_queue_for_dialog(&shared_dialog_key, i64::MAX)
            .unwrap();
        assert_eq!(alice_items.len(), 1);
        assert_eq!(alice_items[0].message_id, "alice-queued");

        storage
            .set_active_profile_id(Some(&bob_profile_id))
            .unwrap();
        let bob_items = storage
            .pending_queue_for_dialog(&shared_dialog_key, i64::MAX)
            .unwrap();
        assert_eq!(bob_items.len(), 1);
        assert_eq!(bob_items[0].message_id, "bob-queued");
    }

    #[test]
    fn create_local_account_persists_account_and_current_device() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();

        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .expect("account should be created");
        let device = storage
            .get_local_device()
            .expect("device snapshot should load")
            .expect("current device should exist");

        assert_eq!(account.descriptor.display_name, "Alice");
        assert_eq!(device.device_label, "Office PC");
        assert!(device.is_current);
        assert_eq!(account.current_device_id, device.device_id);
        assert!(!account.restored_from_recovery);
    }

    #[test]
    fn local_profile_list_and_summary_baseline_are_available() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("profiles-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let list = storage.list_local_profiles().unwrap();
        assert_eq!(list.profiles.len(), 1);
        let profile = &list.profiles[0];
        assert_eq!(profile.state, LocalProfileState::SignedIn);

        let loaded = storage
            .get_local_profile_summary(&profile.profile_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.profile_id, profile.profile_id);
        assert_eq!(loaded.account_id, profile.account_id);
    }

    #[test]
    fn ownership_map_and_delete_routing_are_canonical() {
        assert_eq!(
            ownership_category_for_table("local_account"),
            Some(StorageOwnershipCategory::AccountDeviceScoped)
        );
        assert_eq!(
            ownership_category_for_table("profile_registry"),
            Some(StorageOwnershipCategory::ProfileOwned)
        );
        assert_eq!(
            ownership_category_for_table("local_devices"),
            Some(StorageOwnershipCategory::DeviceScoped)
        );
        assert_eq!(
            ownership_category_for_table("message_attachments"),
            Some(StorageOwnershipCategory::ProfileOwned)
        );
        assert!(profile_owned_tables().contains(&"profile_registry"));
        assert!(profile_owned_tables().contains(&"messages"));
        assert!(profile_owned_tables().contains(&"message_attachments"));
        assert!(!account_device_scoped_tables().contains(&"messages"));
        assert!(device_scoped_tables().contains(&"local_devices"));
        assert!(global_install_scoped_tables().contains(&"app_meta"));
        assert!(!profile_owned_tables().contains(&"direct_route_hints"));
        assert!(profile_delete_tables().contains(&"profile_secrets"));
        assert!(!profile_delete_tables().contains(&"app_meta"));
        assert!(!profile_delete_tables().contains(&"remote_device_observations"));
    }

    #[test]
    fn profile_registry_foundation_tracks_active_profile_and_current_device() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("profile-foundation-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let list = storage.list_local_profiles().unwrap();
        assert_eq!(list.profiles.len(), 1);
        assert_eq!(list.profiles[0].profile_id, profile_id);
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(profile_id.clone())
        );
        assert!(storage.profile_exists(&profile_id).unwrap());
        assert_eq!(
            storage.get_profile_current_device_id(&profile_id).unwrap(),
            Some(device.device_id.clone())
        );
    }

    #[test]
    fn profile_registry_supports_multiple_saved_profiles_and_active_switching() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("multi-profile-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();

        let first = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let first_profile_id = first
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        let first_device_id = first.current_device_id.clone();

        let second = storage
            .create_local_account("Bob", "Laptop", 2_000)
            .unwrap();
        let second_profile_id = second
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        let second_device_id = second.current_device_id.clone();

        let list = storage.list_local_profiles().unwrap();
        assert_eq!(list.profiles.len(), 2);
        assert!(list
            .profiles
            .iter()
            .any(|profile| profile.profile_id == first_profile_id));
        assert!(list
            .profiles
            .iter()
            .any(|profile| profile.profile_id == second_profile_id));
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(second_profile_id.clone())
        );
        assert_eq!(
            storage
                .get_local_account()
                .unwrap()
                .unwrap()
                .descriptor
                .account_id,
            second.descriptor.account_id
        );
        assert_eq!(
            storage.get_local_device().unwrap().unwrap().device_id,
            second_device_id
        );

        storage
            .set_active_profile_id(Some(&first_profile_id))
            .unwrap();
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(first_profile_id.clone())
        );
        assert_eq!(
            storage
                .get_local_account()
                .unwrap()
                .unwrap()
                .descriptor
                .account_id,
            first.descriptor.account_id
        );
        assert_eq!(
            storage.get_local_device().unwrap().unwrap().device_id,
            first_device_id
        );
        assert_eq!(
            storage
                .get_profile_current_device_id(&first_profile_id)
                .unwrap(),
            Some(first_device_id)
        );
        assert_eq!(
            storage
                .get_profile_current_device_id(&second_profile_id)
                .unwrap(),
            Some(second_device_id)
        );
    }

    #[test]
    fn unsupported_legacy_profile_database_is_reported() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("backfill-{}.db", unique_suffix()));

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE local_account (
                    singleton INTEGER NOT NULL DEFAULT 1 UNIQUE,
                    account_id TEXT PRIMARY KEY NOT NULL,
                    display_name TEXT NOT NULL,
                    recovery_version INTEGER NOT NULL,
                    recovery_code TEXT NOT NULL,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE local_devices (
                    device_id TEXT PRIMARY KEY NOT NULL,
                    account_id TEXT NOT NULL,
                    device_label TEXT NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 0,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE UNIQUE INDEX ux_local_devices_current
                ON local_devices(is_current)
                WHERE is_current = 1;
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO local_account (
                    singleton,
                    account_id,
                    display_name,
                    recovery_version,
                    recovery_code,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (1, 'acct-legacy', 'Alice', 2, 'legacy', 0, 1000, 1000);
                INSERT INTO local_devices (
                    device_id,
                    account_id,
                    device_label,
                    is_current,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES ('dev-legacy', 'acct-legacy', 'Office PC', 1, 0, 1000, 1000);
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        let summary = storage.inspect_database_bootstrap_summary().unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            DatabaseBootstrapStateKind::UnsupportedLegacyLocalData
        );
        assert!(summary.reset_required);
        assert!(summary.can_create_new_account);
        assert!(summary.can_restore_from_mnemonic);
    }

    #[test]
    fn runtime_gate_helper_reports_not_signed_in_without_local_profile() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("runtime-gate-none-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();

        assert!(!storage.is_profile_signed_in().unwrap());
    }

    #[test]
    fn restore_local_account_from_mnemonic_supports_v2() {
        let restore_db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("restore-v2-{}.db", unique_suffix()));

        let root_secret = omni_identity::AccountRootSecret(
            "ababababababababababababababababcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".to_string(),
        );
        let recovery_v2 = omni_identity::derive_recovery_phrase_v2(&root_secret).unwrap();

        let storage = Storage::open(&restore_db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .restore_local_account_from_mnemonic("Alice", &recovery_v2.mnemonic, "Laptop", 2_000)
            .unwrap();

        assert_eq!(
            account.account_representation_version,
            ACCOUNT_REPRESENTATION_VERSION_V2
        );
    }

    #[test]
    fn inspect_recovery_input_distinguishes_existing_locked_profile() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-storage-tests")
            .join(format!("inspect-locked-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("local profile summary should exist")
            .profile_id
            .clone();

        storage.sign_out_profile(&profile_id).unwrap();

        let preview = storage
            .inspect_recovery_input(
                &account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
                Some("Alice"),
                Some("Office PC"),
            )
            .unwrap();

        assert_eq!(preview.profile_id, profile_id);
        assert_eq!(preview.conflict, Some(RecoveryConflictCode::ProfileLocked));
        assert_eq!(preview.flow_kind, RecoveryFlowKind::UnlockSavedProfile);
        assert_eq!(
            preview.mnemonic_usage,
            RecoveryMnemonicUsage::OwnershipProofForUnlock
        );
        assert!(preview.can_unlock);
        assert!(!preview.can_restore);
        assert!(!preview.can_reopen);
    }

    #[test]
    fn reopen_local_profile_sets_active_profile_without_unlocking() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-storage-tests")
            .join(format!("reopen-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let alice = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let alice_profile_id = alice
            .local_profile_summary
            .as_ref()
            .expect("local profile summary should exist")
            .profile_id
            .clone();
        let bob = storage
            .create_local_account("Bob", "Laptop", 2_000)
            .unwrap();
        let bob_profile_id = bob
            .local_profile_summary
            .as_ref()
            .expect("local profile summary should exist")
            .profile_id
            .clone();

        let reopened = storage.reopen_local_profile(&alice_profile_id).unwrap();
        assert_eq!(reopened.profile_id, alice_profile_id);
        assert_eq!(reopened.state, LocalProfileState::SignedIn);
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(alice_profile_id.clone())
        );
        assert_eq!(
            storage
                .get_local_account()
                .unwrap()
                .unwrap()
                .local_profile_summary
                .unwrap()
                .profile_id,
            alice_profile_id
        );
        assert_ne!(alice_profile_id, bob_profile_id);
    }

    #[test]
    fn restore_local_account_from_mnemonic_rejects_existing_profile() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-storage-tests")
            .join(format!("restore-conflict-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let result = storage.restore_local_account_from_mnemonic(
            "Alice",
            &account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
            "Laptop",
            2_000,
        );
        let error = result.expect_err("restore should reject an already-known profile");
        assert!(
            error
                .to_string()
                .contains("profile already present on this device"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn sign_out_keeps_profile_saved_and_transitions_to_locked_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("signout-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        let after_sign_out = storage.sign_out_profile(&profile_id).unwrap();
        assert_eq!(after_sign_out.state, LocalProfileState::SignedOutLocked);

        let list = storage.list_local_profiles().unwrap();
        assert_eq!(list.profiles.len(), 1);
        assert_eq!(list.profiles[0].state, LocalProfileState::SignedOutLocked);
    }

    #[test]
    fn sign_out_keeps_local_account_shadow_profile_state_in_sync() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("signout-shadow-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        storage.sign_out_profile(&profile_id).unwrap();

        let connection = Storage::open_keyed_connection(&db_path).expect("shadow db should open");
        let shadow_state: String = connection
            .query_row(
                "SELECT profile_state FROM local_account WHERE profile_id = ?1",
                [profile_id.0.as_str()],
                |row| row.get(0),
            )
            .expect("local_account shadow state should load");
        assert_eq!(shadow_state, "signed_out_locked");
    }

    #[test]
    fn password_unlock_and_wrong_password_behave_safely() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("pw-unlock-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        storage
            .set_local_profile_password(&profile_id, "my-local-password")
            .unwrap();
        storage.sign_out_profile(&profile_id).unwrap();

        let wrong = storage.unlock_profile_with_password(&profile_id, "wrong-password");
        assert!(wrong.is_err());
        let unlocked = storage
            .unlock_profile_with_password(&profile_id, "my-local-password")
            .unwrap();
        assert_eq!(unlocked.state, LocalProfileState::SignedIn);
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(profile_id.clone())
        );
    }

    #[test]
    fn mnemonic_unlock_and_wrong_mnemonic_behave_safely() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("mnemonic-unlock-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let root_secret = omni_identity::AccountRootSecret(
            "12341234123412341234123412341234abababababababababababababababab".to_string(),
        );
        let recovery_v2 = omni_identity::derive_recovery_phrase_v2(&root_secret).unwrap();
        storage
            .restore_local_account_from_mnemonic("Alice", &recovery_v2.mnemonic, "Office PC", 1_000)
            .unwrap();

        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        storage.sign_out_profile(&profile_id).unwrap();

        let wrong = storage.unlock_profile_with_mnemonic(
            &profile_id,
            "foo abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
        );
        assert!(wrong.is_err());
        let unlocked = storage
            .unlock_profile_with_mnemonic(&profile_id, &recovery_v2.mnemonic)
            .unwrap();
        assert_eq!(unlocked.state, LocalProfileState::SignedIn);
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(profile_id.clone())
        );
    }

    #[test]
    fn reopen_local_profile_rejects_locked_profiles() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-storage-tests")
            .join(format!("reopen-locked-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("local profile summary should exist")
            .profile_id
            .clone();

        storage.sign_out_profile(&profile_id).unwrap();

        let reopen_error = storage.reopen_local_profile(&profile_id).unwrap_err();
        assert!(
            reopen_error
                .to_string()
                .contains("profile must be signed in before it can be reopened"),
            "unexpected error: {reopen_error}"
        );
    }

    #[test]
    fn v2_profile_secrets_are_stored_encrypted_at_rest() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("profile-secrets-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();

        let encrypted_blob_json: Option<String> = storage
            .connection
            .query_row(
                "SELECT encrypted_profile_secrets_json FROM profile_secrets WHERE profile_id = ?1",
                rusqlite::params![profile_id.0.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(encrypted_blob_json.is_some());
        assert!(!encrypted_blob_json.unwrap().contains("omni000"));
    }

    #[test]
    fn delete_local_profile_removes_local_account_data() {
        let test_root = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("delete-profile-{}", unique_suffix()));
        let db_path = test_root.join("storage.db");
        let source_attachment_path = test_root.join("delete-profile-attachment.txt");
        let avatar_source_path = test_root.join("delete-profile-avatar.png");

        fs::create_dir_all(source_attachment_path.parent().unwrap()).unwrap();
        fs::write(&source_attachment_path, "attachment to be wiped").unwrap();
        fs::write(&avatar_source_path, tiny_png_bytes()).unwrap();

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let _account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let updated_profile = storage
            .update_local_public_profile("Alice", Some(&avatar_source_path), false, 1_100)
            .unwrap();
        let avatar_image_path = updated_profile
            .public_avatar_image_path
            .clone()
            .expect("managed public avatar path should exist");
        assert!(Path::new(&avatar_image_path).exists());
        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();
        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-remote".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some("dialog:instance:inst-bob".to_string()),
                    initial_observation: None,
                },
                1_500,
            )
            .unwrap();
        storage
            .insert_outgoing_message(
                "msg-1",
                &contact
                    .primary_dialog_key
                    .clone()
                    .unwrap_or_else(|| "dialog:instance:inst-bob".to_string()),
                "hello",
                omni_types::MessageStatus::Queued,
                1_500,
            )
            .unwrap();
        storage
            .upsert_message_security_metadata(
                "msg-1",
                &contact
                    .primary_dialog_key
                    .clone()
                    .unwrap_or_else(|| "dialog:instance:inst-bob".to_string()),
                Some(&contact.contact_id),
                None,
                contact.remote_account_id.as_ref(),
                Some(&DeviceId("dev-remote-1".to_string())),
                MessageEncryptionState::EncryptedPending.as_db_value(),
                None,
                Some(SecurityPayloadKind::Text.as_db_value()),
                1_500,
                1_505,
            )
            .unwrap();
        let attachment_runtime = storage
            .prepare_outgoing_attachment(
                "msg-1",
                &contact
                    .primary_dialog_key
                    .clone()
                    .unwrap_or_else(|| "dialog:instance:inst-bob".to_string()),
                source_attachment_path.to_str().unwrap(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_510,
            )
            .unwrap();
        let attachment_blob_path = attachment_runtime
            .local_blob_path
            .clone()
            .expect("managed blob path should exist");
        assert!(attachment_blob_path.exists());

        storage.delete_local_profile(&profile_id).unwrap();

        assert!(storage.get_local_account().unwrap().is_none());
        assert_eq!(storage.get_active_profile_id().unwrap(), None);
        assert!(storage.list_local_profiles().unwrap().profiles.is_empty());
        assert!(storage.was_local_profile_deleted().unwrap());
        assert!(storage
            .get_local_profile_summary(&profile_id)
            .unwrap()
            .is_none());
        assert!(storage
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .is_none());
        assert_eq!(
            storage.count_profile_rows("messages", &profile_id).unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("message_attachments", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage.count_profile_rows("contacts", &profile_id).unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("contact_dialog_links", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("message_security_metadata", &profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("outbound_queue", &profile_id)
                .unwrap(),
            0
        );
        assert!(!attachment_blob_path.exists());
        assert!(!Path::new(&avatar_image_path).exists());

        storage
            .create_local_account("Alice", "Office PC", 2_000)
            .unwrap();
        assert!(!storage.was_local_profile_deleted().unwrap());
    }

    #[test]
    fn delete_local_profile_keeps_other_profiles_and_active_context_intact() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("delete-inactive-profile-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();

        let alice = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let alice_profile_id = alice
            .local_profile_summary
            .as_ref()
            .expect("alice profile summary should exist")
            .profile_id
            .clone();
        let alice_contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Alice Friend".to_string(),
                    remote_account_id: Some(AccountId("acct-alice-friend".to_string())),
                    public_display_name: Some("Alice Friend".to_string()),
                    remote_instance_id: Some("inst-alice-friend".to_string()),
                    primary_dialog_key: Some("dialog:instance:inst-alice-friend".to_string()),
                    initial_observation: None,
                },
                1_100,
            )
            .unwrap();
        storage
            .insert_outgoing_message(
                "alice-msg",
                &alice_contact
                    .primary_dialog_key
                    .clone()
                    .unwrap_or_else(|| "dialog:instance:inst-alice-friend".to_string()),
                "alice hello",
                MessageStatus::Queued,
                1_120,
            )
            .unwrap();

        let bob = storage
            .create_local_account("Bob", "Laptop", 2_000)
            .unwrap();
        let bob_profile_id = bob
            .local_profile_summary
            .as_ref()
            .expect("bob profile summary should exist")
            .profile_id
            .clone();
        storage
            .set_active_profile_id(Some(&alice_profile_id))
            .unwrap();
        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(alice_profile_id.clone())
        );

        storage.delete_local_profile(&bob_profile_id).unwrap();

        assert_eq!(
            storage.get_active_profile_id().unwrap(),
            Some(alice_profile_id.clone())
        );
        assert!(storage.get_local_account().unwrap().is_some());
        assert!(storage.profile_exists(&alice_profile_id).unwrap());
        assert!(!storage.profile_exists(&bob_profile_id).unwrap());
        assert_eq!(storage.list_local_profiles().unwrap().profiles.len(), 1);
        assert_eq!(
            storage
                .get_local_profile_summary(&alice_profile_id)
                .unwrap()
                .unwrap()
                .profile_id,
            alice_profile_id
        );
        assert!(storage
            .get_local_profile_summary(&bob_profile_id)
            .unwrap()
            .is_none());
        assert!(storage
            .preview_delete_local_profile(&bob_profile_id)
            .unwrap()
            .is_none());
        assert_eq!(
            storage
                .count_profile_rows("messages", &alice_profile_id)
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .count_profile_rows("messages", &bob_profile_id)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .count_profile_rows("contacts", &alice_profile_id)
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .count_profile_rows("contacts", &bob_profile_id)
                .unwrap(),
            0
        );
    }

    #[test]
    fn add_contact_is_idempotent_for_same_remote_account_id() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("contact-dedup-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let first = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_100,
            )
            .unwrap();
        let second = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob Duplicate".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob Duplicate".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_200,
            )
            .unwrap();

        assert_eq!(first.contact_id, second.contact_id);
        let contacts = storage.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].petname, "Bob");
        assert_eq!(
            contacts[0].remote_account_id,
            Some(AccountId("acct-bob".to_string()))
        );
    }

    #[test]
    fn duplicate_contacts_with_same_remote_account_merge_into_canonical_profile_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("contact-merge-existing-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        storage
            .connection
            .execute(
                "
                INSERT INTO contacts (
                    contact_id,
                    petname,
                    remote_account_id,
                    public_display_name,
                    remote_instance_id,
                    relationship_state,
                    has_met,
                    is_blocked,
                    removed_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, 0, NULL, ?6, ?7, ?8)
                ",
                rusqlite::params![
                    "contact-old",
                    "acct-bob",
                    "acct-bob",
                    Option::<String>::None,
                    "inst-old",
                    1_050,
                    1_050,
                    profile_id.0.clone(),
                ],
            )
            .unwrap();
        storage
            .connection
            .execute(
                "
                INSERT INTO contacts (
                    contact_id,
                    petname,
                    remote_account_id,
                    public_display_name,
                    public_avatar_hash_hex,
                    public_avatar_mime_type,
                    public_avatar_pending_hash_hex,
                    public_avatar_pending_mime_type,
                    public_profile_updated_at_unix_ms,
                    remote_instance_id,
                    relationship_state,
                    has_met,
                    is_blocked,
                    removed_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'approved', 1, 0, NULL, ?11, ?12, ?13)
                ",
                rusqlite::params![
                    "contact-new",
                    "Bob",
                    "acct-bob",
                    "Bob Public",
                    "avatar-hash-new",
                    "image/jpeg",
                    "avatar-pending-new",
                    "image/jpeg",
                    1_300,
                    "inst-new",
                    1_200,
                    1_300,
                    profile_id.0.clone(),
                ],
            )
            .unwrap();
        storage
            .upsert_contact_dialog_link(
                &ContactId("contact-old".to_string()),
                &dialog_key_from_instance_id("inst-old"),
                true,
                1_060,
            )
            .unwrap();
        storage
            .upsert_contact_dialog_link(
                &ContactId("contact-new".to_string()),
                &dialog_key_from_instance_id("inst-new"),
                true,
                1_300,
            )
            .unwrap();
        storage
            .upsert_message_security_metadata(
                "dup-merge-msg-1",
                &dialog_key_from_instance_id("inst-old"),
                Some(&ContactId("contact-old".to_string())),
                None,
                Some(&AccountId("acct-bob".to_string())),
                None,
                "encrypted_pending",
                None,
                Some("text"),
                1_100,
                1_100,
            )
            .unwrap();

        // Duplicate rows were planted directly (bypassing the write-time `find_*` merge paths),
        // as could happen from pre-merge-feature data or a race between two inserts. `list_contacts`
        // is a pure read now (the on-read merge was removed to stop a SQLITE_BUSY UI freeze, see
        // `list_contacts`), so the canonical merge is driven by the background sweep — the same call
        // the autoconnect worker tick makes.
        storage
            .merge_duplicate_contacts_for_active_profile(1_400)
            .unwrap();

        let contacts = storage.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].contact_id, ContactId("contact-new".to_string()));
        assert_eq!(contacts[0].petname, "Bob");
        assert_eq!(
            contacts[0].public_display_name.as_deref(),
            Some("Bob Public")
        );
        assert_eq!(
            contacts[0].public_avatar_hash_hex.as_deref(),
            Some("avatar-hash-new")
        );
        assert_eq!(
            contacts[0].public_avatar_pending_hash_hex.as_deref(),
            Some("avatar-pending-new")
        );
        assert!(contacts[0].has_met);

        assert_eq!(
            storage.count_profile_rows("contacts", &profile_id).unwrap(),
            1
        );
        assert_eq!(
            storage
                .resolve_contact_for_dialog(&dialog_key_from_instance_id("inst-old"))
                .unwrap()
                .expect("merged contact should resolve by old dialog")
                .contact_id,
            ContactId("contact-new".to_string())
        );
        assert_eq!(
            storage
                .resolve_contact_for_dialog(&dialog_key_from_instance_id("inst-new"))
                .unwrap()
                .expect("merged contact should resolve by new dialog")
                .contact_id,
            ContactId("contact-new".to_string())
        );

        let security_details = storage
            .get_message_security_details("dup-merge-msg-1")
            .unwrap()
            .expect("security details should exist");
        assert_eq!(
            security_details.contact_id,
            Some(ContactId("contact-new".to_string()))
        );
    }

    #[test]
    fn pending_contact_placeholder_names_are_enriched_after_identity_is_known() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("contact-enrich-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let placeholder = storage
            .add_pending_contact(
                &AddContactInput {
                    petname: "acct-bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("acct-bob".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_100,
            )
            .unwrap();

        let enriched = storage
            .add_pending_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_200,
            )
            .unwrap();

        assert_eq!(placeholder.contact_id, enriched.contact_id);
        assert_eq!(enriched.petname, "Bob");
        assert_eq!(enriched.public_display_name.as_deref(), Some("Bob"));
        assert_eq!(enriched.remote_instance_id.as_deref(), Some("inst-bob"));
    }

    #[test]
    fn custom_petname_survives_identity_enrichment() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("contact-enrich-custom-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        let placeholder = storage
            .add_contact(
                &AddContactInput {
                    petname: "Мой Борис".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("acct-bob".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_100,
            )
            .unwrap();

        let enriched = storage
            .add_pending_contact(
                &AddContactInput {
                    petname: "Boris".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Boris".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_200,
            )
            .unwrap();

        assert_eq!(placeholder.contact_id, enriched.contact_id);
        assert_eq!(enriched.petname, "Мой Борис");
        assert_eq!(enriched.public_display_name.as_deref(), Some("Boris"));
    }

    #[test]
    fn dangerous_action_previews_are_honest_for_stage10_scope() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-storage-tests")
            .join(format!("dangerous-previews-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = storage.list_local_profiles().unwrap().profiles[0]
            .profile_id
            .clone();

        let sign_out = storage
            .preview_sign_out_profile(&profile_id)
            .unwrap()
            .expect("sign out preview should exist");
        assert_eq!(sign_out.action, DangerousActionKind::SignOutProfile);
        assert!(sign_out.supported_now);
        assert!(sign_out.requires_confirmation);
        assert_eq!(sign_out.confirmation_token.as_deref(), Some("SIGN_OUT"));

        let delete_local = storage
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .expect("delete preview should exist");
        assert_eq!(delete_local.action, DangerousActionKind::DeleteLocalProfile);
        assert!(delete_local.supported_now);
        assert!(delete_local.requires_confirmation);
        assert!(delete_local.summary.contains("this local profile"));
        assert!(delete_local.summary.contains("other profiles"));
        assert!(delete_local
            .consequences
            .iter()
            .any(|value| value.contains("messages")));
        // Alice is the ONLY profile here, so the honest preview is not "other local profiles stay
        // intact" (true, but silent about what actually happens): deleting her also takes this
        // device's mesh identity, and the user is entitled to know a new profile will be a new node.
        assert!(
            delete_local
                .consequences
                .iter()
                .any(|value| value.contains("last local profile")
                    && value.contains("mesh node identity")),
            "a last-profile delete must disclose that the device's mesh identity goes with it: {:?}",
            delete_local.consequences
        );

        // With a second profile present the shared device state survives — and the preview must say
        // so instead of repeating the last-profile warning.
        storage
            .create_local_account("Bob", "Laptop", 2_000)
            .unwrap();
        let with_sibling = storage
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .expect("delete preview should exist");
        assert!(
            with_sibling
                .consequences
                .iter()
                .any(|value| value.contains("Other local profiles stay intact")),
            "a non-last delete must promise the shared device state survives: {:?}",
            with_sibling.consequences
        );
        assert_eq!(
            delete_local.confirmation_token.as_deref(),
            Some("DELETE_LOCAL_PROFILE")
        );

        let revoke_and_wipe = storage
            .preview_revoke_current_device_and_wipe(&profile_id)
            .unwrap()
            .expect("revoke/wipe preview should exist");
        assert_eq!(
            revoke_and_wipe.action,
            DangerousActionKind::RevokeCurrentDeviceAndWipe
        );
        assert!(!revoke_and_wipe.supported_now);
        assert!(!revoke_and_wipe.requires_confirmation);
        assert!(revoke_and_wipe.confirmation_token.is_none());
        assert!(revoke_and_wipe.unsupported_reason.is_some());
    }

    #[test]
    fn restore_local_account_reuses_account_identity_and_marks_restored_state() {
        let original_db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("orig-{}.db", unique_suffix()));
        let restore_db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("restore-{}.db", unique_suffix()));

        let original_storage = Storage::open(&original_db_path).unwrap();
        original_storage.migrate().unwrap();
        let original_account = original_storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let original_device = original_storage.get_local_device().unwrap().unwrap();

        let restore_storage = Storage::open(&restore_db_path).unwrap();
        restore_storage.migrate().unwrap();
        let restored_account = restore_storage
            .restore_local_account_from_mnemonic(
                "Alice",
                &original_account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
                "Laptop",
                2_000,
            )
            .unwrap();
        let restored_device = restore_storage.get_local_device().unwrap().unwrap();

        assert_eq!(
            restored_account.descriptor.account_id,
            original_account.descriptor.account_id
        );
        assert_ne!(restored_device.device_id, original_device.device_id);
        assert_eq!(restored_device.device_label, "Laptop");
        assert!(restored_account.restored_from_recovery);
        assert!(restored_device.restored_from_recovery);
    }

    #[test]
    fn local_identity_survives_storage_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let created = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        drop(storage);

        let reopened = Storage::open(&db_path).unwrap();
        reopened.migrate().unwrap();
        let reopened_account = reopened.get_local_account().unwrap().unwrap();
        let reopened_device = reopened.get_local_device().unwrap().unwrap();

        assert_eq!(reopened_account.descriptor, created.descriptor);
        assert_eq!(
            reopened_account.recovery_phrase_v2,
            created.recovery_phrase_v2
        );
        assert_eq!(
            reopened_account.current_device_id,
            reopened_device.device_id
        );
    }

    #[test]
    fn identity_migration_is_safe_on_stage3_database() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                    dialog_key TEXT,
                    remote_delivered_at_unix_ms INTEGER
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();

        assert_eq!(account.descriptor.display_name, "Alice");
        assert!(storage.get_local_device().unwrap().is_some());
    }

    #[test]
    fn shadow_current_device_rows_can_coexist_across_profiles() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let duplicate_insert = storage.connection.execute(
            "
            INSERT INTO local_devices (
                device_id,
                account_id,
                device_label,
                is_current,
                restored_from_recovery,
                created_at_unix_ms,
                updated_at_unix_ms
            )
            VALUES (?1, ?2, ?3, 1, 0, ?4, ?5)
            ",
            rusqlite::params![
                "dev-duplicate",
                account.descriptor.account_id.0,
                "Laptop",
                2_000,
                2_000
            ],
        );

        assert!(duplicate_insert.is_ok());
        assert_eq!(
            storage.get_profile_current_device_id(&profile_id).unwrap(),
            Some(account.current_device_id.clone())
        );
    }

    #[test]
    fn unsupported_legacy_crypto_database_is_reported() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("{}.db", unique_suffix()));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                    dialog_key TEXT,
                    remote_delivered_at_unix_ms INTEGER
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                CREATE TABLE local_account (
                    singleton INTEGER NOT NULL DEFAULT 1 UNIQUE,
                    account_id TEXT PRIMARY KEY NOT NULL,
                    display_name TEXT NOT NULL,
                    recovery_version INTEGER NOT NULL,
                    recovery_code TEXT NOT NULL,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE local_devices (
                    device_id TEXT PRIMARY KEY NOT NULL,
                    account_id TEXT NOT NULL,
                    device_label TEXT NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 0,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE UNIQUE INDEX ux_local_devices_current
                ON local_devices(is_current)
                WHERE is_current = 1;
                INSERT INTO local_account (
                    singleton,
                    account_id,
                    display_name,
                    recovery_version,
                    recovery_code,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (1, 'acct-stage4', 'Alice', 1, 'recovery-1', 0, 1000, 1000);
                INSERT INTO local_devices (
                    device_id,
                    account_id,
                    device_label,
                    is_current,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES ('dev-stage4', 'acct-stage4', 'Office PC', 1, 0, 1000, 1000);
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        let summary = storage.inspect_database_bootstrap_summary().unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            DatabaseBootstrapStateKind::UnsupportedLegacyLocalData
        );
        assert!(summary.reset_required);
        assert!(summary.can_create_new_account);
        assert!(summary.can_restore_from_mnemonic);
    }

    #[test]
    fn unsupported_legacy_toy_crypto_database_is_reported() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("toy-{}.db", unique_suffix()));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                    dialog_key TEXT,
                    remote_delivered_at_unix_ms INTEGER
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                CREATE TABLE local_account (
                    singleton INTEGER NOT NULL DEFAULT 1 UNIQUE,
                    account_id TEXT PRIMARY KEY NOT NULL,
                    display_name TEXT NOT NULL,
                    recovery_version INTEGER NOT NULL,
                    recovery_code TEXT NOT NULL,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE local_devices (
                    device_id TEXT PRIMARY KEY NOT NULL,
                    account_id TEXT NOT NULL,
                    device_label TEXT NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 0,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE UNIQUE INDEX ux_local_devices_current
                ON local_devices(is_current)
                WHERE is_current = 1;
                INSERT INTO local_account VALUES (1, 'acct-toy', 'Alice', 1, 'recovery-1', 0, 1000, 1000);
                INSERT INTO local_devices VALUES ('dev-toy', 'acct-toy', 'Office PC', 1, 0, 1000, 1000);

                CREATE TABLE identity_keys (
                    account_id TEXT PRIMARY KEY NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE device_keys (
                    device_id TEXT PRIMARY KEY NOT NULL,
                    account_id TEXT NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE signed_prekeys (
                    prekey_id INTEGER PRIMARY KEY NOT NULL,
                    device_id TEXT NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    signature_r TEXT NOT NULL,
                    signature_s TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'active',
                    created_at_unix_ms INTEGER NOT NULL,
                    expires_at_unix_ms INTEGER,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE one_time_prekeys (
                    prekey_id INTEGER PRIMARY KEY NOT NULL,
                    device_id TEXT NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'available',
                    created_at_unix_ms INTEGER NOT NULL,
                    consumed_at_unix_ms INTEGER,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE sessions (
                    session_address TEXT PRIMARY KEY NOT NULL,
                    remote_account_id TEXT NOT NULL,
                    remote_device_id TEXT NOT NULL,
                    local_account_id TEXT NOT NULL,
                    local_device_id TEXT NOT NULL,
                    local_identity_key TEXT NOT NULL,
                    local_device_key TEXT NOT NULL,
                    remote_identity_key TEXT NOT NULL,
                    remote_device_key TEXT NOT NULL,
                    session_key_protection_scheme TEXT NOT NULL,
                    session_key_blob TEXT NOT NULL,
                    bootstrap_state TEXT NOT NULL,
                    send_counter INTEGER NOT NULL DEFAULT 0,
                    receive_counter INTEGER NOT NULL DEFAULT 0,
                    remote_signed_prekey_id INTEGER NOT NULL,
                    remote_one_time_prekey_id INTEGER,
                    suite_version INTEGER NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE replay_windows (
                    session_address TEXT PRIMARY KEY NOT NULL,
                    highest_counter INTEGER NOT NULL DEFAULT 0,
                    recent_tags_json TEXT NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE remote_prekey_bundles (
                    bundle_key TEXT PRIMARY KEY NOT NULL,
                    remote_account_id TEXT NOT NULL,
                    remote_device_id TEXT NOT NULL,
                    account_display_name TEXT,
                    device_label TEXT,
                    identity_public_key TEXT NOT NULL,
                    device_public_key TEXT NOT NULL,
                    signed_prekey_id INTEGER NOT NULL,
                    signed_prekey_public_key TEXT NOT NULL,
                    signed_prekey_signature_r TEXT NOT NULL,
                    signed_prekey_signature_s TEXT NOT NULL,
                    one_time_prekey_id INTEGER,
                    one_time_prekey_public_key TEXT,
                    generated_at_unix_ms INTEGER NOT NULL,
                    observed_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL,
                    version INTEGER NOT NULL
                );
                INSERT INTO identity_keys VALUES ('acct-toy', '0011223344556677', 'plaintext_fallback_v1', 'aa', 1, 1000, 1000);
                INSERT INTO device_keys VALUES ('dev-toy', 'acct-toy', '8899aabbccddeeff', 'plaintext_fallback_v1', 'bb', 1, 1000, 1000);
                INSERT INTO signed_prekeys VALUES (7, 'dev-toy', '0102030405060708', 'plaintext_fallback_v1', 'cc', '11', '22', 1, 'active', 1000, NULL, 1000);
                INSERT INTO one_time_prekeys VALUES (8, 'dev-toy', '0a0b0c0d0e0f1011', 'plaintext_fallback_v1', 'dd', 1, 'available', 1000, NULL, 1000);
                INSERT INTO sessions VALUES (
                    'acct-remote::dev-remote',
                    'acct-remote',
                    'dev-remote',
                    'acct-toy',
                    'dev-toy',
                    '0011223344556677',
                    '8899aabbccddeeff',
                    'fedcba9876543210',
                    '7766554433221100',
                    'plaintext_fallback_v1',
                    'ee',
                    'established',
                    1,
                    1,
                    7,
                    8,
                    1,
                    1000,
                    1000
                );
                INSERT INTO replay_windows VALUES ('acct-remote::dev-remote', 1, '[]', 1000);
                INSERT INTO remote_prekey_bundles VALUES (
                    'acct-remote::dev-remote',
                    'acct-remote',
                    'dev-remote',
                    'Remote',
                    'Remote PC',
                    '0011223344556677',
                    '8899aabbccddeeff',
                    7,
                    '0102030405060708',
                    '11',
                    '22',
                    8,
                    '0a0b0c0d0e0f1011',
                    1000,
                    1000,
                    1000,
                    1
                );
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        let summary = storage.inspect_database_bootstrap_summary().unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            DatabaseBootstrapStateKind::UnsupportedLegacyLocalData
        );
        assert!(summary.reset_required);
    }

    #[test]
    fn migrate_prekey_tables_to_device_scoped_primary_keys_preserves_rows_and_allows_same_prekey_ids_per_device(
    ) {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("prekey-migration-{}.db", unique_suffix()));

        let connection = Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE local_account (
                    singleton INTEGER NOT NULL DEFAULT 1,
                    account_id TEXT PRIMARY KEY NOT NULL,
                    display_name TEXT NOT NULL,
                    recovery_version INTEGER NOT NULL,
                    recovery_code TEXT NOT NULL,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE local_devices (
                    device_id TEXT PRIMARY KEY NOT NULL,
                    account_id TEXT NOT NULL,
                    device_label TEXT NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 0,
                    restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                    created_at_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL,
                    FOREIGN KEY(account_id) REFERENCES local_account(account_id)
                );
                INSERT INTO local_account VALUES (1, 'acct-one', 'Alice', 1, 'recovery-1', 0, 1000, 1000);
                INSERT INTO local_devices VALUES ('dev-one', 'acct-one', 'Office PC', 1, 0, 1000, 1000);

                CREATE TABLE signed_prekeys (
                    prekey_id INTEGER PRIMARY KEY NOT NULL,
                    device_id TEXT NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    signature_r TEXT NOT NULL,
                    signature_s TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'active',
                    created_at_unix_ms INTEGER NOT NULL,
                    expires_at_unix_ms INTEGER,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                CREATE TABLE one_time_prekeys (
                    prekey_id INTEGER PRIMARY KEY NOT NULL,
                    device_id TEXT NOT NULL,
                    public_key TEXT NOT NULL,
                    private_key_protection_scheme TEXT NOT NULL,
                    private_key_blob TEXT NOT NULL,
                    suite_version INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'available',
                    created_at_unix_ms INTEGER NOT NULL,
                    consumed_at_unix_ms INTEGER,
                    updated_at_unix_ms INTEGER NOT NULL
                );
                INSERT INTO signed_prekeys VALUES (1, 'dev-one', '0102030405060708', 'plaintext_fallback_v1', 'aa', '11', '22', 2, 'active', 1000, NULL, 1000);
                INSERT INTO one_time_prekeys VALUES (1, 'dev-one', '0a0b0c0d0e0f1011', 'plaintext_fallback_v1', 'bb', 2, 'available', 1000, NULL, 1000);
                ",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();

        let mut signed_statement = storage
            .connection
            .prepare("PRAGMA table_info(signed_prekeys)")
            .unwrap();
        let signed_pk_columns = signed_statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .filter(|(_, pk_position)| *pk_position > 0)
            .collect::<Vec<_>>();
        assert_eq!(
            signed_pk_columns,
            vec![("device_id".to_string(), 1), ("prekey_id".to_string(), 2),]
        );

        let mut one_time_statement = storage
            .connection
            .prepare("PRAGMA table_info(one_time_prekeys)")
            .unwrap();
        let one_time_pk_columns = one_time_statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .filter(|(_, pk_position)| *pk_position > 0)
            .collect::<Vec<_>>();
        assert_eq!(
            one_time_pk_columns,
            vec![("device_id".to_string(), 1), ("prekey_id".to_string(), 2),]
        );

        storage
            .connection
            .execute(
                "
                INSERT INTO local_devices (
                    device_id,
                    account_id,
                    device_label,
                    is_current,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, 0, 0, ?4, ?5)
                ",
                rusqlite::params!["dev-two", "acct-one", "Laptop", 1_100, 1_100],
            )
            .unwrap();
        storage
            .connection
            .execute(
                "
                INSERT INTO signed_prekeys (
                    device_id,
                    prekey_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    signature_r,
                    signature_s,
                    suite_version,
                    state,
                    created_at_unix_ms,
                    expires_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, NULL, ?10)
                ",
                rusqlite::params![
                    "dev-two",
                    1,
                    "1112131415161718",
                    "plaintext_fallback_v1",
                    "cc",
                    "33",
                    "44",
                    CRYPTO_SUITE_VERSION_CURRENT as i64,
                    1_100,
                    1_100,
                ],
            )
            .unwrap();
        storage
            .connection
            .execute(
                "
                INSERT INTO one_time_prekeys (
                    device_id,
                    prekey_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    suite_version,
                    state,
                    created_at_unix_ms,
                    consumed_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'available', ?7, NULL, ?8)
                ",
                rusqlite::params![
                    "dev-two",
                    1,
                    "191a1b1c1d1e1f20",
                    "plaintext_fallback_v1",
                    "dd",
                    CRYPTO_SUITE_VERSION_CURRENT as i64,
                    1_100,
                    1_100,
                ],
            )
            .unwrap();

        let signed_count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM signed_prekeys WHERE prekey_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let one_time_count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM one_time_prekeys WHERE prekey_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(signed_count, 2);
        assert_eq!(one_time_count, 2);
    }

    #[test]
    fn ensure_local_crypto_foundation_repairs_missing_one_time_prekeys_without_sessions() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("repair-partial-foundation-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        assert!(storage
            .get_active_signed_prekey(&device.device_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_available_one_time_prekey(&device.device_id)
            .unwrap()
            .is_some());

        storage
            .connection
            .execute(
                "DELETE FROM one_time_prekeys WHERE device_id = ?1",
                rusqlite::params![device.device_id.0.as_str()],
            )
            .unwrap();

        assert!(storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_local_device_key(&device.device_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_active_signed_prekey(&device.device_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_available_one_time_prekey(&device.device_id)
            .unwrap()
            .is_none());

        storage.ensure_local_crypto_foundation(1_200).unwrap();

        assert!(storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_local_device_key(&device.device_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_active_signed_prekey(&device.device_id)
            .unwrap()
            .is_some());
        assert!(storage
            .get_available_one_time_prekey(&device.device_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn drained_one_time_prekeys_are_topped_up_instead_of_bricking_a_profile_with_sessions() {
        // Профиль с ЖИВЫМИ сессиями и пустым пулом одноразовых предключей — тот самый случай, что
        // убил стенд 2026-08-14. Соседний тест проверяет пул без сессий: там фундамент можно просто
        // пересобрать, и ошибки не было. С сессиями пересобирать нельзя (сменится личность), и
        // старый код отдавал «local crypto foundation is only partially initialized» — вход падал,
        // приложение уходило на онбординг при целой базе. Пул тратится по одному на каждый входящий
        // bootstrap, выдавался ровно один раз (8 штук) и не пополнялся никогда, так что попасть сюда
        // мог любой профиль.
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("drained-prekeys-with-sessions-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        let identity_before = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();

        let remote = generate_local_key_hierarchy(
            &AccountId("acct-remote".to_string()),
            &DeviceId("dev-remote".to_string()),
            1_200,
            1,
        )
        .unwrap();
        let remote_bundle = build_prekey_bundle(
            &remote.identity_key,
            &remote.device_key,
            &remote.signed_prekey,
            remote.one_time_prekeys.first(),
            1_200,
        )
        .unwrap();
        storage
            .record_remote_prekey_bundle(&remote_bundle, Some("Remote"), Some("Remote PC"), 1_250)
            .unwrap();
        let local_device_key = storage.get_local_device_key(&device.device_id).unwrap().unwrap();
        let (session, _) = bootstrap_initiator_session(
            &identity_before,
            &local_device_key,
            &remote_bundle,
            1_300,
        )
        .unwrap();
        storage
            .store_session_record(&mut establish_session(&session, 1_301))
            .unwrap();

        // Пул выбран досуха — ровно то, что делают входящие знакомства.
        storage
            .connection
            .execute(
                "DELETE FROM one_time_prekeys WHERE device_id = ?1",
                rusqlite::params![device.device_id.0.as_str()],
            )
            .unwrap();

        storage.ensure_local_crypto_foundation(1_400).unwrap();

        assert!(
            storage
                .get_available_one_time_prekey(&device.device_id)
                .unwrap()
                .is_some(),
            "пул обязан долиться, а не считаться повреждением"
        );
        let identity_after = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            identity_before.public_key, identity_after.public_key,
            "пополнение расходника не имеет права менять личность"
        );
    }

    /// One bundle reaches many peers and they all name its one-time prekey: the second to bootstrap
    /// still opens it within a week of the first, not after; taking it again does not move that week.
    #[test]
    fn a_one_time_prekey_taken_by_one_peer_still_opens_another_peers_bootstrap_for_a_week() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("shared-one-time-prekey-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        let named = storage.build_local_prekey_bundle(1_200).unwrap().one_time_prekey.unwrap().prekey_id;

        let taken_at = 10_000;
        storage.mark_one_time_prekey_consumed(&device.device_id, named, taken_at).unwrap();
        assert!(storage.get_available_one_time_prekey_by_id(&device.device_id, named).unwrap().is_none());
        let late = storage
            .get_one_time_prekey_for_inbound_bootstrap(&device.device_id, named, taken_at + 60_000)
            .unwrap()
            .expect("the second peer's bootstrap opens");
        assert_eq!(late.prekey_id, named);
        storage
            .mark_one_time_prekey_consumed(&device.device_id, named, taken_at + 60_000)
            .unwrap();
        let week_later = taken_at + crate::ONE_TIME_PREKEY_SHARED_GRACE_MS + 1;
        assert!(
            storage
                .get_one_time_prekey_for_inbound_bootstrap(&device.device_id, named, week_later)
                .unwrap()
                .is_none(),
            "a week after the FIRST take it is gone, a second take does not extend it"
        );
        assert_ne!(
            storage.build_local_prekey_bundle(week_later).unwrap().one_time_prekey.map(|prekey| prekey.prekey_id),
            Some(named),
            "the bundle moves on to an unused one"
        );
    }

    #[test]
    fn replenished_prekey_ids_continue_past_everything_already_stored() {
        // Номера обязано назначать ХРАНИЛИЩЕ. Генератор берёт их из счётчика, который живёт в
        // процессе и начинается с единицы при каждом запуске приложения, а израсходованные строки из
        // таблицы не удаляются — поэтому после перезапуска долив вставлял бы уже занятые номера и
        // падал на UNIQUE, прямо внутри инициализации. Профиль после этого не открывался вовсе
        // (живой случай 2026-08-14).
        //
        // Перезапуск процесса в тесте не воспроизвести (счётчик статический), поэтому проверяем сам
        // инвариант: любой долитый номер больше всего, что уже лежит. Высокий существующий номер
        // ставит генератор заведомо «позади» — без назначения из хранилища проверка падает.
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("prekey-ids-continue-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        storage
            .connection
            .execute(
                "UPDATE one_time_prekeys SET prekey_id = 10000 WHERE device_id = ?1 AND prekey_id = (SELECT MIN(prekey_id) FROM one_time_prekeys WHERE device_id = ?1)",
                rusqlite::params![device.device_id.0.as_str()],
            )
            .unwrap();

        storage
            .replenish_one_time_prekeys(&device.device_id, 1_500)
            .unwrap();

        let lowest_new: i64 = storage
            .connection
            .query_row(
                "SELECT MIN(prekey_id) FROM one_time_prekeys WHERE device_id = ?1 AND created_at_unix_ms = 1500",
                rusqlite::params![device.device_id.0.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            lowest_new > 10000,
            "долитые номера обязаны продолжать существующие, а не начинаться заново (получено {lowest_new})"
        );
    }

    #[test]
    fn consuming_the_last_one_time_prekey_refills_the_pool() {
        // Доливать только на входе мало: между опустошением пула и перезапуском все входящие
        // bootstrap'ы отваливались бы с «expected one-time prekey is missing».
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("refill-on-consume-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        // Тратим всё, что есть, по одному — как это делают входящие знакомства.
        let mut consumed = 0;
        while let Some(prekey) = storage.get_available_one_time_prekey(&device.device_id).unwrap() {
            storage
                .mark_one_time_prekey_consumed(&device.device_id, prekey.prekey_id, 1_200)
                .unwrap();
            consumed += 1;
            if consumed > 64 {
                break;
            }
        }

        assert!(
            consumed > omni_crypto::DEFAULT_ONE_TIME_PREKEY_COUNT,
            "пул обязан долиться в процессе, а не кончиться на стартовом запасе (израсходовано {consumed})"
        );
        assert!(storage
            .get_available_one_time_prekey(&device.device_id)
            .unwrap()
            .is_some());
    }

    /// Optimistic concurrency on session writes: of two copies read at the same revision, the first
    /// write wins and the second — now stale, it would roll the ratchet back — is refused and
    /// changes nothing (docs/bugs/stale-session-copy-diverges-ratchet.md). Negative half: a copy
    /// re-read after the write stores fine, and a deliberate replacement ignores the revision.
    /// Also: rows written before the column existed are lifted to revision 1 by `migrate`.
    #[test]
    fn a_stale_session_copy_is_refused_and_a_fresh_one_is_stored() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("cas-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        let remote = generate_local_key_hierarchy(
            &AccountId("acct-remote".to_string()),
            &DeviceId("dev-remote".to_string()),
            1_200,
            1,
        )
        .unwrap();
        let remote_bundle = build_prekey_bundle(
            &remote.identity_key,
            &remote.device_key,
            &remote.signed_prekey,
            remote.one_time_prekeys.first(),
            1_200,
        )
        .unwrap();
        let local_identity_key =
            storage.get_local_identity_key(&account.descriptor.account_id).unwrap().unwrap();
        let local_device_key = storage.get_local_device_key(&device.device_id).unwrap().unwrap();
        let (built, _) =
            bootstrap_initiator_session(&local_identity_key, &local_device_key, &remote_bundle, 1_300)
                .unwrap();
        let mut session = establish_session(&built, 1_301);
        assert_eq!(session.revision, 0, "a just-built session was never stored");
        storage.store_session_record(&mut session).unwrap();
        assert_eq!(session.revision, 1);
        let address = session.session_address.clone();

        let mut first = storage.get_session_record(&address).unwrap().unwrap();
        let mut second = storage.get_session_record(&address).unwrap().unwrap();
        first.send_counter = 7;
        second.send_counter = 3;
        storage.store_session_record(&mut first).unwrap();
        assert_eq!(first.revision, 2);
        let error = storage.store_session_record(&mut second).unwrap_err().to_string();
        assert!(error.contains(crate::STALE_SESSION_WRITE), "{error}");
        let stored = storage.get_session_record(&address).unwrap().unwrap();
        assert_eq!(stored.send_counter, 7, "the stale write changed nothing");
        assert_eq!(stored.revision, 2);
        // A just-built session cannot silently overwrite an existing one either.
        let mut rebuilt = establish_session(&built, 1_400);
        assert!(storage.store_session_record(&mut rebuilt).is_err());

        // Negative half: the same copy keeps working, a re-read copy stores, a replacement wins.
        first.send_counter = 8;
        // Remembered keys of overtaken frames round-trip through the row (the late frame may come
        // after a restart).
        first.protected_skipped_message_keys = Some(omni_crypto::protect_bytes(&[5u8; 72]).unwrap());
        storage.store_session_record(&mut first).unwrap();
        assert_eq!(
            storage.get_session_record(&address).unwrap().unwrap().protected_skipped_message_keys,
            first.protected_skipped_message_keys
        );
        let mut reread = storage.get_session_record(&address).unwrap().unwrap();
        reread.send_counter = 9;
        storage.store_session_record(&mut reread).unwrap();
        storage.replace_session_record(&mut rebuilt).unwrap();
        let stored = storage.get_session_record(&address).unwrap().unwrap();
        assert_eq!(stored.send_counter, rebuilt.send_counter, "the replacement is what is stored");
        assert_eq!(stored.revision, 5);
        assert_eq!(rebuilt.revision, 5);

        // Legacy rows (revision 0, before the column existed) are lifted to 1 on migrate.
        storage.connection.execute("UPDATE sessions SET revision = 0", []).unwrap();
        storage.migrate().unwrap();
        assert_eq!(storage.get_session_record(&address).unwrap().unwrap().revision, 1);
    }

    #[test]
    fn local_crypto_identity_and_session_records_survive_reopen() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        let remote = generate_local_key_hierarchy(
            &AccountId("acct-remote".to_string()),
            &DeviceId("dev-remote".to_string()),
            1_200,
            1,
        )
        .unwrap();
        let remote_bundle = build_prekey_bundle(
            &remote.identity_key,
            &remote.device_key,
            &remote.signed_prekey,
            remote.one_time_prekeys.first(),
            1_200,
        )
        .unwrap();
        storage
            .record_remote_prekey_bundle(&remote_bundle, Some("Remote"), Some("Remote PC"), 1_250)
            .unwrap();

        let local_identity_key = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();
        let local_device_key = storage
            .get_local_device_key(&device.device_id)
            .unwrap()
            .unwrap();
        let (session, _) = bootstrap_initiator_session(
            &local_identity_key,
            &local_device_key,
            &remote_bundle,
            1_300,
        )
        .unwrap();
        let mut session = establish_session(&session, 1_301);
        storage.store_session_record(&mut session).unwrap();
        let replay_window = storage
            .ensure_replay_window(&session.session_address, 1_302)
            .unwrap();
        storage.store_replay_window(&replay_window).unwrap();

        drop(storage);

        let reopened = Storage::open(&db_path).unwrap();
        reopened.migrate().unwrap();

        assert!(reopened
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .is_some());
        assert!(reopened
            .get_local_device_key(&device.device_id)
            .unwrap()
            .is_some());
        assert!(reopened
            .get_remote_prekey_bundle(&remote_bundle.account_id, &remote_bundle.device_id)
            .unwrap()
            .is_some());
        assert!(reopened
            .get_session_record(&session.session_address)
            .unwrap()
            .is_some());
        assert!(reopened
            .get_replay_window(&session.session_address)
            .unwrap()
            .is_some());
    }

    #[test]
    fn stage6_sync_migration_is_safe_on_existing_stage5_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-6-storage-tests")
            .join(format!("legacy-{}.db", unique_suffix()));

        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let account = storage
            .create_local_account("Alice", "Office PC", 1_000)
            .unwrap();
        let contact = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_050,
            )
            .unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        let remote = generate_local_key_hierarchy(
            &AccountId("acct-remote".to_string()),
            &DeviceId("dev-remote".to_string()),
            1_200,
            1,
        )
        .unwrap();
        let remote_bundle = build_prekey_bundle(
            &remote.identity_key,
            &remote.device_key,
            &remote.signed_prekey,
            remote.one_time_prekeys.first(),
            1_200,
        )
        .unwrap();
        storage
            .record_remote_prekey_bundle(&remote_bundle, Some("Remote"), Some("Remote PC"), 1_250)
            .unwrap();
        let local_identity_key = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();
        let local_device_key = storage
            .get_local_device_key(&storage.get_local_device().unwrap().unwrap().device_id)
            .unwrap()
            .unwrap();
        let (session, _) = bootstrap_initiator_session(
            &local_identity_key,
            &local_device_key,
            &remote_bundle,
            1_300,
        )
        .unwrap();
        storage
            .store_session_record(&mut establish_session(&session, 1_301))
            .unwrap();

        drop(storage);

        let reopened = Storage::open(&db_path).unwrap();
        reopened.migrate().unwrap();
        assert!(reopened.get_local_account().unwrap().is_some());
        assert!(reopened.get_local_device().unwrap().is_some());
        assert!(reopened.get_contact(&contact.contact_id).is_ok());
        assert!(reopened
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .is_some());
        assert!(reopened
            .get_session_record(&SessionAddress {
                remote_account_id: AccountId("acct-remote".to_string()),
                remote_device_id: DeviceId("dev-remote".to_string()),
            })
            .unwrap()
            .is_some());
    }

    // TM-006 path confinement regression tests.
    // See docs/decisions/path-confinement-tm006.md.

    #[test]
    fn delete_local_data_rejects_relative_path() {
        let result = Storage::delete_local_data_at_path(std::path::PathBuf::from("relative/path.db"));
        assert!(result.is_err(), "relative path must be rejected");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("absolute"),
            "error message should mention absolute requirement, got: {msg}"
        );
    }

    #[test]
    fn delete_local_data_rejects_windows_system_path() {
        // Windows-only: try to delete inside C:\Windows. On non-Windows hosts
        // this test still runs but exercises the non-Windows blocklist path
        // (which doesn't list `/Windows` — so the test would erroneously pass).
        // Skip on non-Windows.
        #[cfg(windows)]
        {
            let target = std::path::PathBuf::from(r"C:\Windows\system32\config.dat");
            let result = Storage::delete_local_data_at_path(&target);
            assert!(result.is_err(), "system path must be rejected");
            let msg = format!("{}", result.unwrap_err());
            assert!(
                msg.contains("protected system directory"),
                "error message should mention protected system dir, got: {msg}"
            );
        }
    }

    #[test]
    fn delete_local_data_accepts_user_data_path() {
        // A path inside the temp directory must succeed (delete returns Ok
        // even if files don't exist — graceful NotFound handling).
        let target = std::env::temp_dir()
            .join("omni-tm006-confinement-tests")
            .join(format!("ok-{}.db", unique_suffix()));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let result = Storage::delete_local_data_at_path(&target);
        assert!(
            result.is_ok(),
            "user-data path must pass confinement, got: {:?}",
            result
        );
    }

    // TM-005 attachment quota regression tests.
    // See docs/decisions/attachment-transfer-quotas.md.

    #[test]
    fn count_inbound_attachments_for_dialog_counts_only_downloads_holding_a_file() {
        let db_path = std::env::temp_dir()
            .join("omni-tm005-storage-tests")
            .join(format!("count-{}.db", unique_suffix()));
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "PC", 1_000).unwrap();

        let dialog_a = dialog_key_from_instance_id("inst-bob");
        let dialog_b = dialog_key_from_instance_id("inst-carol");

        // Three offers on dialog A, two on dialog B.
        for (idx, dialog_key) in [
            ("a1", dialog_a.as_str()),
            ("a2", dialog_a.as_str()),
            ("a3", dialog_a.as_str()),
            ("b1", dialog_b.as_str()),
            ("b2", dialog_b.as_str()),
        ] {
            storage
                .store_incoming_attachment_offer(
                    idx,
                    dialog_key,
                    "f.txt",
                    100,
                    Some("text/plain"),
                    "sha",
                    Some("chacha20_poly1305_v1"),
                    Some(TEST_MEDIA_KEY_HEX),
                    Some(TEST_MEDIA_NONCE_SEED_HEX),
                    Some(16_384),
                    1_010,
                )
                .unwrap();
        }
        // An offer nobody asked for holds no file: it is not counted.
        assert_eq!(storage.count_inbound_attachments_for_dialog(&dialog_a).unwrap(), 0);
        assert_eq!(storage.count_inbound_attachments_for_dialog(&dialog_b).unwrap(), 0);

        // Asked for, it holds a temp file until it is done.
        for idx in ["a1", "a2"] {
            let save_path = std::env::temp_dir()
                .join("omni-tm005-storage-tests")
                .join(format!("count-save-{idx}-{}.bin", unique_suffix()));
            storage
                .mark_attachment_download_requested(idx, &save_path, &format!("xfer-{idx}"), 1_050)
                .unwrap();
        }
        assert_eq!(storage.count_inbound_attachments_for_dialog(&dialog_a).unwrap(), 2);
        assert_eq!(storage.count_inbound_attachments_for_dialog(&dialog_b).unwrap(), 0);

        // Transition one record to Failed — it must drop out of the count.
        storage
            .mark_attachment_transfer_failed(
                "a1",
                omni_types::AttachmentTransferState::Failed,
                "manual",
                1_100,
            )
            .unwrap();
        assert_eq!(storage.count_inbound_attachments_for_dialog(&dialog_a).unwrap(), 1);
    }

    /// A big outgoing file is written down first and read later (C2, attachment-content-tree.md):
    /// `Preparing` is not a copy to serve; the read makes it `Available` under the tree of the very
    /// file that was picked; a changed file, a stopped pass and a cancelled row end without a tree —
    /// and without a managed copy left behind.
    #[test]
    fn an_outgoing_file_is_prepared_in_two_steps_and_a_stopped_one_leaves_nothing() {
        let root = std::env::temp_dir()
            .join("omni-prepare-storage-tests")
            .join(format!("prepare-{}", unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        let storage = Storage::open(root.join("omni.db")).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "PC", 1_000).unwrap();
        let dialog = dialog_key_from_instance_id("inst-bob");
        let e2ee = omni_types::AttachmentMediaProtectionMode::E2eeMedia;
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let big: Vec<u8> = (0..3 * 1024 * 1024 + 5)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let write = |name: &str, bytes: &[u8]| {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            path.to_string_lossy().into_owned()
        };
        for id in ["m-big", "m-changed", "m-stop", "m-small"] {
            storage
                .insert_outgoing_message(id, &dialog, "", omni_types::MessageStatus::Queued, 1_000)
                .unwrap();
        }

        // Written down: named, sized, keyed; no tree, not served.
        let source = write("big.bin", &big);
        let begun = storage.begin_outgoing_attachment("m-big", &dialog, &source, None, e2ee, 1_000).unwrap();
        assert_eq!(begun.attachment.state, omni_types::AttachmentTransferState::Preparing);
        assert!(!begun.attachment.local_blob_available);
        assert_eq!(begun.attachment.content_id_hex, "");
        assert_eq!(begun.attachment.file_size_bytes, big.len() as i64);
        assert_eq!(storage.list_preparing_attachments().unwrap(), vec![("m-big".to_string(), source.clone())]);
        assert!(storage.set_attachment_preparation_progress("m-big", 1024, 1_050).unwrap());

        // Read: available under the tree of what was picked; the reading was heard all the way.
        let mut heard = Vec::new();
        let done = storage
            .finish_outgoing_attachment_preparation("m-big", &mut |read| { heard.push(read); true }, 1_100)
            .unwrap();
        assert_eq!(done, vec!["m-big".to_string()]);
        let ready = storage.get_attachment_runtime_info("m-big").unwrap().unwrap();
        assert_eq!(ready.attachment.state, omni_types::AttachmentTransferState::Available);
        assert_eq!(ready.attachment.content_id_hex, content_id_hex_of(&big));
        assert_eq!(ready.attachment.bytes_transferred, 0);
        assert!(ready.attachment.local_blob_available);
        assert_eq!(heard.last().copied(), Some(big.len() as u64));
        assert!(storage.attachment_content_tree("m-big").unwrap().is_some());
        assert!(storage.list_preparing_attachments().unwrap().is_empty());
        // Done is done: no second reading, no progress over an available file.
        assert!(!storage.set_attachment_preparation_progress("m-big", 1, 1_150).unwrap());

        // The file changed after it was picked: no tree of another file under this message.
        let source = write("changed.bin", &big[..4096]);
        storage.begin_outgoing_attachment("m-changed", &dialog, &source, None, e2ee, 1_000).unwrap();
        fs::write(&source, &big[..8192]).unwrap();
        let changed = storage.finish_outgoing_attachment_preparation("m-changed", &mut |_| true, 1_200);
        assert!(changed.unwrap_err().to_string().contains("changed"));

        // Asked to stop: no tree, still preparing (a restart reads it again) — until cancelled.
        let source = write("stop.bin", &big);
        storage.begin_outgoing_attachment("m-stop", &dialog, &source, None, e2ee, 1_000).unwrap();
        let stopped = storage.finish_outgoing_attachment_preparation("m-stop", &mut |_| false, 1_300);
        assert!(stopped.unwrap_err().to_string().contains(crate::ATTACHMENT_PREPARATION_STOPPED));
        assert_eq!(
            storage.get_attachment_runtime_info("m-stop").unwrap().unwrap().attachment.state,
            omni_types::AttachmentTransferState::Preparing
        );
        assert!(storage.fail_outgoing_attachment_preparation("m-stop", "cancelled", 1_350).unwrap());
        let cancelled = storage.get_attachment_runtime_info("m-stop").unwrap().unwrap();
        assert_eq!(cancelled.attachment.state, omni_types::AttachmentTransferState::Failed);
        assert_eq!(cancelled.attachment.last_error.as_deref(), Some("cancelled"));
        assert_eq!(storage.get_message("m-stop").unwrap().status, omni_types::MessageStatus::Failed);
        // Out of the queue too: it would otherwise offer a file with no tree.
        assert!(storage
            .pending_queue_for_dialog(&dialog, i64::MAX)
            .unwrap()
            .iter()
            .all(|item| item.message_id != "m-stop"));
        assert!(!storage.fail_outgoing_attachment_preparation("m-stop", "again", 1_360).unwrap());
        assert!(!storage.fail_outgoing_attachment_preparation("m-big", "too late", 1_360).unwrap());

        // Cancelled while it was being read: the managed copy the reading made does not stay.
        let small = &big[..500];
        let source = write("small.bin", small);
        storage.begin_outgoing_attachment("m-small", &dialog, &source, None, e2ee, 1_000).unwrap();
        let raced = storage.finish_outgoing_attachment_preparation(
            "m-small",
            &mut |_| {
                storage.fail_outgoing_attachment_preparation("m-small", "cancelled", 1_400).unwrap();
                true
            },
            1_400,
        );
        assert!(raced.unwrap_err().to_string().contains(crate::ATTACHMENT_PREPARATION_STOPPED));
        let managed = storage
            .managed_blob_path_for_attachment(&content_id_hex_of(small), "small.bin")
            .unwrap();
        assert!(!managed.exists(), "{}", managed.display());
        let left: Vec<String> = storage.list_preparing_attachments().unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(left, vec!["m-changed".to_string()]);
    }

    /// A preparation cut short (a restart, the profile stopped) picks up at the blocks it had read,
    /// not at the start, and the tree is that of one whole reading. Cancelled, it keeps nothing.
    #[test]
    fn a_preparation_cut_short_picks_up_where_it_was() {
        let root = std::env::temp_dir()
            .join("omni-prepare-storage-tests")
            .join(format!("resume-{}", unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        let storage = Storage::open(root.join("omni.db")).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "PC", 1_000).unwrap();
        let dialog = dialog_key_from_instance_id("inst-bob");
        let e2ee = omni_types::AttachmentMediaProtectionMode::E2eeMedia;
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let bytes: Vec<u8> = (0..2 * block + 4321).map(|at| (at * 13 % 249) as u8).collect();
        let source = root.join("film.mkv");
        fs::write(&source, &bytes).unwrap();
        let source = source.to_string_lossy().into_owned();
        for id in ["m-film", "m-again"] {
            storage.insert_outgoing_message(id, &dialog, "", omni_types::MessageStatus::Queued, 1_000).unwrap();
        }
        storage.begin_outgoing_attachment("m-film", &dialog, &source, None, e2ee, 1_000).unwrap();

        // Stopped a little into the second block: the first is kept.
        let stopped = storage.finish_outgoing_attachment_preparation("m-film", &mut |read| read < (block + block / 4) as u64, 1_100);
        assert!(stopped.unwrap_err().to_string().contains(crate::ATTACHMENT_PREPARATION_STOPPED));
        let mut heard = Vec::new();
        let done = storage
            .finish_outgoing_attachment_preparation("m-film", &mut |read| { heard.push(read); true }, 1_200)
            .unwrap();
        assert_eq!(done, vec!["m-film".to_string()]);
        assert!(heard[0] > block as u64, "read again from {} bytes", heard[0] - omni_crypto::CONTENT_TREE_LEAF_BYTES);
        let ready = storage.get_attachment_runtime_info("m-film").unwrap().unwrap();
        assert_eq!(ready.attachment.content_id_hex, content_id_hex_of(&bytes));
        assert!(storage.attachment_content_tree("m-film").unwrap().is_some());

        // Cancelled after it was cut short: the blocks go with it, nothing to pick up later.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut changed = bytes.clone();
        changed[7] ^= 0xFF;
        fs::write(&source, &changed).unwrap();
        storage.begin_outgoing_attachment("m-again", &dialog, &source, None, e2ee, 1_300).unwrap();
        let _ = storage.finish_outgoing_attachment_preparation("m-again", &mut |read| read < (block + block / 4) as u64, 1_310);
        assert!(storage.fail_outgoing_attachment_preparation("m-again", "cancelled", 1_320).unwrap());
        assert!(storage.attachment_content_tree("m-again").unwrap().is_none());
        let kept: Option<Vec<u8>> = storage
            .connection
            .query_row("SELECT content_tree_blocks FROM message_attachments WHERE message_id = 'm-again'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, None);
    }

    /// One file sent to several chats is read once (C3): the messages waiting on it are completed
    /// by one reading, under one tree; sent again later, unchanged, it is not read at all. A file
    /// changed since is read anew.
    #[test]
    fn one_file_sent_to_several_chats_is_read_once() {
        let root = std::env::temp_dir()
            .join("omni-prepare-storage-tests")
            .join(format!("shared-{}", unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        let storage = Storage::open(root.join("omni.db")).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "PC", 1_000).unwrap();
        let e2ee = omni_types::AttachmentMediaProtectionMode::E2eeMedia;
        let bytes: Vec<u8> = (0..2 * 1024 * 1024 + 77).map(|i| (i * 7 % 253) as u8).collect();
        let source = root.join("film.mkv");
        fs::write(&source, &bytes).unwrap();
        let source = source.to_string_lossy().into_owned();
        let chats = ["inst-bob", "inst-carol", "inst-dave"].map(dialog_key_from_instance_id);
        for (id, dialog) in ["to-bob", "to-carol", "to-dave"].iter().zip(&chats) {
            storage.insert_outgoing_message(id, dialog, "", omni_types::MessageStatus::Queued, 1_000).unwrap();
        }

        storage.begin_outgoing_attachment("to-bob", &chats[0], &source, None, e2ee, 1_000).unwrap();
        storage.begin_outgoing_attachment("to-carol", &chats[1], &source, None, e2ee, 1_001).unwrap();
        // The progress of the one reading is every waiting bubble's.
        assert!(storage.set_attachment_preparation_progress("to-bob", 4096, 1_010).unwrap());
        assert_eq!(storage.get_attachment_runtime_info("to-carol").unwrap().unwrap().attachment.bytes_transferred, 4096);

        let mut reads = 0;
        let done = storage
            .finish_outgoing_attachment_preparation("to-carol", &mut |_| { reads += 1; true }, 1_100)
            .unwrap();
        assert_eq!(done, vec!["to-bob".to_string(), "to-carol".to_string()]);
        assert!(reads > 0);
        let id_of = |message: &str| storage.get_attachment_runtime_info(message).unwrap().unwrap().attachment.content_id_hex;
        assert_eq!(id_of("to-bob"), content_id_hex_of(&bytes));
        assert_eq!(id_of("to-carol"), id_of("to-bob"));
        assert!(storage.attachment_content_tree("to-bob").unwrap().is_some());

        // Sent once more, unchanged: ready without reading a byte.
        storage.begin_outgoing_attachment("to-dave", &chats[2], &source, None, e2ee, 1_200).unwrap();
        let done = storage
            .finish_outgoing_attachment_preparation("to-dave", &mut |_| panic!("read again"), 1_210)
            .unwrap();
        assert_eq!(done, vec!["to-dave".to_string()]);
        assert_eq!(id_of("to-dave"), id_of("to-bob"));
        // Each offer keeps its own key — one file, separate secrets per chat.
        let key_of = |message: &str| storage.get_attachment_runtime_info(message).unwrap().unwrap().content_key_hex;
        assert_ne!(key_of("to-dave"), key_of("to-bob"));

        // Changed since: read anew, under the tree of what it is now.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut changed = bytes.clone();
        changed[100] ^= 0xFF;
        fs::write(&source, &changed).unwrap();
        storage.insert_outgoing_message("to-bob-2", &chats[0], "", omni_types::MessageStatus::Queued, 1_300).unwrap();
        storage.begin_outgoing_attachment("to-bob-2", &chats[0], &source, None, e2ee, 1_300).unwrap();
        let mut reads = 0;
        storage
            .finish_outgoing_attachment_preparation("to-bob-2", &mut |_| { reads += 1; true }, 1_310)
            .unwrap();
        assert!(reads > 0);
        assert_eq!(id_of("to-bob-2"), content_id_hex_of(&changed));
    }

    fn content_id_hex_of(bytes: &[u8]) -> String {
        let mut tree = omni_crypto::ContentTreeBuilder::new();
        tree.update(bytes);
        hex::encode(tree.finish().id)
    }

    fn unique_suffix() -> u128 {
        crate::sweep_stale_test_temp();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;

        (timestamp << 16) ^ counter
    }

    // TM-006 follow-up (2026-05-21): verify the path-confinement
    // blocklist that gates `Storage::delete_local_data_at_path`,
    // `Storage::save_attachment_copy_to_path`,
    // `Storage::export_database_backup`, the bundle delete in
    // `omni-app::export_backup_bundle_contents`, and the entry-point
    // gate in `omni-app::request_attachment_download`. The helper is
    // crate-private; we exercise it through the public wrapper
    // `Storage::confine_destructive_path_for_callers` to match what
    // out-of-crate callers see.
    // See docs/decisions/tm-006-destructive-path-confinement.md.

    #[test]
    #[cfg(windows)]
    fn tm006_confine_rejects_windows_system_path() {
        for sample in [
            r"C:\Windows\System32\config.dat",
            r"C:\Program Files\evil.db",
            r"C:\Program Files (x86)\evil.db",
            r"C:\ProgramData\evil.db",
        ] {
            let result = Storage::confine_destructive_path_for_callers(Path::new(sample));
            assert!(
                result.is_err(),
                "TM-006 leak: confine accepted a Windows system path: {sample}"
            );
            let message = format!("{}", result.unwrap_err());
            assert!(
                message.contains("protected system directory"),
                "unexpected error for '{sample}': {message}"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn tm006_confine_rejects_drive_root() {
        // Bare drive roots resolve to "" once the drive letter and
        // extended-length prefix are stripped — they must be rejected.
        for sample in [r"C:\", r"D:\", r"\\?\C:\"] {
            let result = Storage::confine_destructive_path_for_callers(Path::new(sample));
            assert!(
                result.is_err(),
                "TM-006 leak: confine accepted a bare drive root: {sample}"
            );
        }
    }

    #[test]
    fn tm006_confine_rejects_relative_path() {
        // Destructive paths MUST be absolute. Anything relative —
        // `foo.db`, `..\..\Windows\evil`, etc. — is rejected before
        // canonicalization so callers can't relative-path-traverse
        // out of their intended root.
        for sample in ["foo.db", "..", "../../Windows/evil", "data/foo.db"] {
            let result = Storage::confine_destructive_path_for_callers(Path::new(sample));
            assert!(
                result.is_err(),
                "TM-006 leak: confine accepted a relative path: {sample}"
            );
        }
    }

    #[test]
    fn tm006_confine_accepts_path_under_temp() {
        // Positive: a fresh path under the OS temp directory must
        // round-trip through the helper. The path doesn't need to
        // exist — the helper walks up to an existing ancestor when
        // the leaf is missing.
        let temp = std::env::temp_dir();
        let candidate = temp.join(format!("omni-tm006-confine-{}.db", unique_suffix()));
        let result = Storage::confine_destructive_path_for_callers(&candidate);
        assert!(
            result.is_ok(),
            "TM-006 false positive: confine rejected legitimate temp path \
             '{}': {:?}",
            candidate.display(),
            result
        );
    }

    /// An old build opened on this database re-creates the pre-MLS `group_ops`; the next start drops
    /// that journal and keeps the MLS groups' rows (it dropped them, stand 2026-10-11). A database a
    /// newer build has stamped is refused untouched.
    #[test]
    fn an_old_builds_group_ops_does_not_cost_the_groups_and_a_newer_schema_is_refused() {
        let db_path = std::env::temp_dir()
            .join("omni-groups-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let account = storage.create_local_account("Alice", "Laptop", 1_000).expect("account");
        let profile_id = account.local_profile_summary.as_ref().expect("profile summary").profile_id.clone();
        storage.set_active_profile_id(Some(&profile_id)).unwrap();
        let group_id = "00112233445566778899aabbccddeeff";
        storage
            .upsert_group(&super::GroupRecord {
                group_id: group_id.to_string(),
                name: "Друзья".to_string(),
                my_member_pub: "alice-pub".to_string(),
                my_status: "member".to_string(),
                dialog_key: super::group_dialog_key(group_id),
                created_at_unix_ms: 1_000,
                updated_at_unix_ms: 1_000,
            })
            .unwrap();
        storage
            .connection
            .execute_batch("CREATE TABLE group_ops (op BLOB); CREATE TABLE group_sender_keys (k BLOB);")
            .unwrap();

        storage.migrate().expect("migrations should succeed");
        assert!(storage.get_group(group_id).unwrap().is_some(), "the MLS group's row stays");
        assert!(!storage.table_exists("group_ops").unwrap(), "the old journal goes");

        // Negative half: a database from a newer build is not migrated over.
        storage
            .connection
            .execute("UPDATE app_meta SET value = ?1 WHERE key = 'schema_version'", [(super::SCHEMA_VERSION + 1).to_string()])
            .unwrap();
        storage.connection.execute_batch("CREATE TABLE group_ops (op BLOB);").unwrap();
        let refused = storage.migrate().expect_err("a newer schema is refused");
        assert!(refused.to_string().contains("newer Omni"), "{refused}");
        assert!(storage.table_exists("group_ops").unwrap(), "nothing was changed");
    }

    #[test]
    fn group_rows_members_invites_and_epoch_bookkeeping_roundtrip() {
        let db_path = std::env::temp_dir()
            .join("omni-groups-tests")
            .join(format!("{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        let account = storage
            .create_local_account("Alice", "Laptop", 1_000)
            .expect("account");
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary")
            .profile_id
            .clone();
        storage.set_active_profile_id(Some(&profile_id)).unwrap();

        let group_id = "00112233445566778899aabbccddeeff";
        let dialog_key = super::group_dialog_key(group_id);
        let record = super::GroupRecord {
            group_id: group_id.to_string(),
            name: "Друзья".to_string(),
            my_member_pub: "alice-pub".to_string(),
            my_status: "member".to_string(),
            dialog_key: dialog_key.clone(),
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
        };
        storage.upsert_group(&record).unwrap();
        assert_eq!(storage.get_group(group_id).unwrap().as_ref(), Some(&record));
        assert_eq!(storage.get_group_by_dialog_key(&dialog_key).unwrap().as_ref(), Some(&record));
        assert_eq!(storage.next_group_feed_seq(group_id).unwrap(), 1);
        assert_eq!(storage.next_group_feed_seq(group_id).unwrap(), 2);

        // Member projection; a member who said "leaving" stays marked across a refresh.
        let member = |pub_: &str, role: &str| super::GroupMemberRecord {
            member_pub: pub_.to_string(),
            account_id: format!("acct-{pub_}"),
            display_name: pub_.to_string(),
            role: role.to_string(),
            status: "member".to_string(),
            node_hint_hex: "00".repeat(20),
        };
        storage
            .replace_group_members(group_id, &[member("alice-pub", "admin"), member("bob-pub", "member")])
            .unwrap();
        storage.set_group_member_status(group_id, "bob-pub", "left").unwrap();
        storage
            .replace_group_members(group_id, &[member("alice-pub", "admin"), member("bob-pub", "member")])
            .unwrap();
        let members = storage.list_group_members(group_id).unwrap();
        assert_eq!(members.iter().find(|m| m.member_pub == "bob-pub").unwrap().status, "left");
        // Negative half: once MLS no longer has him, he is gone from the projection.
        storage.replace_group_members(group_id, &[member("alice-pub", "admin")]).unwrap();
        assert_eq!(storage.list_group_members(group_id).unwrap().len(), 1);

        // Invitations: pending and accepted ones are listed, a declined one is not.
        let invite = super::GroupInviteRecord {
            group_id: "ffeeddccbbaa99887766554433221100".to_string(),
            name: "Работа".to_string(),
            inviter_name: "Bob".to_string(),
            inviter_dialog_key: "dialog:bob".to_string(),
            status: "pending".to_string(),
            received_at_unix_ms: 2_000,
        };
        storage.upsert_group_invite(&invite).unwrap();
        assert_eq!(storage.list_open_group_invites().unwrap(), vec![invite.clone()]);
        let accepted = super::GroupInviteRecord { status: "accepted".to_string(), ..invite.clone() };
        storage.upsert_group_invite(&accepted).unwrap();
        assert_eq!(storage.list_open_group_invites().unwrap(), vec![accepted]);
        storage
            .upsert_group_invite(&super::GroupInviteRecord { status: "declined".to_string(), ..invite.clone() })
            .unwrap();
        assert!(storage.list_open_group_invites().unwrap().is_empty());
        assert_eq!(storage.get_group_invite(&invite.group_id).unwrap().unwrap().status, "declined");

        // Pending inbound: taken once, oldest epoch first.
        storage.push_group_pending_inbound(group_id, 5, b"e5", 10).unwrap();
        storage.push_group_pending_inbound(group_id, 4, b"e4", 11).unwrap();
        assert_eq!(storage.take_group_pending_inbound(group_id).unwrap(), vec![b"e4".to_vec(), b"e5".to_vec()]);
        assert!(storage.take_group_pending_inbound(group_id).unwrap().is_empty());

        // Outbox: what we sent at an epoch or later; own changes taken once.
        storage.record_group_outbox(group_id, 3, "m1", "{\"a\":1}", 1).unwrap();
        storage.record_group_outbox(group_id, 4, "m2", "{\"a\":2}", 2).unwrap();
        assert_eq!(storage.list_group_outbox_since(group_id, 4).unwrap(), vec!["{\"a\":2}".to_string()]);
        storage.record_group_own_change(group_id, 4, "{\"add\":1}").unwrap();
        assert_eq!(storage.take_group_own_change(group_id, 4).unwrap().as_deref(), Some("{\"add\":1}"));
        assert_eq!(storage.take_group_own_change(group_id, 4).unwrap(), None);

        // Who wrote what.
        let meta = super::GroupMessageMeta {
            message_id: "g-1".to_string(),
            sender_pub: Some("bob-pub".to_string()),
            sender_name: "Bob".to_string(),
            kind: "text".to_string(),
        };
        storage.insert_group_message_meta(group_id, &meta).unwrap();
        assert_eq!(storage.list_group_message_meta(group_id).unwrap(), vec![meta]);

        // The MLS storage key is set once and kept.
        assert_eq!(storage.get_mls_storage_key_protected().unwrap(), None);
        storage.set_mls_storage_key_protected("first").unwrap();
        storage.set_mls_storage_key_protected("second").unwrap();
        assert_eq!(storage.get_mls_storage_key_protected().unwrap().as_deref(), Some("first"));

        // Feed outbox: oldest first, gone once delivered.
        storage.enqueue_group_feed(group_id, b"c1", 1).unwrap();
        storage.enqueue_group_feed(group_id, b"c2", 2).unwrap();
        let queued = storage.list_group_feed_outbox(10).unwrap();
        assert_eq!(queued.iter().map(|q| q.2.clone()).collect::<Vec<_>>(), vec![b"c1".to_vec(), b"c2".to_vec()]);
        storage.delete_group_feed_outbox(queued[0].0).unwrap();
        assert_eq!(storage.list_group_feed_outbox(10).unwrap().len(), 1);

        // Sent invitations are consumed once; an accept from someone never invited finds none.
        storage.record_group_sent_invite(group_id, "dialog:bob", 1).unwrap();
        assert!(!storage.take_group_sent_invite(group_id, "dialog:mallory").unwrap());
        assert!(storage.take_group_sent_invite(group_id, "dialog:bob").unwrap());
        assert!(!storage.take_group_sent_invite(group_id, "dialog:bob").unwrap());

        storage.delete_group_rows(group_id).unwrap();
        assert!(storage.get_group(group_id).unwrap().is_none());
        assert!(storage.list_group_members(group_id).unwrap().is_empty());
    }

    fn tiny_png_bytes() -> &'static [u8] {
        &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x01, 0x73, 0x52, 0x47, 0x42, 0x00,
            0xAE, 0xCE, 0x1C, 0xE9, 0x00, 0x00, 0x00, 0x04, 0x67, 0x41, 0x4D, 0x41, 0x00, 0x00,
            0xB1, 0x8F, 0x0B, 0xFC, 0x61, 0x05, 0x00, 0x00, 0x00, 0x09, 0x70, 0x48, 0x59, 0x73,
            0x00, 0x00, 0x0E, 0xC3, 0x00, 0x00, 0x0E, 0xC3, 0x01, 0xC7, 0x6F, 0xA8, 0x64, 0x00,
            0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x18, 0x57, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
            0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0xA6, 0x5C, 0x9B, 0x5D, 0x00, 0x00, 0x00, 0x00,
            0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// Remote access per contact (remote-access-for-contacts.md): our grant and the contact's are
    /// separate, a revocation is a state of its own (the contact must still hear it), and an older
    /// message from the contact never undoes a newer one. Negative halves: an unknown contact has no
    /// access either way, and a stale "granted" loses to a newer revocation.
    #[test]
    fn remote_access_keeps_both_directions_and_never_goes_back_in_time() {
        use omni_types::RemoteAccessMode;
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("remote-access-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).expect("storage should open");
        storage.migrate().expect("migrations should succeed");
        storage.create_local_account("Alice", "Office PC", 1_000).expect("account");
        let bob = storage
            .add_contact(
                &AddContactInput {
                    petname: "Bob".to_string(),
                    remote_account_id: Some(AccountId("acct-bob".to_string())),
                    public_display_name: Some("Bob".to_string()),
                    remote_instance_id: Some("inst-bob".to_string()),
                    primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                    initial_observation: None,
                },
                1_100,
            )
            .expect("contact");

        let none = storage.get_contact_remote_access(&bob.contact_id).unwrap();
        assert_eq!((none.granted_mode, none.received_mode), (None, None), "nothing until granted");

        let granted = storage
            .set_contact_remote_access_granted(&bob.contact_id, Some(RemoteAccessMode::Ask), 2_000)
            .unwrap();
        assert_eq!((granted.granted_mode, granted.received_mode), (Some(RemoteAccessMode::Ask), None));
        let revoked = storage.set_contact_remote_access_granted(&bob.contact_id, None, 3_000).unwrap();
        assert_eq!((revoked.granted_mode, revoked.granted_changed_at_unix_ms), (None, Some(3_000)), "a revocation is remembered");

        storage
            .set_contact_remote_access_received(&bob.contact_id, None, 5_000)
            .unwrap();
        let stale = storage
            .set_contact_remote_access_received(&bob.contact_id, Some(RemoteAccessMode::Unattended), 4_000)
            .unwrap();
        assert_eq!(stale.received_mode, None, "an older grant does not undo a newer revocation");
        let newer = storage
            .set_contact_remote_access_received(&bob.contact_id, Some(RemoteAccessMode::Unattended), 6_000)
            .unwrap();
        assert_eq!(newer.received_mode, Some(RemoteAccessMode::Unattended));
        assert_eq!(newer.granted_mode, None, "the contact's grant does not touch ours");

        let listed = storage.list_contact_remote_access().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].contact_id, bob.contact_id);
        let stranger = storage.get_contact_remote_access(&ContactId("nobody".into())).unwrap();
        assert_eq!((stranger.granted_mode, stranger.received_mode), (None, None));
    }

    // docs/decisions/multi-device-equal-nodes.md — the account key chain at rest.
    fn open_account_key_storage(label: &str) -> Storage {
        let db_path = std::env::temp_dir()
            .join("omni-account-key-tests")
            .join(format!("{label}-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage
    }

    fn restore_with_crypto(storage: &Storage, mnemonic: &str, device_label: &str) -> AccountId {
        let account = storage
            .restore_local_account_from_mnemonic("Alice", mnemonic, device_label, 2_000)
            .unwrap();
        storage.ensure_local_crypto_foundation(2_100).unwrap();
        account.descriptor.account_id
    }

    #[test]
    fn a_new_profile_holds_its_key_chain_and_a_certificate_for_this_device() {
        let storage = open_account_key_storage("new-profile");
        let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();

        let keys = storage.get_account_keys(&account.descriptor.account_id).unwrap().unwrap();
        assert_eq!(keys.master_public_key.account_id(), account.descriptor.account_id);
        assert_eq!(keys.working_certificate.version, 1);
        omni_crypto::verify_working_key_certificate(&keys.working_certificate).unwrap();

        let chain = storage.get_local_device_certificate_chain().unwrap().unwrap();
        omni_crypto::verify_device_certificate_chain(&chain).unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        let identity = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();
        assert_eq!(chain.device.device_id, device.device_id);
        // The certified key is the one the mesh node address comes from.
        assert_eq!(chain.device.identity_signing_public_key, identity.signing_public_key);

        // Idempotent: a second pass neither re-issues nor duplicates.
        storage.ensure_local_crypto_foundation(1_200).unwrap();
        let devices = storage.list_account_devices(&account.descriptor.account_id).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].chain, chain);
        assert_eq!(devices[0].device_label, "Office PC");
    }

    #[test]
    fn the_phrase_restores_the_same_account_and_working_key_as_another_device() {
        let root_secret = omni_identity::AccountRootSecret(
            "ababababababababababababababababcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".to_string(),
        );
        let mnemonic = omni_identity::derive_recovery_phrase_v2(&root_secret).unwrap().mnemonic;
        let laptop = open_account_key_storage("restore-laptop");
        let desktop = open_account_key_storage("restore-desktop");
        let laptop_account = restore_with_crypto(&laptop, &mnemonic, "Laptop");
        let desktop_account = restore_with_crypto(&desktop, &mnemonic, "Desktop");

        assert_eq!(laptop_account, desktop_account);
        let laptop_keys = laptop.get_account_keys(&laptop_account).unwrap().unwrap();
        let desktop_keys = desktop.get_account_keys(&desktop_account).unwrap().unwrap();
        assert_eq!(laptop_keys.master_public_key, desktop_keys.master_public_key);
        assert_eq!(
            laptop_keys.working_certificate.working_public_key,
            desktop_keys.working_certificate.working_public_key
        );
        // Two devices of one account, each with its own node identity.
        let laptop_chain = laptop.get_local_device_certificate_chain().unwrap().unwrap();
        let desktop_chain = desktop.get_local_device_certificate_chain().unwrap().unwrap();
        assert_ne!(laptop_chain.device.device_id, desktop_chain.device.device_id);
        assert_ne!(
            laptop_chain.device.identity_signing_public_key,
            desktop_chain.device.identity_signing_public_key
        );
        assert_eq!(laptop_chain.account_id(), desktop_chain.account_id());
    }

    #[test]
    fn deleting_the_profile_removes_its_key_chain() {
        let storage = open_account_key_storage("delete-keys");
        let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        let profile_id = storage.get_active_profile_id().unwrap().unwrap();

        storage.delete_local_profile(&profile_id).unwrap();

        assert!(storage.get_account_keys(&account.descriptor.account_id).unwrap().is_none());
        assert!(storage
            .list_account_devices(&account.descriptor.account_id)
            .unwrap()
            .is_empty());
    }

    const REVOCATION_TEST_ROOT: &str =
        "ababababababababababababababababcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

    /// Two devices of one account, restored from the same phrase, each knowing the other's chain.
    fn laptop_and_desktop_that_know_each_other(name: &str) -> (Storage, Storage, DeviceId, DeviceId) {
        let root_secret = omni_identity::AccountRootSecret(REVOCATION_TEST_ROOT.to_string());
        let mnemonic = omni_identity::derive_recovery_phrase_v2(&root_secret).unwrap().mnemonic;
        let laptop = open_account_key_storage(&format!("{name}-laptop"));
        let desktop = open_account_key_storage(&format!("{name}-desktop"));
        restore_with_crypto(&laptop, &mnemonic, "Laptop");
        restore_with_crypto(&desktop, &mnemonic, "Desktop");
        let laptop_chain = laptop.get_local_device_certificate_chain().unwrap().unwrap();
        let desktop_chain = desktop.get_local_device_certificate_chain().unwrap().unwrap();
        laptop
            .record_own_device(&desktop_chain, "Desktop", "dialog:instance:desktop", 2_000)
            .unwrap();
        desktop
            .record_own_device(&laptop_chain, "Laptop", "dialog:instance:laptop", 2_000)
            .unwrap();
        (
            laptop,
            desktop,
            laptop_chain.device.device_id,
            desktop_chain.device.device_id,
        )
    }

    #[test]
    fn a_revoked_device_is_dropped_everywhere_and_the_revoked_device_learns_it_was_taken_back() {
        let (laptop, desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke");
        assert_eq!(laptop.list_own_device_dialogs().unwrap().len(), 1);

        let revocation = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();
        let outcome = laptop.apply_device_revocation(&revocation, 3_000).unwrap();
        assert!(outcome.newly_applied);
        assert!(!outcome.targets_local_device);
        assert!(laptop.list_own_device_dialogs().unwrap().is_empty());
        assert!(laptop
            .is_device_revoked(&revocation.account_id, &desktop_id)
            .unwrap());
        // Hearing it twice changes nothing and is not worth passing on again.
        assert!(!laptop.apply_device_revocation(&revocation, 3_100).unwrap().newly_applied);

        // The revoked device receives the same statement and finds it is the one named.
        let outcome = desktop.apply_device_revocation(&revocation, 3_200).unwrap();
        assert!(outcome.newly_applied);
        assert!(outcome.targets_local_device);

        assert_eq!(
            laptop.list_device_revocations(&revocation.account_id).unwrap(),
            vec![revocation]
        );
    }

    #[test]
    fn a_password_folder_signs_a_revocation_only_with_its_password() {
        let (laptop, _desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-password");
        let profile_id = laptop.get_active_profile_id().unwrap().unwrap();
        laptop.set_local_profile_password(&profile_id, "right password").unwrap();

        // The working key is under the password now: nothing signs without it.
        laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap_err();
        laptop
            .issue_local_device_revocation_with_password(&desktop_id, "wrong password", 3_000)
            .unwrap_err();

        let revocation = laptop
            .issue_local_device_revocation_with_password(&desktop_id, "right password", 3_000)
            .unwrap();
        omni_crypto::verify_device_revocation(&revocation).unwrap();
        assert!(laptop.apply_device_revocation(&revocation, 3_000).unwrap().newly_applied);
    }

    #[test]
    fn moving_to_a_new_working_key_keeps_the_chosen_devices_and_drops_the_rest() {
        let (laptop, desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("key-move-keep");
        let account_id = laptop.get_local_account().unwrap().unwrap().descriptor.account_id;
        assert_eq!(laptop.current_working_key_version(&account_id).unwrap(), Some(1));

        let (key_move, seed) = laptop
            .move_local_account_to_new_working_key(std::slice::from_ref(&desktop_id), None, None, 5_000)
            .unwrap();
        assert_eq!(key_move.working.version, 2);
        assert_eq!(laptop.current_working_key_version(&account_id).unwrap(), Some(2));
        assert_eq!(laptop.get_local_device_certificate_chain().unwrap().unwrap().working.version, 2);
        // The kept device's certificate was re-issued under the new key.
        let on_laptop = laptop
            .list_account_devices(&account_id)
            .unwrap()
            .into_iter()
            .find(|record| record.chain.device.device_id == desktop_id)
            .unwrap();
        assert_eq!(on_laptop.chain.working.version, 2);
        assert!(on_laptop.revoked_at_unix_ms.is_none());

        // The desktop is handed the move and the seed: it holds the new key and its new certificate.
        let outcome = desktop.apply_account_key_move(&key_move, Some(seed), None, 5_100).unwrap();
        assert!(outcome.newly_applied && outcome.own_account && outcome.this_device_kept);
        assert_eq!(desktop.current_working_key_version(&account_id).unwrap(), Some(2));
        assert_eq!(desktop.get_local_device_certificate_chain().unwrap().unwrap().working.version, 2);
        // ...and can sign with it: a revocation issued by the desktop verifies under working key 2.
        let laptop_id = laptop.get_local_device().unwrap().unwrap().device_id;
        let revocation = desktop.issue_local_device_revocation(&laptop_id, None, 5_200).unwrap();
        assert_eq!(revocation.working.version, 2);
        omni_crypto::verify_device_revocation(&revocation).unwrap();
        // Hearing the same move twice changes nothing.
        assert!(!desktop.apply_account_key_move(&key_move, Some(seed), None, 5_300).unwrap().newly_applied);
    }

    #[test]
    fn a_device_the_move_does_not_keep_is_out_and_cannot_come_back_with_its_old_certificate() {
        let (laptop, desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("key-move-drop");
        let account_id = laptop.get_local_account().unwrap().unwrap().descriptor.account_id;
        let old_chain = desktop.get_local_device_certificate_chain().unwrap().unwrap();

        let (key_move, _seed) = laptop
            .move_local_account_to_new_working_key(&[], None, None, 5_000)
            .unwrap();
        assert!(laptop.is_device_revoked(&account_id, &desktop_id).unwrap());
        assert!(laptop.list_own_device_dialogs().unwrap().is_empty());
        // A move is not a signed revocation: nothing of it is passed on as one.
        assert!(laptop.list_device_revocations(&account_id).unwrap().is_empty());
        // The thief's device presents its old certificate again: refused.
        laptop
            .record_own_device(&old_chain, "Desktop", "dialog:instance:desktop", 6_000)
            .unwrap_err();
        // The device left out learns it, and finds itself not kept.
        let outcome = desktop.apply_account_key_move(&key_move, None, None, 5_100).unwrap();
        assert!(outcome.newly_applied && !outcome.this_device_kept);
        // A revocation signed with the stolen (old) key no longer counts.
        let stolen_key_revocation = desktop.issue_local_device_revocation(
            &laptop.get_local_device().unwrap().unwrap().device_id,
            None,
            4_000,
        );
        if let Ok(revocation) = stolen_key_revocation {
            laptop.apply_device_revocation(&revocation, 6_100).unwrap_err();
        }
    }

    #[test]
    fn a_move_needs_the_phrase_that_belongs_to_the_account() {
        let (laptop, _desktop, _laptop_id, _desktop_id) =
            laptop_and_desktop_that_know_each_other("key-move-phrase");
        let other_root = omni_identity::AccountRootSecret(
            "1212121212121212121212121212121234343434343434343434343434343434".to_string(),
        );
        let other_phrase = omni_identity::derive_recovery_phrase_v2(&other_root).unwrap().mnemonic;
        laptop
            .move_local_account_to_new_working_key(&[], Some(&other_phrase), None, 5_000)
            .unwrap_err();
    }

    fn certified_remote_bundle(root_byte: &str, version: u64, device: &str) -> omni_crypto::PrekeyBundle {
        let root = omni_identity::AccountRootSecret(root_byte.to_string());
        let master = omni_crypto::AccountMasterKey::derive(&root).unwrap();
        let working = master.working_key(version).unwrap();
        let certificate = omni_crypto::issue_working_key_certificate(&master, &working, 1_000);
        let device_id = DeviceId(device.to_string());
        let hierarchy = omni_crypto::generate_local_key_hierarchy(
            &master.public_key().account_id(),
            &device_id,
            1_000,
            1,
        )
        .unwrap();
        let mut bundle = omni_crypto::build_prekey_bundle(
            &hierarchy.identity_key,
            &hierarchy.device_key,
            &hierarchy.signed_prekey,
            hierarchy.one_time_prekeys.first(),
            1_000,
        )
        .unwrap();
        bundle.device_certificate_chain = Some(
            omni_crypto::issue_device_certificate(
                &certificate,
                &working,
                &device_id,
                bundle.identity_key,
                bundle.identity_signing_key,
                bundle.device_key,
                1_000,
            )
            .unwrap(),
        );
        bundle
    }

    #[test]
    fn a_contact_that_moved_keeps_its_re_certified_devices_and_loses_the_others() {
        const CONTACT_ROOT: &str = "5656565656565656565656565656565678787878787878787878787878787878";
        let bob = open_account_key_storage("key-move-contact");
        bob.create_local_account("Bob", "Bob PC", 1_000).unwrap();
        let kept = certified_remote_bundle(CONTACT_ROOT, 1, "dev-kept");
        let left = certified_remote_bundle(CONTACT_ROOT, 1, "dev-left");
        let account_id = kept.account_id.clone();
        bob.record_remote_prekey_bundle(&kept, None, Some("Kept"), 2_000).unwrap();
        bob.record_remote_prekey_bundle(&left, None, Some("Left"), 2_000).unwrap();
        assert!(!bob.is_device_revoked(&account_id, &kept.device_id).unwrap());

        // The contact moves: working key 2 certifies "kept" only.
        let root = omni_identity::AccountRootSecret(CONTACT_ROOT.to_string());
        let master = omni_crypto::AccountMasterKey::derive(&root).unwrap();
        let key_move = omni_crypto::issue_account_key_move(
            &master,
            &master.working_key(2).unwrap(),
            &[kept.device_certificate_chain.clone().unwrap()],
            3_000,
        )
        .unwrap();
        let outcome = bob.apply_account_key_move(&key_move, None, None, 3_100).unwrap();
        assert!(outcome.newly_applied && !outcome.own_account);
        assert_eq!(outcome.devices_superseded, 1);
        assert_eq!(bob.current_working_key_version(&account_id).unwrap(), Some(2));
        assert!(!bob.is_device_revoked(&account_id, &kept.device_id).unwrap());
        assert!(bob.is_device_revoked(&account_id, &left.device_id).unwrap());
        // Only the kept device is a route any more.
        let listed: Vec<_> = bob
            .list_remote_prekey_bundles_for_account(&account_id)
            .unwrap()
            .into_iter()
            .map(|bundle| bundle.device_id)
            .collect();
        assert_eq!(listed, vec![kept.device_id.clone()]);

        // The device left behind keeps presenting its old certificate: refused.
        bob.record_remote_prekey_bundle(&left, None, Some("Left"), 4_000).unwrap_err();
        // A device certified under the new key is welcome even though the move did not name it.
        let newcomer = certified_remote_bundle(CONTACT_ROOT, 2, "dev-new");
        bob.record_remote_prekey_bundle(&newcomer, None, Some("New"), 4_100).unwrap();
        assert!(!bob.is_device_revoked(&account_id, &newcomer.device_id).unwrap());
    }

    #[test]
    fn a_newer_certificate_seen_without_the_move_message_still_moves_the_account() {
        const CONTACT_ROOT: &str = "9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9abcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbc";
        let bob = open_account_key_storage("key-move-observed");
        bob.create_local_account("Bob", "Bob PC", 1_000).unwrap();
        let old = certified_remote_bundle(CONTACT_ROOT, 1, "dev-old");
        let account_id = old.account_id.clone();
        bob.record_remote_prekey_bundle(&old, None, Some("Old"), 2_000).unwrap();

        // Bob was away for the move; the first thing he sees is a device with the new certificate.
        let fresh = certified_remote_bundle(CONTACT_ROOT, 2, "dev-fresh");
        bob.record_remote_prekey_bundle(&fresh, None, Some("Fresh"), 3_000).unwrap();
        assert_eq!(bob.current_working_key_version(&account_id).unwrap(), Some(2));
        assert!(bob.is_device_revoked(&account_id, &old.device_id).unwrap());
        assert!(!bob.is_device_revoked(&account_id, &fresh.device_id).unwrap());
    }

    #[test]
    fn a_device_that_appears_is_new_until_the_owner_has_seen_it() {
        let (laptop, _desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("device-new");
        let account_id = laptop.get_local_account().unwrap().unwrap().descriptor.account_id;
        let acknowledged = |storage: &Storage| {
            storage
                .list_account_devices(&account_id)
                .unwrap()
                .into_iter()
                .find(|record| record.chain.device.device_id == desktop_id)
                .map(|record| record.acknowledged_at_unix_ms)
        };
        // The other device just showed up: nobody has told the owner yet.
        assert_eq!(acknowledged(&laptop), Some(None));

        laptop.acknowledge_account_devices(5_000).unwrap();
        assert_eq!(acknowledged(&laptop), Some(Some(5_000)));
        // Acknowledging again does not move the time it was seen.
        laptop.acknowledge_account_devices(9_000).unwrap();
        assert_eq!(acknowledged(&laptop), Some(Some(5_000)));
    }

    #[test]
    fn only_a_revoked_device_leaves_the_list_and_it_stays_revoked() {
        let (laptop, _desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("device-hide");
        let account_id = laptop.get_local_account().unwrap().unwrap().descriptor.account_id;
        let hidden = |storage: &Storage| {
            storage
                .list_account_devices(&account_id)
                .unwrap()
                .into_iter()
                .find(|record| record.chain.device.device_id == desktop_id)
                .map(|record| record.hidden_at_unix_ms)
        };
        // A device still in the account cannot be taken out of the list, one by one or all at once.
        assert_eq!(laptop.hide_revoked_account_devices(Some(&desktop_id), 2_000).unwrap(), 0);
        assert_eq!(laptop.hide_revoked_account_devices(None, 2_000).unwrap(), 0);
        assert_eq!(hidden(&laptop), Some(None));

        let revocation = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();
        laptop.apply_device_revocation(&revocation, 3_000).unwrap();
        assert_eq!(laptop.hide_revoked_account_devices(Some(&desktop_id), 4_000).unwrap(), 1);
        assert_eq!(hidden(&laptop), Some(Some(4_000)));
        // Out of the list, not out of the revocation.
        assert!(laptop.is_device_revoked(&account_id, &desktop_id).unwrap());
        assert_eq!(laptop.hide_revoked_account_devices(None, 5_000).unwrap(), 0);
    }

    #[test]
    fn a_device_that_was_taken_back_erases_its_profile_and_only_then() {
        let (laptop, desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-wipe");
        // Nothing revoked: neither device erases anything.
        assert!(!desktop.wipe_profile_if_taken_back().unwrap());
        assert!(desktop.get_local_account().unwrap().is_some());

        let revocation = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();
        laptop.apply_device_revocation(&revocation, 3_000).unwrap();
        // The device that took it back keeps its profile.
        assert!(!laptop.wipe_profile_if_taken_back().unwrap());
        assert!(laptop.get_local_account().unwrap().is_some());

        let outcome = desktop.apply_device_revocation(&revocation, 3_200).unwrap();
        assert!(outcome.targets_local_device);
        assert!(desktop.wipe_profile_if_taken_back().unwrap());
        assert!(desktop.get_local_account().unwrap().is_none());
        assert!(desktop.get_active_profile_id().unwrap().is_none());
    }

    /// An own device's «last heard» never moves back; one never heard counts from when we first
    /// learned of it (its silence survives a restart — the rendezvous sweep seeds from this).
    #[test]
    fn an_own_devices_last_heard_only_moves_forward_and_starts_at_first_seen() {
        let (laptop, _desktop, _laptop_id, desktop_id) = laptop_and_desktop_that_know_each_other("own-heard");
        let account = laptop.get_local_account().unwrap().unwrap().descriptor.account_id;
        let first_seen = *laptop.own_devices_last_heard(&account).unwrap().get(&desktop_id).expect("known device");
        laptop.note_own_device_heard(&account, &desktop_id, first_seen + 10_000).unwrap();
        laptop.note_own_device_heard(&account, &desktop_id, first_seen + 5_000).unwrap();
        assert_eq!(laptop.own_devices_last_heard(&account).unwrap().get(&desktop_id), Some(&(first_seen + 10_000)));
    }

    #[test]
    fn a_revoked_device_stays_revoked_when_its_certificate_shows_up_again() {
        let (laptop, desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-sticky");
        let revocation = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();
        laptop.apply_device_revocation(&revocation, 3_000).unwrap();

        let chain = desktop.get_local_device_certificate_chain().unwrap().unwrap();
        laptop
            .record_own_device(&chain, "Desktop", "dialog:instance:desktop", 4_000)
            .unwrap();
        assert!(laptop.list_own_device_dialogs().unwrap().is_empty());
    }

    #[test]
    fn a_device_cannot_revoke_itself_or_a_device_the_account_does_not_have() {
        let (laptop, _desktop, laptop_id, _desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-refuse");
        laptop.issue_local_device_revocation(&laptop_id, None, 3_000).unwrap_err();
        laptop
            .issue_local_device_revocation(&DeviceId("dev-nobody".to_string()), None, 3_000)
            .unwrap_err();
    }

    #[test]
    fn a_revocation_signed_by_a_superseded_working_key_is_refused() {
        let (laptop, _desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-old-key");
        let old = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();

        // The account moved to working key 2: a device certified by it is on record.
        let root_secret = omni_identity::AccountRootSecret(REVOCATION_TEST_ROOT.to_string());
        let master = omni_crypto::AccountMasterKey::derive(&root_secret).unwrap();
        let working = master.working_key(2).unwrap();
        let certificate = omni_crypto::issue_working_key_certificate(&master, &working, 3_500);
        let newer = omni_crypto::issue_device_certificate(
            &certificate,
            &working,
            &DeviceId("dev-phone".to_string()),
            omni_crypto::IdentityPublicKey([5; 32]),
            omni_crypto::IdentitySigningPublicKey([6; 32]),
            omni_crypto::DevicePublicKey([7; 32]),
            3_500,
        )
        .unwrap();
        laptop
            .record_own_device(&newer, "Phone", "dialog:instance:phone", 3_500)
            .unwrap();

        laptop.apply_device_revocation(&old, 4_000).unwrap_err();
        assert!(!laptop.is_device_revoked(&old.account_id, &desktop_id).unwrap());
    }

    #[test]
    fn a_forged_revocation_is_refused() {
        let (laptop, _desktop, _laptop_id, desktop_id) =
            laptop_and_desktop_that_know_each_other("revoke-forged");
        let mut forged = laptop.issue_local_device_revocation(&desktop_id, None, 3_000).unwrap();
        forged.device_id = DeviceId("dev-someone-else".to_string());
        laptop.apply_device_revocation(&forged, 3_100).unwrap_err();
    }

    fn working_key_scheme(storage: &Storage, account_id: &AccountId) -> String {
        storage
            .connection
            .query_row(
                "SELECT working_key_protection_scheme FROM account_keys WHERE account_id = ?1",
                rusqlite::params![account_id.0.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn the_working_key_follows_the_profile_password() {
        let storage = open_account_key_storage("password-working-key");
        let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        let account_id = account.descriptor.account_id.clone();
        let profile_id = storage.get_active_profile_id().unwrap().unwrap();
        let before = storage.get_account_keys(&account_id).unwrap().unwrap();
        assert_eq!(working_key_scheme(&storage, &account_id), "portable_plain_v1");

        storage.set_local_profile_password(&profile_id, "first secret").unwrap();
        assert_eq!(working_key_scheme(&storage, &account_id), "password_wrapped_v1");
        assert!(matches!(
            storage.read_working_key(&account_id, None).unwrap(),
            super::StoredWorkingKey::Locked
        ));
        // A second "set" cannot reach the key under the current password.
        storage
            .set_local_profile_password(&profile_id, "other secret")
            .unwrap_err();

        storage
            .change_local_profile_password(&profile_id, "first secret", "second secret")
            .unwrap();
        let second_key = storage.verify_profile_password(&profile_id, "second secret").unwrap();
        let super::StoredWorkingKey::Ready(working, _) =
            storage.read_working_key(&account_id, Some(&second_key)).unwrap()
        else {
            panic!("the new password must open the working key");
        };
        assert_eq!(working.public_key(), before.working_certificate.working_public_key);

        storage.remove_local_profile_password(&profile_id, "second secret").unwrap();
        assert_eq!(working_key_scheme(&storage, &account_id), "portable_plain_v1");
        assert!(matches!(
            storage.read_working_key(&account_id, None).unwrap(),
            super::StoredWorkingKey::Ready(..)
        ));
        assert_eq!(storage.get_account_keys(&account_id).unwrap().unwrap(), before);
    }

    // docs/decisions/multi-device-equal-nodes.md, 1.4 — a copied folder is a new device.
    fn new_profile_db(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("omni-device-binding-tests")
            .join(format!("{label}-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("omni.db");
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        storage.ensure_local_crypto_foundation(1_100).unwrap();
        db_path
    }

    fn reopen(db_path: &Path) -> Storage {
        let storage = Storage::open(db_path).unwrap();
        storage.migrate().unwrap();
        storage.ensure_local_crypto_foundation(5_000).unwrap();
        storage
    }

    fn device_and_node_key(storage: &Storage) -> (DeviceId, omni_crypto::IdentitySigningPublicKey) {
        let account = storage.get_local_account().unwrap().unwrap();
        let device = storage.get_local_device().unwrap().unwrap();
        let identity = storage
            .get_local_identity_key(&account.descriptor.account_id)
            .unwrap()
            .unwrap();
        (device.device_id, identity.signing_public_key)
    }

    #[test]
    fn reopening_the_profile_in_place_keeps_the_device() {
        let db_path = new_profile_db("in-place");
        let before = device_and_node_key(&reopen(&db_path));
        let after = device_and_node_key(&reopen(&db_path));
        assert_eq!(before, after);
    }

    #[test]
    fn a_copied_folder_becomes_a_new_device_of_the_same_account() {
        let original_path = new_profile_db("copy-original");
        let original = device_and_node_key(&reopen(&original_path));
        let original_instance = reopen(&original_path).get_or_create_local_instance_id().unwrap();
        let copy_dir = original_path.parent().unwrap().with_extension("copy");
        fs::create_dir_all(&copy_dir).unwrap();
        let copy_path = copy_dir.join("omni.db");
        fs::copy(&original_path, &copy_path).unwrap();
        // A folder copy carries the database's key file with it.
        fs::copy(key_file_of(&original_path), key_file_of(&copy_path)).unwrap();

        let copy = reopen(&copy_path);
        let copied = device_and_node_key(&copy);
        assert_ne!(copied.0, original.0, "the copy needs its own device id");
        assert_ne!(copied.1, original.1, "and its own node identity");
        assert_ne!(
            copy.get_or_create_local_instance_id().unwrap(),
            original_instance,
            "and its own installation id"
        );

        let account = copy.get_local_account().unwrap().unwrap();
        let devices = copy.list_account_devices(&account.descriptor.account_id).unwrap();
        assert_eq!(devices.len(), 2, "both devices belong to the account");
        for device in &devices {
            omni_crypto::verify_device_certificate_chain(&device.chain).unwrap();
        }
        assert_eq!(
            copy.get_local_device_certificate_chain().unwrap().unwrap().device.device_id,
            copied.0
        );
        // The original is untouched, and the copy is stable from now on.
        assert_eq!(device_and_node_key(&reopen(&original_path)), original);
        assert_eq!(device_and_node_key(&reopen(&copy_path)), copied);
    }

    #[test]
    fn device_keys_that_no_longer_open_mean_another_machine() {
        // DPAPI on another machine fails to open the device keys; a damaged blob fails the same way.
        let db_path = new_profile_db("foreign-keys");
        let before = device_and_node_key(&reopen(&db_path));
        {
            let storage = Storage::open(&db_path).unwrap();
            storage
                .connection
                .execute("UPDATE identity_keys SET signing_private_key_blob = '00'", [])
                .unwrap();
        }
        let storage = reopen(&db_path);
        let after = device_and_node_key(&storage);
        assert_ne!(after.0, before.0);
        assert_ne!(after.1, before.1);
        let identity = storage
            .get_local_identity_key(&storage.get_local_account().unwrap().unwrap().descriptor.account_id)
            .unwrap()
            .unwrap();
        omni_crypto::mesh_identity_signing_key_bytes(&identity).unwrap();
    }

    // docs/decisions/multi-device-equal-nodes.md, «Синхронизация контактов».
    fn contact_input(account: &str, petname: &str) -> AddContactInput {
        AddContactInput {
            petname: petname.to_string(),
            remote_account_id: Some(AccountId(account.to_string())),
            public_display_name: Some(petname.to_string()),
            remote_instance_id: None,
            primary_dialog_key: None,
            initial_observation: None,
        }
    }

    fn contact_sync_record(storage: &Storage, account: &str) -> crate::ContactSyncRecord {
        storage
            .get_contact_sync_record(&AccountId(account.to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_device_notes_its_contact_changes_as_versions_and_a_removal_as_a_tombstone() {
        let (laptop, _desktop, laptop_id, _desktop_id) = laptop_and_desktop_that_know_each_other("csync-local");
        let bob = laptop.add_contact(&contact_input("acc-bob", "Bob"), 3_000).unwrap();
        laptop
            .add_pending_contact(&contact_input("acc-carol", "Carol"), 3_000)
            .unwrap();

        assert_eq!(laptop.refresh_contact_sync_records(&laptop_id, 3_100).unwrap(), 1);
        // A pending request is this device's own business: not shared.
        assert!(laptop.get_contact_sync_record(&AccountId("acc-carol".into())).unwrap().is_none());
        let first = contact_sync_record(&laptop, "acc-bob");
        assert_eq!(
            (first.state, first.state_version, first.petname_version),
            (crate::ContactSyncState::Approved, 1, 1)
        );
        // Nothing changed: nothing new.
        assert_eq!(laptop.refresh_contact_sync_records(&laptop_id, 3_200).unwrap(), 0);

        laptop.rename_contact(&bob.contact_id, "Bobby", 3_300).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 3_300).unwrap();
        let renamed = contact_sync_record(&laptop, "acc-bob");
        assert_eq!((renamed.petname.as_str(), renamed.petname_version, renamed.state_version), ("Bobby", 2, 1));
        assert!(renamed.seq > first.seq, "a change goes to the end of the device's list");

        laptop.decline_contact(&bob.contact_id, 3_400).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 3_400).unwrap();
        assert_eq!(contact_sync_record(&laptop, "acc-bob").state, crate::ContactSyncState::Blocked);

        laptop.forget_contact(&bob.contact_id, 3_500).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 3_500).unwrap();
        let tombstone = contact_sync_record(&laptop, "acc-bob");
        assert_eq!((tombstone.state, tombstone.state_version), (crate::ContactSyncState::Removed, 3));
        assert_eq!(laptop.list_contact_sync_records_after(0, u64::MAX).unwrap().len(), 1, "the tombstone stays");
        assert_eq!(laptop.refresh_contact_sync_records(&laptop_id, 3_600).unwrap(), 0);
    }

    #[test]
    fn the_mailbox_takes_recent_blocks_and_removals_newest_first() {
        let (laptop, _desktop, laptop_id, _desktop_id) = laptop_and_desktop_that_know_each_other("csync-mailbox");
        let bob = laptop.add_contact(&contact_input("acc-bob", "Bob"), 3_000).unwrap();
        let carol = laptop.add_contact(&contact_input("acc-carol", "Carol"), 3_000).unwrap();
        laptop.add_contact(&contact_input("acc-dave", "Dave"), 3_000).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 3_000).unwrap();
        laptop.forget_contact(&bob.contact_id, 4_000).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 4_000).unwrap();
        laptop.decline_contact(&carol.contact_id, 5_000).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 5_000).unwrap();

        let accounts = |since, limit| {
            laptop
                .list_recent_contact_sync_blocks_and_removals(since, limit)
                .unwrap()
                .into_iter()
                .map(|record| record.account_id.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(accounts(0, 8), ["acc-carol", "acc-bob"], "an approval is not for the mailbox");
        assert_eq!(accounts(4_500, 8), ["acc-carol"], "older than the window");
        assert_eq!(accounts(0, 1), ["acc-carol"]);
    }

    #[test]
    fn a_device_that_was_away_cannot_bring_a_removed_contact_back() {
        let (laptop, desktop, laptop_id, desktop_id) = laptop_and_desktop_that_know_each_other("csync-away");
        // Both devices know Bob (version 1).
        let bob = laptop.add_contact(&contact_input("acc-bob", "Bob"), 3_000).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 3_000).unwrap();
        let shared = contact_sync_record(&laptop, "acc-bob");
        desktop.merge_contact_sync_record(&shared, 3_000).unwrap().unwrap();

        // The desktop is away; the laptop removes Bob.
        laptop.forget_contact(&bob.contact_id, 4_000).unwrap();
        laptop.refresh_contact_sync_records(&laptop_id, 4_000).unwrap();
        let tombstone = contact_sync_record(&laptop, "acc-bob");

        // Meanwhile the desktop renames Bob, twice: its name version is ahead.
        let mut renamed = contact_sync_record(&desktop, "acc-bob");
        renamed.petname = "Bobby".into();
        renamed.petname_version = 3;
        renamed.petname_writer = desktop_id.0.clone();

        // They meet. The desktop's older relationship does not revive Bob on the laptop…
        let merged = laptop.merge_contact_sync_record(&renamed, 5_000).unwrap().unwrap();
        assert_eq!(merged.state, None, "the relationship the desktop has is older");
        assert_eq!(contact_sync_record(&laptop, "acc-bob").state, crate::ContactSyncState::Removed);
        // …and the desktop takes the removal.
        let merged = desktop.merge_contact_sync_record(&tombstone, 5_000).unwrap().unwrap();
        assert_eq!(merged.state, Some(crate::ContactSyncState::Removed));
        // Hearing the same record twice changes nothing.
        assert!(desktop.merge_contact_sync_record(&tombstone, 5_100).unwrap().is_none());
    }

    #[test]
    fn a_removal_beats_an_approval_made_without_seeing_it() {
        let (laptop, desktop, laptop_id, desktop_id) = laptop_and_desktop_that_know_each_other("csync-tie");
        let base = crate::ContactSyncRecord {
            account_id: AccountId("acc-bob".into()),
            state: crate::ContactSyncState::Blocked,
            state_version: 1,
            state_writer: laptop_id.0.clone(),
            petname: "Bob".into(),
            petname_version: 1,
            petname_writer: laptop_id.0.clone(),
            display_name: None,
            seq: 0,
        };
        // Both start from version 1; the laptop removes, the desktop unblocks (approves).
        let removal = crate::ContactSyncRecord {
            state: crate::ContactSyncState::Removed,
            state_version: 2,
            ..base.clone()
        };
        let approval = crate::ContactSyncRecord {
            state: crate::ContactSyncState::Approved,
            state_version: 2,
            state_writer: desktop_id.0.clone(),
            ..base.clone()
        };
        for storage in [&laptop, &desktop] {
            storage.merge_contact_sync_record(&base, 3_000).unwrap();
        }
        laptop.merge_contact_sync_record(&removal, 3_100).unwrap();
        laptop.merge_contact_sync_record(&approval, 3_200).unwrap();
        desktop.merge_contact_sync_record(&approval, 3_100).unwrap();
        desktop.merge_contact_sync_record(&removal, 3_200).unwrap();
        // Whatever the order and whichever writer id sorts first, both end up removed.
        assert_eq!(contact_sync_record(&laptop, "acc-bob").state, crate::ContactSyncState::Removed);
        assert_eq!(contact_sync_record(&desktop, "acc-bob").state, crate::ContactSyncState::Removed);
        // A contact added again after the removal was seen is newer, and stands.
        let re_added = crate::ContactSyncRecord {
            state: crate::ContactSyncState::Approved,
            state_version: 3,
            ..base
        };
        desktop.merge_contact_sync_record(&re_added, 3_300).unwrap().unwrap();
        assert_eq!(contact_sync_record(&desktop, "acc-bob").state, crate::ContactSyncState::Approved);
    }

    #[test]
    fn each_device_acknowledges_how_far_it_has_come_and_never_moves_back() {
        let (laptop, _desktop, _laptop_id, desktop_id) = laptop_and_desktop_that_know_each_other("csync-mark");
        assert_eq!(laptop.contact_sync_mark(&desktop_id).unwrap(), crate::ContactSyncMark::default());
        laptop.record_contact_sync_sent(&desktop_id, 5, 1_000).unwrap();
        laptop.record_contact_sync_acked(&desktop_id, 5).unwrap();
        laptop.record_contact_sync_acked(&desktop_id, 3).unwrap();
        let mark = laptop.contact_sync_mark(&desktop_id).unwrap();
        assert_eq!((mark.acked_seq, mark.sent_through_seq, mark.sent_at_unix_ms), (5, 5, 1_000));
    }

    /// The queries the background tick runs four times a second read only the rows they need, through
    /// a partial index — not every contact or every attachment the profile ever had. Checked on the
    /// plan, because on a test-sized table a full scan is just as fast and nothing else would notice.
    #[test]
    fn the_per_tick_queries_use_their_partial_indexes() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-storage-tests")
            .join(format!("tick-plan-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        let plan = |sql: &str| -> String {
            let mut statement = storage.connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = statement.query_map([], |row| row.get::<_, String>(3)).unwrap();
            rows.map(|row| row.unwrap()).collect::<Vec<_>>().join(" | ")
        };
        let presence = plan(
            "SELECT c.contact_id FROM contact_presence p JOIN contacts c ON c.contact_id = p.contact_id AND c.profile_id = p.profile_id              WHERE p.profile_id = 'x' AND p.presence_state <> 'offline' AND COALESCE(p.last_activity_at_unix_ms, 0) <= 5                AND c.removed_at_unix_ms IS NULL AND c.relationship_state = 'approved'",
        );
        assert!(presence.contains("idx_contact_presence_not_offline"), "{presence}");
        let downloads = plan(
            "SELECT DISTINCT dialog_key FROM message_attachments WHERE profile_id = 'x'                AND remote_holder_available = 1 AND state IN ('requested', 'waiting_for_reconnect') ORDER BY dialog_key ASC",
        );
        assert!(downloads.contains("idx_message_attachments_pending_download"), "{downloads}");
        let per_dialog = plan(
            "SELECT message_id FROM message_attachments WHERE dialog_key = 'd' AND profile_id = 'x'                AND remote_holder_available = 1 AND state IN ('requested', 'waiting_for_reconnect')              ORDER BY updated_at_unix_ms ASC, message_id ASC",
        );
        assert!(per_dialog.contains("idx_message_attachments_pending_download"), "{per_dialog}");
        let by_account = plan("SELECT contact_id FROM contacts WHERE profile_id = 'x' AND remote_account_id = 'a'");
        assert!(by_account.contains("idx_contacts_account_profile"), "{by_account}");
        let by_dialog = plan("SELECT contact_id FROM contact_dialog_links WHERE dialog_key = 'd' AND profile_id = 'x'");
        assert!(by_dialog.contains("idx_contact_dialog_links_dialog"), "{by_dialog}");
        let bundles = plan("SELECT remote_device_id FROM remote_prekey_bundles WHERE remote_account_id = 'a' ORDER BY observed_at_unix_ms DESC");
        assert!(bundles.contains("idx_remote_prekey_bundles_account"), "{bundles}");
    }

    /// The chat list's "last message per chat": the newest of each chat (ties by id), and no chat
    /// without messages — read without walking the whole history.
    #[test]
    fn the_newest_message_of_each_chat_is_found_without_reading_the_history() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("latest-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let a = dialog_key_from_instance_id("peer-a");
        let b = dialog_key_from_instance_id("peer-b");
        let c = dialog_key_from_instance_id("peer-c");
        storage.insert_outgoing_message("a-1", &a, "a old", MessageStatus::Sent, 1_100).unwrap();
        storage.insert_incoming_message("a-2", &a, "a new", 1_300).unwrap();
        storage.insert_outgoing_message("a-0", &a, "a older", MessageStatus::Sent, 1_000).unwrap();
        storage.insert_incoming_message("b-1", &b, "b same time", 1_200).unwrap();
        storage.insert_incoming_message("b-2", &b, "b same time, later id", 1_200).unwrap();
        storage.insert_outgoing_message("c-1", &c, "c only", MessageStatus::Sent, 900).unwrap();

        let latest = storage.list_latest_messages_by_dialog().unwrap();
        let bodies: std::collections::BTreeMap<_, _> =
            latest.iter().map(|(dialog, message)| (dialog.clone(), message.body.clone())).collect();
        assert_eq!(bodies.len(), 3, "{bodies:?}");
        assert_eq!(bodies[&storage.chat_dialog_key_for(&a).unwrap()], "a new");
        assert_eq!(bodies[&storage.chat_dialog_key_for(&b).unwrap()], "b same time, later id");
        assert_eq!(bodies[&storage.chat_dialog_key_for(&c).unwrap()], "c only");

        let mut statement = storage
            .connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM messages WHERE id IN (WITH RECURSIVE dialogs(dialog_key) AS (                  SELECT (SELECT MIN(dialog_key) FROM messages WHERE profile_id = 'x' AND dialog_key > '') UNION ALL                  SELECT (SELECT MIN(dialog_key) FROM messages WHERE profile_id = 'x' AND dialog_key > dialogs.dialog_key)                  FROM dialogs WHERE dialogs.dialog_key IS NOT NULL) SELECT (SELECT id FROM messages                  WHERE profile_id = 'x' AND dialog_key = dialogs.dialog_key ORDER BY created_at_unix_ms DESC, id DESC LIMIT 1)                  FROM dialogs WHERE dialogs.dialog_key IS NOT NULL)",
            )
            .unwrap();
        let plan: Vec<String> =
            statement.query_map([], |row| row.get::<_, String>(3)).unwrap().map(|row| row.unwrap()).collect();
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN messages")),
            "no step reads every message: {plan:?}"
        );
    }

    /// The active circle the mesh keeps links with: contacts added, heard from or written with since
    /// the cut-off, or with something queued for them — and nobody else.
    #[test]
    fn the_active_circle_is_the_recently_added_heard_written_and_queued() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("circle-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let with_dialog = |account: &str, dialog: &str| AddContactInput {
            primary_dialog_key: Some(dialog.to_string()),
            ..contact_input(account, account)
        };
        storage.add_contact(&contact_input("acc-old", "Old"), 1_000).unwrap();
        let heard = storage.add_contact(&contact_input("acc-heard", "Heard"), 1_000).unwrap();
        storage.record_contact_presence_activity(&heard.contact_id, 9_000).unwrap();
        storage.add_contact(&contact_input("acc-new", "New"), 9_500).unwrap();
        storage.add_contact(&with_dialog("acc-written", "dialog:instance:written"), 1_000).unwrap();
        storage.insert_incoming_message("w-1", "dialog:instance:written", "hi", 9_000).unwrap();
        storage.add_contact(&with_dialog("acc-quiet", "dialog:instance:quiet"), 1_000).unwrap();
        storage.insert_incoming_message("q-0", "dialog:instance:quiet", "long ago", 1_100).unwrap();
        storage.add_contact(&with_dialog("acc-queued", "dialog:instance:queued"), 1_000).unwrap();
        storage
            .insert_outgoing_message("o-1", "dialog:instance:queued", "waiting", MessageStatus::Queued, 1_200)
            .unwrap();

        let mut circle: Vec<String> = storage
            .list_recently_active_contact_accounts(5_000)
            .unwrap()
            .into_iter()
            .map(|account| account.0)
            .collect();
        circle.sort();
        assert_eq!(circle, vec!["acc-heard", "acc-new", "acc-queued", "acc-written"]);
    }

    /// Unread counters: incoming lines past the read mark; reading, or writing in the chat, clears
    /// them; a group's own system lines never count; a profile from before the marks starts read.
    #[test]
    fn unread_lines_are_incoming_past_the_read_mark() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("unread-{}.db", unique_suffix()));
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        let a = storage.chat_dialog_key_for(&dialog_key_from_instance_id("peer-a")).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 0);

        storage.insert_incoming_message("a-1", &a, "one", 1_100).unwrap();
        storage.insert_incoming_message("a-2", &a, "two", 1_200).unwrap();
        storage.insert_outgoing_message("a-0", &a, "old own line", MessageStatus::Sent, 1_000).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 2, "an own line older than both reads nothing");
        let before = storage.last_read_mark_updated_at_unix_ms().unwrap();

        storage.mark_dialog_read(&a).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 0);
        assert_ne!(storage.last_read_mark_updated_at_unix_ms().unwrap(), before, "the list sees the read");

        storage.insert_incoming_message("a-3", &a, "three", 1_300).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 1);
        storage.insert_outgoing_message("a-4", &a, "my answer", MessageStatus::Sent, 1_400).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 0, "answering is reading");

        let group = crate::group_dialog_key("g-1");
        storage.insert_incoming_message("g-sys", &group, "Bob joined", 2_000).unwrap();
        storage
            .insert_group_message_meta(
                "g-1",
                &crate::GroupMessageMeta { message_id: "g-sys".into(), sender_pub: None, sender_name: String::new(), kind: "system".into() },
            )
            .unwrap();
        storage.insert_incoming_message("g-1", &group, "hi all", 2_100).unwrap();
        assert_eq!(storage.count_unread_in_dialog(&group).unwrap(), 1, "the system line does not count");

        // A profile from before the read marks: its history so far is read.
        storage.connection.execute_batch("DROP TABLE dialog_read_marks").unwrap();
        storage.migrate().unwrap();
        assert_eq!(storage.count_unread_in_dialog(&group).unwrap(), 0);
        assert_eq!(storage.count_unread_in_dialog(&a).unwrap(), 0);
    }

    // --- the database at rest (docs/decisions/db-at-rest-encryption.md) ---------------------------

    fn encrypted_test_db(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join("omni-stage-1-storage-tests")
            .join(format!("{name}-{}.db", unique_suffix()))
    }

    fn file_contains(path: &Path, needle: &[u8]) -> bool {
        fs::read(path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
    }

    fn key_file_of(db_path: &Path) -> std::path::PathBuf {
        let mut name = db_path.file_name().unwrap().to_os_string();
        name.push(".keys");
        db_path.with_file_name(name)
    }

    /// What a folder copied from another machine looks like here: the DPAPI wrap does not open (drop
    /// it), and this process has never unwrapped the key.
    fn as_if_from_another_machine(db_path: &Path) {
        let key_path = key_file_of(db_path);
        let mut file: serde_json::Value = serde_json::from_slice(&fs::read(&key_path).unwrap()).unwrap();
        file["dpapi"] = serde_json::Value::Null;
        fs::write(&key_path, serde_json::to_vec(&file).unwrap()).unwrap();
        super::forget_database_key(db_path);
    }

    #[test]
    fn a_new_database_is_encrypted_on_disk_and_its_key_wraps_the_profile() {
        let db_path = encrypted_test_db("at-rest-new");
        {
            let storage = Storage::open(&db_path).unwrap();
            storage.migrate().unwrap();
            storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
            let dialog = storage.chat_dialog_key_for(&dialog_key_from_instance_id("peer-a")).unwrap();
            storage.insert_incoming_message("m-1", &dialog, "secret-hello-at-rest", 1_100).unwrap();
            storage.connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        assert!(!file_contains(&db_path, b"SQLite format 3"), "no plaintext SQLite header");
        assert!(!file_contains(&db_path, b"secret-hello-at-rest"), "no message text in the file");
        let key_file: serde_json::Value = serde_json::from_slice(&fs::read(key_file_of(&db_path)).unwrap()).unwrap();
        assert!(!key_file["dpapi"].is_null(), "sealed to this Windows account");
        assert_eq!(key_file["accounts"].as_array().unwrap().len(), 1, "one recovery-phrase wrap per profile");
        assert_eq!(Storage::database_lock_state(&db_path).unwrap(), super::DatabaseLockState::Openable);
    }

    #[test]
    fn a_plaintext_database_is_encrypted_on_first_open_with_its_rows_and_user_version() {
        let db_path = encrypted_test_db("at-rest-migrate");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let plain = rusqlite::Connection::open(&db_path).unwrap();
            plain
                .execute_batch(
                    "PRAGMA journal_mode = WAL;
                     CREATE TABLE legacy (body TEXT);
                     INSERT INTO legacy VALUES ('plaintext-before-upgrade');
                     PRAGMA user_version = 7;",
                )
                .unwrap();
        }
        assert!(file_contains(&db_path, b"SQLite format 3"));

        let storage = Storage::open(&db_path).unwrap();
        let body: String = storage.connection.query_row("SELECT body FROM legacy", [], |row| row.get(0)).unwrap();
        let user_version: i64 = storage.connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        drop(storage);
        assert_eq!(body, "plaintext-before-upgrade");
        assert_eq!(user_version, 7, "sqlcipher_export does not copy it; the migration does");
        assert!(!file_contains(&db_path, b"SQLite format 3"));
        assert!(!file_contains(&db_path, b"plaintext-before-upgrade"));
        let mut leftover = db_path.file_name().unwrap().to_os_string();
        leftover.push(".plaintext-old");
        assert!(!db_path.with_file_name(leftover).exists(), "the plaintext original is wiped");
    }

    #[test]
    fn a_folder_from_another_machine_opens_only_with_a_profiles_recovery_phrase() {
        let db_path = encrypted_test_db("at-rest-moved");
        let mnemonic = {
            let storage = Storage::open(&db_path).unwrap();
            storage.migrate().unwrap();
            let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
            account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone()
        };
        let stranger = {
            let other = Storage::open(encrypted_test_db("at-rest-stranger")).unwrap();
            other.migrate().unwrap();
            let account = other.create_local_account("Mallory", "Laptop", 1_000).unwrap();
            account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone()
        };

        as_if_from_another_machine(&db_path);
        assert_eq!(
            Storage::database_lock_state(&db_path).unwrap(),
            super::DatabaseLockState::NeedsRecoveryPhrase
        );
        assert!(matches!(Storage::open(&db_path), Err(omni_types::OmniError::StorageLocked(_))));
        assert!(Storage::unlock_database_with_mnemonic(&db_path, &stranger).is_err(), "not this file's profile");

        Storage::unlock_database_with_mnemonic(&db_path, &mnemonic).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        assert!(storage.get_local_account().unwrap().is_some());
        drop(storage);
        // Sealed to this machine again: the next process opens it with nothing asked.
        super::forget_database_key(&db_path);
        assert_eq!(Storage::database_lock_state(&db_path).unwrap(), super::DatabaseLockState::Openable);
    }

    /// The phrase typed to open a moved folder is its profile's: the profile's root (sealed by the
    /// other machine) is sealed here too, so the profile does not ask for the phrase a second time.
    /// Its state stays as it was — opening the data is not signing in.
    #[test]
    fn the_phrase_that_opens_a_moved_folder_also_seals_its_profiles_root_here() {
        let db_path = encrypted_test_db("at-rest-adopt");
        let (mnemonic, state) = {
            let storage = Storage::open(&db_path).unwrap();
            storage.migrate().unwrap();
            let account = storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
            let state = account.local_profile_summary.as_ref().unwrap().state;
            // The other machine's DPAPI seal of the profile secrets does not open here.
            storage
                .connection
                .execute("UPDATE profile_secrets SET profile_master_key_blob_hex = '00'", [])
                .unwrap();
            assert!(storage.get_local_account().unwrap().unwrap().recovery_phrase_v2.is_none());
            (account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone(), state)
        };
        as_if_from_another_machine(&db_path);

        Storage::unlock_database_with_mnemonic(&db_path, &mnemonic).unwrap();
        let storage = Storage::open(&db_path).unwrap();
        storage.adopt_profile_root_with_mnemonic(&mnemonic).unwrap();
        let account = storage.get_local_account().unwrap().unwrap();
        assert_eq!(
            account.recovery_phrase_v2.as_ref().map(|phrase| phrase.mnemonic.as_str()),
            Some(mnemonic.as_str()),
            "the root opens on this machine now"
        );
        assert_eq!(account.local_profile_summary.unwrap().state, state, "not signed in by it");

        let stranger = {
            let other = Storage::open(encrypted_test_db("at-rest-adopt-stranger")).unwrap();
            other.migrate().unwrap();
            let account = other.create_local_account("Mallory", "Laptop", 1_000).unwrap();
            account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone()
        };
        storage
            .adopt_profile_root_with_mnemonic(&stranger)
            .expect("a phrase of no profile here only opened nothing");
    }

    #[test]
    fn deleting_the_last_profiles_traces_keeps_the_key_of_the_database_it_keeps() {
        let db_path = encrypted_test_db("at-rest-traces");
        {
            let storage = Storage::open(&db_path).unwrap();
            storage.migrate().unwrap();
        }
        Storage::delete_device_traces_keeping_open_database(&db_path).unwrap();
        assert!(key_file_of(&db_path).exists());
        super::forget_database_key(&db_path);
        Storage::open(&db_path).unwrap().migrate().unwrap();
    }

    #[test]
    fn a_backup_copy_is_encrypted_and_carries_the_key_file() {
        let db_path = encrypted_test_db("at-rest-backup-src");
        let backup_path = encrypted_test_db("at-rest-backup-copy");
        let storage = Storage::open(&db_path).unwrap();
        storage.migrate().unwrap();
        storage.create_local_account("Alice", "Office PC", 1_000).unwrap();
        storage.export_database_backup(&backup_path).unwrap();
        drop(storage);
        assert!(!file_contains(&backup_path, b"SQLite format 3"));
        assert!(key_file_of(&backup_path).exists());
        let copy = Storage::open(&backup_path).unwrap();
        assert!(copy.get_local_account().unwrap().is_some());
    }
}