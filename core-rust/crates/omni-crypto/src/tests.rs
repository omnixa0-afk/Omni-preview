#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use omni_identity::{AccountId, DeviceId};

    use super::{
        bootstrap_initiator_session, bootstrap_responder_session, build_prekey_bundle,
        clear_platform_secret_wrap_key_override_for_tests, configure_platform_secret_wrap_key_hex,
        decrypt_at_rest_bytes, decrypt_delivery_receipt, decrypt_text_message,
        derive_fingerprint_material, encrypt_at_rest_bytes,
        encrypt_delivery_receipt, encrypt_text_message, establish_session, fingerprint_hex,
        generate_local_key_hierarchy, generate_x25519_secret, new_replay_window_state,
        protect_bytes, safety_number, secret_protection_scheme_for_target,
        session_rendezvous_pair_secret, sign_message,
        unprotect_bytes, verify_prekey_bundle, verify_signature, ProtectedSecret,
        SecretProtectionScheme,
        CRYPTO_SUITE_VERSION_CURRENT, DEFAULT_ONE_TIME_PREKEY_COUNT,
    };

    static SECRET_WRAP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn account_id(value: &str) -> AccountId {
        AccountId(value.to_string())
    }

    fn device_id(value: &str) -> DeviceId {
        DeviceId(value.to_string())
    }

    #[test]
    fn key_hierarchy_generation_creates_all_required_material() {
        let hierarchy = generate_local_key_hierarchy(
            &account_id("acct-local"),
            &device_id("dev-local"),
            1_000,
            DEFAULT_ONE_TIME_PREKEY_COUNT,
        )
        .unwrap();

        assert_eq!(hierarchy.identity_key.suite_version, CRYPTO_SUITE_VERSION_CURRENT);
        assert_eq!(hierarchy.device_key.suite_version, CRYPTO_SUITE_VERSION_CURRENT);
        assert_eq!(hierarchy.signed_prekey.suite_version, CRYPTO_SUITE_VERSION_CURRENT);
        assert_eq!(hierarchy.one_time_prekeys.len(), DEFAULT_ONE_TIME_PREKEY_COUNT);
        assert_ne!(hierarchy.identity_key.public_key.0, [0u8; 32]);
        assert_ne!(hierarchy.identity_key.signing_public_key.0, [0u8; 32]);
        assert_ne!(hierarchy.device_key.public_key.0, [0u8; 32]);
    }

    #[test]
    fn ed25519_identity_signing_and_verification_work() {
        let hierarchy = generate_local_key_hierarchy(
            &account_id("acct-sign"),
            &device_id("dev-sign"),
            1_000,
            1,
        )
        .unwrap();
        let message = b"stage5-ed25519-signature-check";
        let signature = sign_message(&hierarchy.identity_key.protected_signing_private_key, message)
            .unwrap();

        verify_signature(
            &hierarchy.identity_key.signing_public_key,
            message,
            &signature,
        )
        .unwrap();
    }

    #[test]
    fn x25519_key_agreement_flow_is_symmetric() {
        let alice_secret = generate_x25519_secret();
        let bob_secret = generate_x25519_secret();
        let alice_public = x25519_dalek::PublicKey::from(&alice_secret);
        let bob_public = x25519_dalek::PublicKey::from(&bob_secret);

        let alice_shared = alice_secret.diffie_hellman(&bob_public);
        let bob_shared = bob_secret.diffie_hellman(&alice_public);

        assert_eq!(alice_shared.to_bytes(), bob_shared.to_bytes());
    }

    #[test]
    fn fingerprint_and_safety_number_derivation_are_stable() {
        let local = generate_local_key_hierarchy(
            &account_id("acct-alice"),
            &device_id("dev-alice"),
            100,
            1,
        )
        .unwrap();
        let remote = generate_local_key_hierarchy(
            &account_id("acct-bob"),
            &device_id("dev-bob"),
            200,
            1,
        )
        .unwrap();

        let material = derive_fingerprint_material(
            &local.identity_key.account_id,
            &local.device_key.device_id,
            local.identity_key.public_key,
            local.identity_key.signing_public_key,
            local.device_key.public_key,
            &remote.identity_key.account_id,
            &remote.device_key.device_id,
            remote.identity_key.public_key,
            remote.identity_key.signing_public_key,
            remote.device_key.public_key,
        );

        let fingerprint = fingerprint_hex(&material);
        let safety_number_value = safety_number(&material);

        assert_eq!(fingerprint.len(), 64);
        assert!(safety_number_value.contains(' '));
    }

    #[test]
    fn session_bootstrap_and_crypto_facade_roundtrip_work() {
        let alice =
            generate_local_key_hierarchy(&account_id("acct-alice"), &device_id("dev-a"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-bob"), &device_id("dev-b"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        verify_prekey_bundle(&bob_bundle).unwrap();

        let (mut alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let bob_one_time = bob.one_time_prekeys.first().unwrap();
        let mut bob_session = bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            Some(bob_one_time),
            &bootstrap,
            301,
        )
        .unwrap();
        alice_session = establish_session(&alice_session, 302);
        bob_session = establish_session(&bob_session, 302);

        let mut alice_replay = new_replay_window_state(alice_session.session_address.clone(), 302);
        let mut bob_replay = new_replay_window_state(bob_session.session_address.clone(), 302);

        let encrypted = encrypt_text_message(&mut alice_session, "msg-1", "hello", 400).unwrap();
        let decrypted = decrypt_text_message(&mut bob_session, &mut bob_replay, &encrypted).unwrap();
        assert_eq!(decrypted.message_id, "msg-1");
        assert_eq!(decrypted.body, "hello");

        let receipt = encrypt_delivery_receipt(&mut bob_session, "msg-1", 450).unwrap();
        let decrypted_receipt =
            decrypt_delivery_receipt(&mut alice_session, &mut alice_replay, &receipt).unwrap();
        assert_eq!(decrypted_receipt.message_id, "msg-1");
        assert_eq!(decrypted_receipt.delivered_at_unix_ms, 450);
    }

    #[test]
    fn both_sides_derive_identical_rendezvous_pair_secret() {
        // The blinded serverless-rendezvous topic + beacon AEAD need BOTH peers to hold the
        // same stable per-contact secret. It is derived from the SYMMETRIC X3DH shared secret,
        // so the initiator and responder must produce an identical, non-trivial value — and it
        // must NOT ratchet (a re-bootstrap reproduces it). See serverless-bootstrap-rendezvous.md.
        let alice =
            generate_local_key_hierarchy(&account_id("acct-alice"), &device_id("dev-a"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-bob"), &device_id("dev-b"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();

        let (alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let bob_one_time = bob.one_time_prekeys.first().unwrap();
        let bob_session = bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            Some(bob_one_time),
            &bootstrap,
            301,
        )
        .unwrap();

        let alice_secret = session_rendezvous_pair_secret(&alice_session).unwrap();
        let bob_secret = session_rendezvous_pair_secret(&bob_session).unwrap();
        assert!(alice_secret.is_some(), "initiator derives a rendezvous secret");
        assert_eq!(
            alice_secret, bob_secret,
            "both peers derive the identical stable rendezvous secret"
        );
        assert_ne!(alice_secret.unwrap(), [0u8; 32], "secret is not the trivial value");
    }

    #[test]
    fn invite_bootstrap_secret_is_deterministic_and_domain_separated() {
        // First-contact bootstrap: both sides derive the SAME secret from the invite nonce (so they
        // meet on the same blinded bootstrap topic), it's non-trivial, and it is domain-separated
        // from the durable pair secret — feeding the same 32 bytes through both derivations must yield
        // DIFFERENT secrets, so a bootstrap topic can never alias a durable one. See
        // docs/decisions/invite-bootstrap-rendezvous.md.
        let nonce = [0x5au8; 32];
        let inviter = crate::derive_rendezvous_bootstrap_secret(&nonce).unwrap();
        let importer = crate::derive_rendezvous_bootstrap_secret(&nonce).unwrap();
        assert_eq!(inviter, importer, "both sides derive the identical bootstrap secret");
        assert_ne!(inviter, [0u8; 32], "secret is not the trivial value");
        assert_ne!(inviter, nonce, "secret is the HKDF output, not the raw nonce");
        assert_ne!(
            crate::derive_rendezvous_bootstrap_secret(&nonce).unwrap(),
            crate::derive_rendezvous_pair_secret(&nonce).unwrap(),
            "bootstrap and durable derivations are domain-separated"
        );
    }

    #[test]
    fn replay_window_rejects_replayed_payloads() {
        let alice =
            generate_local_key_hierarchy(&account_id("acct-r1"), &device_id("dev-r1"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-r2"), &device_id("dev-r2"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        let (mut alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let mut bob_session = bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            &bootstrap,
            301,
        )
        .unwrap();
        alice_session = establish_session(&alice_session, 302);
        bob_session = establish_session(&bob_session, 302);
        let mut bob_replay = new_replay_window_state(bob_session.session_address.clone(), 302);

        let encrypted = encrypt_text_message(&mut alice_session, "msg-r", "hello", 400).unwrap();
        decrypt_text_message(&mut bob_session, &mut bob_replay, &encrypted).unwrap();
        let replay_error = decrypt_text_message(&mut bob_session, &mut bob_replay, &encrypted)
            .unwrap_err()
            .to_string();

        assert!(
            replay_error.contains("replayed") || replay_error.contains("replay window"),
            "unexpected replay error: {replay_error}"
        );
    }

    /// Mesh durable delivery re-offers held frames after every reconnect, so a frame from a ratchet
    /// chain the receiver has already moved past arrives again. It must be recognised as the
    /// duplicate it is — not run through the receive ratchet and reported as "authentication
    /// failed", which the app counts toward rebuilding the session
    /// (docs/bugs/receipt-after-restart-fails-to-decrypt.md).
    #[test]
    fn a_duplicate_from_an_old_chain_is_a_replay_not_an_auth_failure() {
        let alice =
            generate_local_key_hierarchy(&account_id("acct-d1"), &device_id("dev-d1"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-d2"), &device_id("dev-d2"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        let (mut alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let mut bob_session = bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            &bootstrap,
            301,
        )
        .unwrap();
        alice_session = establish_session(&alice_session, 302);
        bob_session = establish_session(&bob_session, 302);
        let mut alice_replay = new_replay_window_state(alice_session.session_address.clone(), 302);
        let mut bob_replay = new_replay_window_state(bob_session.session_address.clone(), 302);

        // Alice's first chain: one frame delivered, one sealed but never delivered.
        let delivered = encrypt_text_message(&mut alice_session, "msg-1", "one", 400).unwrap();
        let undelivered = encrypt_text_message(&mut alice_session, "msg-lost", "lost", 401).unwrap();
        decrypt_text_message(&mut bob_session, &mut bob_replay, &delivered).unwrap();
        // A round trip moves both sides to new chains.
        let reply = encrypt_text_message(&mut bob_session, "msg-2", "two", 402).unwrap();
        decrypt_text_message(&mut alice_session, &mut alice_replay, &reply).unwrap();
        let newer = encrypt_text_message(&mut alice_session, "msg-3", "three", 403).unwrap();
        decrypt_text_message(&mut bob_session, &mut bob_replay, &newer).unwrap();
        let remote_key_before = bob_session.remote_ratchet_public_key;
        let receive_counter_before = bob_session.receive_counter;

        let error = decrypt_text_message(&mut bob_session, &mut bob_replay, &delivered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("replay window"), "a known duplicate is a replay: {error}");
        assert_eq!(bob_session.remote_ratchet_public_key, remote_key_before, "state untouched");
        assert_eq!(bob_session.receive_counter, receive_counter_before, "state untouched");
        let next = encrypt_text_message(&mut alice_session, "msg-4", "four", 404).unwrap();
        assert_eq!(
            decrypt_text_message(&mut bob_session, &mut bob_replay, &next).unwrap().body,
            "four",
            "the session keeps working after the duplicate"
        );

        // A frame Bob never saw from that old chain is not a duplicate: it arrives late, and its
        // key was remembered when the chain was replaced (`previous_chain_length` told Bob there
        // was one more). It opens once and only once.
        assert_eq!(
            decrypt_text_message(&mut bob_session, &mut bob_replay, &undelivered).unwrap().body,
            "lost"
        );
        let error = decrypt_text_message(&mut bob_session, &mut bob_replay, &undelivered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("replay"), "opened twice: {error}");
    }

    /// Two established sessions, alice -> bob, with replay windows.
    fn established_pair(
        tag: &str,
    ) -> (super::SessionRecord, super::SessionRecord, super::ReplayWindowState, super::ReplayWindowState) {
        let alice = generate_local_key_hierarchy(
            &account_id(&format!("acct-{tag}1")),
            &device_id(&format!("dev-{tag}1")),
            100,
            1,
        )
        .unwrap();
        let bob = generate_local_key_hierarchy(
            &account_id(&format!("acct-{tag}2")),
            &device_id(&format!("dev-{tag}2")),
            200,
            1,
        )
        .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        let (alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let bob_session = bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            &bootstrap,
            301,
        )
        .unwrap();
        let alice_session = establish_session(&alice_session, 302);
        let bob_session = establish_session(&bob_session, 302);
        let alice_replay = new_replay_window_state(alice_session.session_address.clone(), 302);
        let bob_replay = new_replay_window_state(bob_session.session_address.clone(), 302);
        (alice_session, bob_session, alice_replay, bob_replay)
    }

    /// The mesh and the couriers do not promise order: frame 2 overtaken by frame 3 used to be
    /// refused as "replayed" for good, because the chain had already stepped past its key
    /// (docs/bugs/message-stuck-sending-after-restart.md).
    #[test]
    fn a_frame_overtaken_by_a_later_one_still_opens() {
        let (mut alice, mut bob, _, mut bob_replay) = established_pair("o");
        let first = encrypt_text_message(&mut alice, "msg-1", "one", 400).unwrap();
        let second = encrypt_text_message(&mut alice, "msg-2", "two", 401).unwrap();
        let third = encrypt_text_message(&mut alice, "msg-3", "three", 402).unwrap();

        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &third).unwrap().body, "three");
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &first).unwrap().body, "one");
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &second).unwrap().body, "two");
        assert!(bob.protected_skipped_message_keys.is_none(), "every remembered key was spent");
        let fourth = encrypt_text_message(&mut alice, "msg-4", "four", 403).unwrap();
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &fourth).unwrap().body, "four");
    }

    /// A forged frame that names a remembered slot neither opens nor spends the key, and leaves
    /// the session as it was.
    #[test]
    fn a_forged_frame_does_not_spend_a_remembered_key() {
        let (mut alice, mut bob, _, mut bob_replay) = established_pair("f");
        let first = encrypt_text_message(&mut alice, "msg-1", "one", 400).unwrap();
        let second = encrypt_text_message(&mut alice, "msg-2", "two", 401).unwrap();
        decrypt_text_message(&mut bob, &mut bob_replay, &second).unwrap();

        let mut forged = first.clone();
        forged.mac_hex = "00".repeat(16);
        let before = bob.clone();
        let error = decrypt_text_message(&mut bob, &mut bob_replay, &forged)
            .unwrap_err()
            .to_string();
        assert!(error.contains("authentication failed"), "{error}");
        assert_eq!(bob.protected_skipped_message_keys, before.protected_skipped_message_keys);
        assert_eq!(bob.receive_counter, before.receive_counter);
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &first).unwrap().body, "one");
    }

    /// Past the window on the CURRENT chain is a real gap (the session needs a rebuild); far past
    /// it on a chain we do not know is a dead session's frame offered again — told apart, so the
    /// app does not rebuild a working session over it.
    #[test]
    fn a_far_frame_on_an_unknown_chain_is_not_a_gap_in_ours() {
        let (mut alice, mut bob, _, mut bob_replay) = established_pair("g");
        let first = encrypt_text_message(&mut alice, "msg-1", "one", 400).unwrap();
        decrypt_text_message(&mut bob, &mut bob_replay, &first).unwrap();

        let mut far_on_ours = encrypt_text_message(&mut alice, "msg-2", "two", 401).unwrap();
        far_on_ours.counter = 500;
        let error = decrypt_text_message(&mut bob, &mut bob_replay, &far_on_ours).unwrap_err().to_string();
        assert!(error.contains("counter exceeded the supported receive gap"), "{error}");

        let (mut stranger, _, _, _) = established_pair("h");
        let mut far_dead = encrypt_text_message(&mut stranger, "msg-x", "dead", 402).unwrap();
        far_dead.counter = 3_000;
        let error = decrypt_text_message(&mut bob, &mut bob_replay, &far_dead).unwrap_err().to_string();
        assert!(error.contains("unknown ratchet chain"), "{error}");
        assert!(!error.contains("counter exceeded"), "{error}");
        // The session is untouched and keeps working.
        let next = encrypt_text_message(&mut alice, "msg-3", "three", 403).unwrap();
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &next).unwrap().body, "three");
    }

    /// The one-way lock of 2026-10-15: alice's new chain ran far ahead while bob could not hear her
    /// (1500 frames lost on a relay-only stretch). Bob opens her next frame anyway — stepping the chain
    /// up to it — and the session works both ways again; nobody had to rebuild it.
    #[test]
    fn a_new_chain_far_past_the_gap_still_opens_and_unlocks_the_session() {
        let (mut alice, mut bob, mut alice_replay, mut bob_replay) = established_pair("j");
        let hello = encrypt_text_message(&mut alice, "msg-1", "hello", 400).unwrap();
        decrypt_text_message(&mut bob, &mut bob_replay, &hello).unwrap();
        let reply = encrypt_text_message(&mut bob, "msg-b1", "reply", 401).unwrap();
        decrypt_text_message(&mut alice, &mut alice_replay, &reply).unwrap();

        // Alice's next chain (she heard bob's key): 1500 frames bob never gets.
        for n in 0..1500 {
            encrypt_text_message(&mut alice, &format!("lost-{n}"), "lost", 402).unwrap();
        }
        let through = encrypt_text_message(&mut alice, "msg-through", "through", 403).unwrap();
        assert!(through.counter > 1500);
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &through).unwrap().body, "through");

        let answer = encrypt_text_message(&mut bob, "msg-b2", "answer", 404).unwrap();
        assert_eq!(decrypt_text_message(&mut alice, &mut alice_replay, &answer).unwrap().body, "answer");
        let after = encrypt_text_message(&mut alice, "msg-after", "after", 405).unwrap();
        assert_eq!(decrypt_text_message(&mut bob, &mut bob_replay, &after).unwrap().body, "after");
    }

    /// Remembered keys survive a store and a load: the late frame may come after a restart.
    #[test]
    fn remembered_keys_survive_serialization() {
        let (mut alice, mut bob, _, mut bob_replay) = established_pair("s");
        let first = encrypt_text_message(&mut alice, "msg-1", "one", 400).unwrap();
        let second = encrypt_text_message(&mut alice, "msg-2", "two", 401).unwrap();
        decrypt_text_message(&mut bob, &mut bob_replay, &second).unwrap();
        let mut reloaded: super::SessionRecord =
            serde_json::from_str(&serde_json::to_string(&bob).unwrap()).unwrap();
        assert_eq!(
            decrypt_text_message(&mut reloaded, &mut bob_replay, &first).unwrap().body,
            "one"
        );
    }

    #[test]
    fn at_rest_payload_encryption_roundtrip_works() {
        let master_key = [7u8; 32];
        let aad = b"dialog:test";
        let plaintext = br#"{"kind":"text","message_id":"msg-1"}"#;

        let encrypted = encrypt_at_rest_bytes(&master_key, aad, plaintext).unwrap();
        let decrypted = decrypt_at_rest_bytes(&master_key, aad, &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
        assert_ne!(encrypted.ciphertext_hex, hex::encode(plaintext));
    }

    #[test]
    fn android_target_never_uses_plaintext_secret_protection_scheme() {
        assert_eq!(
            secret_protection_scheme_for_target("android"),
            SecretProtectionScheme::PlatformWrappedV1
        );
        assert_ne!(
            secret_protection_scheme_for_target("android"),
            SecretProtectionScheme::PlaintextFallbackV1
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_dpapi_secret_protection_roundtrip_works() {
        let protected = protect_bytes(b"stage9-dpapi-check").unwrap();
        assert_eq!(protected.scheme, SecretProtectionScheme::DpapiV1);

        let unprotected = unprotect_bytes(&protected).unwrap();
        assert_eq!(unprotected, b"stage9-dpapi-check");
    }

    #[test]
    fn platform_wrapped_secret_roundtrip_works_when_platform_key_is_present() {
        let _env_lock = SECRET_WRAP_ENV_LOCK.lock().unwrap();
        configure_platform_secret_wrap_key_hex(
            "0909090909090909090909090909090909090909090909090909090909090909",
        )
        .unwrap();

        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let wrapped_bytes = super::platform_wrap(b"stage9-platform-wrap")?;
            let protected = ProtectedSecret {
                scheme: SecretProtectionScheme::PlatformWrappedV1,
                blob_hex: hex::encode(wrapped_bytes),
            };
            let roundtrip = unprotect_bytes(&protected)?;
            assert_eq!(roundtrip, b"stage9-platform-wrap");
            Ok(())
        })();

        clear_platform_secret_wrap_key_override_for_tests();

        result.unwrap();
    }

    // --- Group crypto (G2) --------------------------------------------------

    #[test]
    fn group_call_media_keys_differ_per_sender() {
        let secret = super::generate_group_key();
        let a = super::derive_group_call_media_key(&secret, "pub-a").expect("derive");
        let b = super::derive_group_call_media_key(&secret, "pub-b").expect("derive");
        assert_ne!(a, b);
        assert_eq!(
            a,
            super::derive_group_call_media_key(&secret, "pub-a").expect("derive")
        );
    }

    #[test]
    fn group_call_media_seals_with_plaintext_routing_header() {
        let secret = super::generate_group_key();
        let key = super::derive_group_call_media_key(&secret, "pub-a").expect("derive");
        let rtp = vec![0x80u8, 0x60, 1, 2, 3, 4, 5, 6, 7, 8];
        let sealed = super::seal_group_call_media(&key, 0x4F4D4E41, 42, &rtp).expect("seal");

        // The forwarder reads the header WITHOUT the key (E2E: it never decrypts).
        let (ssrc, seq) = super::peek_group_call_media_header(&sealed).expect("header");
        assert_eq!((ssrc, seq), (0x4F4D4E41, 42));

        // The recipient (holding the sender's media key) recovers the RTP payload.
        let opened = super::open_group_call_media(&key, &sealed).expect("open");
        assert_eq!(opened, rtp);

        // Wrong key / a flipped header byte fails authentication.
        let other = super::derive_group_call_media_key(&secret, "pub-b").expect("derive");
        assert!(super::open_group_call_media(&other, &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered[0] ^= 0xFF;
        assert!(super::open_group_call_media(&key, &tampered).is_err());
    }

    fn file_at_rest_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omni-file-at-rest-{}",
            hex::encode(super::random_bytes(6).unwrap())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn a_file_at_rest_reads_back_whole_and_from_any_offset() {
        use std::io::{Read as _, Seek as _, Write as _};
        let dir = file_at_rest_dir();
        let dek = [7u8; 32];
        let chunk = super::FILE_AT_REST_CHUNK_BYTES as usize;
        for len in [0, 1, chunk - 1, chunk, chunk + 1, chunk * 3 + chunk / 2] {
            let plain = pattern(len);
            let path = dir.join(format!("f{len}"));
            let mut writer = super::FileAtRestWriter::create(&path, &dek).unwrap();
            for piece in plain.chunks(1000) {
                writer.write_all(piece).unwrap();
            }
            writer.finish().unwrap();
            assert!(super::is_file_at_rest(&path));
            assert_eq!(super::file_at_rest_len(&path).unwrap(), len as u64);
            let on_disk = std::fs::read(&path).unwrap();
            if len > 64 {
                assert!(on_disk.windows(64).all(|window| window != &plain[..64]), "no plaintext on disk");
            }

            let mut reader = super::FileAtRestReader::open(&path, &dek).unwrap();
            assert_eq!(reader.len(), len as u64);
            let mut back = Vec::new();
            reader.read_to_end(&mut back).unwrap();
            assert_eq!(back, plain, "len {len}");
            if len > 10 {
                let at = len - len / 3;
                reader.seek(std::io::SeekFrom::Start(at as u64)).unwrap();
                let mut tail = Vec::new();
                reader.read_to_end(&mut tail).unwrap();
                assert_eq!(tail, plain[at..], "len {len} from {at}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_at_rest_assembled_out_of_order_opens_after_a_rename_and_only_under_its_key() {
        use std::io::Read as _;
        let dir = file_at_rest_dir();
        let dek = [9u8; 32];
        let chunk = super::FILE_AT_REST_CHUNK_BYTES as usize;
        let plain = pattern(chunk * 5 + 123);
        let part = dir.join("download.part");
        {
            // Leaves land out of order, the last (short) one first; the writer is reopened in between,
            // as a resumed download would.
            let mut writer = super::FileAtRestWriter::open_for_offsets(&part, &dek).unwrap();
            writer.write_at((chunk * 4) as u64, &plain[chunk * 4..]).unwrap();
            writer.write_at(0, &plain[..chunk * 2]).unwrap();
            let mut writer = super::FileAtRestWriter::open_for_offsets(&part, &dek).unwrap();
            writer.write_at((chunk * 2) as u64, &plain[chunk * 2..chunk * 4]).unwrap();
            assert!(writer.write_at(5, b"x").is_err(), "unaligned writes are refused");
        }
        let done = dir.join("done.jpg");
        std::fs::rename(&part, &done).unwrap();
        let mut back = Vec::new();
        super::FileAtRestReader::open(&done, &dek).unwrap().read_to_end(&mut back).unwrap();
        assert_eq!(back, plain);

        let mut other = Vec::new();
        assert!(super::FileAtRestReader::open(&done, &[1u8; 32]).unwrap().read_to_end(&mut other).is_err());

        let mut bytes = std::fs::read(&done).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&done, &bytes).unwrap();
        let mut tampered = Vec::new();
        assert!(super::FileAtRestReader::open(&done, &dek).unwrap().read_to_end(&mut tampered).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_file_reads_as_it_is_and_converts_both_ways() {
        use std::io::Read as _;
        let dir = file_at_rest_dir();
        let dek = [3u8; 32];
        let plain = pattern(200_000);
        let source = dir.join("plain.bin");
        std::fs::write(&source, &plain).unwrap();
        assert!(!super::is_file_at_rest(&source));
        let mut back = Vec::new();
        let mut reader = super::FileAtRestReader::open(&source, &dek).unwrap();
        assert!(!reader.is_encrypted());
        reader.read_to_end(&mut back).unwrap();
        assert_eq!(back, plain);

        let sealed = dir.join("sealed.bin");
        super::encrypt_file_at_rest(&source, &sealed, &dek).unwrap();
        let opened = dir.join("opened.bin");
        super::decrypt_file_at_rest(&sealed, &opened, &dek).unwrap();
        assert_eq!(std::fs::read(&opened).unwrap(), plain);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
