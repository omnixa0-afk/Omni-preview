// Included in tests.rs: uses the real profile, MLS and group-feed fixtures.
fn workspace_files_command(
    app: &OmniApp,
    json: serde_json::Value,
) -> omni_types::OmniResult<crate::WorkspaceFilesDetails> {
    app.prepare_workspace_files_command(serde_json::from_value(json).unwrap())?
        .run()
}

fn workspace_git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A member's node as a content stream gives it: version records and content of the project
/// `published` (its working folder `work`), whole or by blocks, as `app_workspace_content::serve_content_stream`
/// and `serve_blocks_stream` do past their gate.
fn workspace_content_stream(
    published: PathBuf,
    work: Option<PathBuf>,
) -> impl Fn(omni_mesh_runtime::MeshNodeAddress) -> omni_types::OmniResult<omni_mesh_runtime::BulkStream>
{
    move |_| {
        use omni_mesh_runtime::BulkStream;
        use tokio::io::AsyncReadExt as _;
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_recv, a_send) = tokio::io::split(a);
        let (mut b_recv, b_send) = tokio::io::split(b);
        let (published, work) = (published.clone(), work.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut magic = [0_u8; 4];
            rt.block_on(b_recv.read_exact(&mut magic)).unwrap();
            if magic == crate::app_workspace::content::blocks::BLOCKS_MAGIC {
                let request = crate::app_workspace::content::blocks::read_blocks_request(&mut b_recv, &rt).unwrap();
                let file = crate::app_workspace::test_object_path(&published, work.as_deref(), &request.oid)
                    .unwrap_or_else(|| published.join("not-here"));
                crate::app_workspace::content::blocks::serve_blocks_request(&published, &request, &file, Box::new(b_send), &rt, &mut crate::app_serve_right::ServeRight::new(|| Ok(()))).unwrap();
                return;
            }
            let mut header = [0_u8; 1];
            rt.block_on(b_recv.read_exact(&mut header)).unwrap();
            let mut request = vec![0_u8; usize::from(header[0]) + 64 + 8];
            rt.block_on(b_recv.read_exact(&mut request)).unwrap();
            let at = usize::from(header[0]);
            let oid = String::from_utf8(request[at..at + 64].to_vec()).unwrap();
            let offset = u64::from_le_bytes(request[at + 64..].try_into().unwrap());
            let file = crate::app_workspace::test_object_path(&published, work.as_deref(), &oid)
                .unwrap_or_else(|| published.join("not-here"));
            crate::app_workspace::content::serve_object(
                &file,
                offset,
                BulkStream {
                    send: Box::new(b_send),
                    recv: Box::new(b_recv),
                },
                &mut crate::app_serve_right::ServeRight::new(|| Ok(())),
            )
            .unwrap();
        });
        Ok(BulkStream {
            send: Box::new(a_send),
            recv: Box::new(a_recv),
        })
    }
}

/// A stream to `app`'s node for the project `repo`.
fn workspace_stream_from(
    app: &OmniApp,
    group: &str,
    repo: &str,
) -> impl Fn(omni_mesh_runtime::MeshNodeAddress) -> omni_types::OmniResult<omni_mesh_runtime::BulkStream>
{
    let record = app.storage.get_workspace_repo(group, repo).unwrap().unwrap();
    workspace_content_stream(PathBuf::from(record.published_path), record.work_path.map(PathBuf::from))
}

/// The mesh's part, by hand: `from` announces its line, `to` hears it and runs a round of the files
/// work with `from`'s node as the one linked member.
fn workspace_exchange(from: &OmniApp, to: &OmniApp, group: &str, repo: &str) -> crate::WorkspaceFilesSync {
    from.workspace_publish(group, repo).unwrap();
    deliver_group_feed(from, &[to]);
    let open = workspace_stream_from(from, group, repo);
    crate::app_workspace::files_work(
        &to.storage,
        group,
        repo,
        &[omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20])],
        &open,
    )
    .unwrap()
}

fn workspace_fetch_metadata(from: &OmniApp, to: &OmniApp, group: &str, repo: &str) {
    workspace_exchange(from, to, group, repo);
}

/// Blind audit 2026-11-11, pass 13 (growth without a bound): a member's events whose key it chooses
/// a part of — a file taken, a project's line — were kept every one, and a line of a project we had
/// not got made that project here, however many were named.
#[test]
fn a_members_workspace_events_and_the_projects_it_names_are_bounded() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-bounds", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-bounds", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let folder = std::env::temp_dir().join(format!("omni-workspace-bounds-{}", unique_suffix()));
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("readme.txt"), "x").unwrap();
    alice.workspace_add_files_repo(&gid, "files", &folder.to_string_lossy()).unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    let bob_member = alice.storage.list_group_members(&gid).unwrap().into_iter().find(|m| m.member_pub != alice.storage.get_group(&gid).unwrap().unwrap().my_member_pub).unwrap();
    let sender = omni_group_mls::MemberCard {
        member_pub: bob_member.member_pub.clone(),
        account_id: bob_member.account_id.clone(),
        display_name: "Bob".into(),
        device_chain: String::new(),
    };
    let now = super::current_unix_ms();
    let max = omni_storage::WORKSPACE_FILE_EVENTS_PER_SENDER_KIND_MAX;
    for i in 0..max + 20 {
        alice
            .storage
            .put_workspace_file_event(
                &gid,
                &format!("lock:files:part-{i}.step:{}", sender.member_pub),
                &omni_storage::WorkspaceFileEventRecord {
                    sender_account: sender.account_id.clone(),
                    sender_member: sender.member_pub.clone(),
                    event_json: format!("{{\"n\":{i}}}"),
                    at_unix_ms: now + i,
                },
            )
            .unwrap();
    }
    let kept: Vec<_> = alice.storage.list_workspace_file_events(&gid).unwrap().into_iter().filter(|r| r.sender_account == sender.account_id && r.event_json.starts_with("{\"n\"")).collect();
    assert_eq!(kept.len() as i64, max, "the newest, up to the bound");
    assert!(kept.iter().all(|r| r.at_unix_ms >= now + 20), "the oldest went");

    // Pass 14: a reviewer's «needs changes» on other versions do not push out its «accepted».
    let review = |oid: String, accepted: bool, at: i64| {
        let event = omni_protocol::WorkspaceFilesEventV1::Review { repo: "files".into(), oid: oid.clone(), accepted, authority: None, at_unix_ms: at };
        alice
            .storage
            .put_workspace_file_event(
                &gid,
                &format!("review:files:{oid}:{}", sender.member_pub),
                &omni_storage::WorkspaceFileEventRecord {
                    sender_account: sender.account_id.clone(),
                    sender_member: sender.member_pub.clone(),
                    event_json: serde_json::to_string(&event).unwrap(),
                    at_unix_ms: at,
                },
            )
            .unwrap();
    };
    review("a".repeat(40), true, now);
    for i in 0..omni_storage::WORKSPACE_REVIEWS_OPEN_MAX + 10 {
        review(format!("{i:040x}"), false, now + 1 + i);
    }
    let reviews: Vec<_> = alice.storage.list_workspace_file_events(&gid).unwrap().into_iter().filter(|r| r.sender_account == sender.account_id && r.event_json.contains("\"review\"")).collect();
    assert!(reviews.iter().any(|r| r.event_json.contains(&"a".repeat(40))), "the accepted one stays");
    assert_eq!(reviews.len() as i64, omni_storage::WORKSPACE_REVIEWS_OPEN_MAX + 1, "the rest, up to their bound");

    let tip = alice
        .storage
        .list_workspace_file_events(&gid)
        .unwrap()
        .into_iter()
        .find_map(|r| match serde_json::from_str::<omni_protocol::WorkspaceFilesEventV1>(&r.event_json) {
            Ok(omni_protocol::WorkspaceFilesEventV1::Tip { version, .. }) => Some(version),
            _ => None,
        })
        .expect("alice's own line");
    let say = |repo: String| {
        let event = omni_protocol::WorkspaceFilesEventV1::Tip { repo, version: tip.clone(), at_unix_ms: super::current_unix_ms() };
        crate::app_workspace::custody::receive(&alice.storage, &gid, &sender, &serde_json::to_string(&event).unwrap())
    };
    let room = crate::app_workspace::custody::WORKSPACE_PROJECTS_MAX - alice.workspace_repos(&gid).unwrap().len();
    for i in 0..room {
        say(format!("named-{i}")).unwrap();
    }
    assert!(say("one-more".into()).is_err(), "no more projects than the bound");
    assert_eq!(alice.workspace_repos(&gid).unwrap().len(), crate::app_workspace::custody::WORKSPACE_PROJECTS_MAX);
    say("files".into()).unwrap();
}

#[test]
fn workspace_custody_needs_admin_nomination_and_consent_and_demotion_preserves_membership() {
    let alice = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-custody", "alice"),
        "Alice",
        "A",
    );
    let bob = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-custody", "bob"),
        "Bob",
        "B",
    );
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    // Custodians are appointed per project: two projects, the role is asked for one.
    let projects = std::env::temp_dir().join(format!("omni-custody-projects-{}", unique_suffix()));
    for repo in ["files", "other"] {
        let folder = projects.join(repo);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("readme.txt"), repo).unwrap();
        alice.workspace_add_files_repo(&gid, repo, &folder.to_string_lossy()).unwrap();
        alice.workspace_save_version(&gid, repo, "v1").unwrap();
        workspace_fetch_metadata(&alice, &bob, &gid, repo);
    }
    let details_of = |app: &OmniApp, repo: &str| {
        workspace_files_command(
            app,
            serde_json::json!({"action":"details","group_id":gid,"repo":repo}),
        )
        .unwrap()
    };
    let details = |app: &OmniApp| details_of(app, "files");
    let initial = details(&bob);
    assert!(
        initial.policy.participants_need_approval && !initial.policy.newcomers_download_current
    );
    assert!(!initial.can_review);
    let bob_account = initial
        .members
        .iter()
        .find(|m| m.is_me)
        .unwrap()
        .account_id
        .clone();
    let bob_member = alice
        .storage
        .list_group_members(&gid)
        .unwrap()
        .into_iter()
        .find(|m| m.account_id == bob_account)
        .unwrap();
    let sender = omni_group_mls::MemberCard {
        member_pub: bob_member.member_pub,
        account_id: bob_account.clone(),
        display_name: "Bob".into(),
        device_chain: String::new(),
    };
    let forged = omni_protocol::WorkspaceFilesEventV1::Policy {
        repo: "files".into(),
        policy: omni_protocol::WorkspaceFilesPolicyV1 {
            participants_need_approval: false,
            newcomers_download_current: true,
            edit_only_taken: false,
            changed_at_unix_ms: super::current_unix_ms(),
            ..Default::default()
        },
    };
    assert!(
        crate::app_workspace::custody::receive(
            &alice.storage,
            &gid,
            &sender,
            &serde_json::to_string(&forged).unwrap()
        )
        .is_err(),
        "an authenticated member cannot forge an admin's policy"
    );
    assert!(workspace_files_command(
        &bob,
        serde_json::json!({"action":"nominate","group_id":gid,"repo":"files","account_id":bob_account})
    )
    .is_err());
    workspace_files_command(
        &alice,
        serde_json::json!({"action":"nominate","group_id":gid,"repo":"files","account_id":bob_account}),
    )
    .unwrap();
    deliver_group_feed(&alice, &[&bob]);
    let proposed = details(&bob);
    assert!(!proposed.can_review, "nomination alone grants no authority");
    let nomination = proposed
        .members
        .iter()
        .find(|m| m.is_me)
        .unwrap()
        .nomination
        .clone()
        .unwrap();
    assert!(workspace_files_command(&bob,serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":null})).is_err());
    assert!(workspace_files_command(&bob,serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":"relative-workspace"})).is_err(), "a custody folder must be absolute");
    assert!(!details(&bob).can_review, "invalid folder consent grants no authority");
    let folder = std::env::temp_dir().join(format!("omni-custody-{}", unique_suffix()));
    workspace_files_command(&bob,serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":folder})).unwrap();
    let early_review = omni_protocol::WorkspaceFilesEventV1::Review {
        repo: "files".into(),
        oid: "a".repeat(40),
        accepted: true,
        authority: Some(nomination.clone()),
        at_unix_ms: super::current_unix_ms(),
    };
    crate::app_workspace::custody::receive(
        &alice.storage,
        &gid,
        &sender,
        &serde_json::to_string(&early_review).unwrap(),
    )
    .unwrap();
    assert!(
        !alice
            .storage
            .list_workspace_file_events(&gid)
            .unwrap()
            .iter()
            .any(|r| matches!(
                serde_json::from_str::<omni_protocol::WorkspaceFilesEventV1>(&r.event_json),
                Ok(omni_protocol::WorkspaceFilesEventV1::Review { oid, .. }) if oid == "a".repeat(40)
            )),
        "a decision arriving before its consent has no authority yet"
    );
    deliver_group_feed(&bob, &[&alice]);
    assert!(
        alice
            .storage
            .list_workspace_file_events(&gid)
            .unwrap()
            .iter()
            .any(|r| matches!(
                serde_json::from_str::<omni_protocol::WorkspaceFilesEventV1>(&r.event_json),
                Ok(omni_protocol::WorkspaceFilesEventV1::Review { accepted: true, oid, .. }) if oid == "a".repeat(40)
            )),
        "the same decision becomes effective once its matching consent arrives"
    );
    assert!(details(&bob).can_review);
    assert!(
        !details_of(&bob, "other").can_review,
        "custody of one project gives no authority over another"
    );
    assert!(details(&alice)
        .members
        .iter()
        .any(|m| m.account_id == bob_account && m.keeper));
    workspace_files_command(
        &alice,
        serde_json::json!({"action":"dismiss","group_id":gid,"repo":"files","account_id":bob_account}),
    )
    .unwrap();
    deliver_group_feed(&alice, &[&bob]);
    assert!(!details(&bob).can_review);
    let future_consent = omni_protocol::WorkspaceFilesEventV1::Consent {
        account_id: bob_account.clone(),
        repo: "files".into(),
        nomination: nomination.clone(),
        accepted: true,
        at_unix_ms: super::current_unix_ms() + 240_000,
    };
    crate::app_workspace::custody::receive(
        &alice.storage,
        &gid,
        &sender,
        &serde_json::to_string(&future_consent).unwrap(),
    )
    .unwrap();
    let changed = workspace_files_command(&alice,serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":true,"newcomers_download_current":false})).unwrap();
    assert!(
        changed.policy.changed_at_unix_ms <= super::current_unix_ms() + 1000,
        "another member's clock cannot advance the admin's policy clock"
    );
    assert_eq!(
        details_of(&alice, "other").policy.changed_at_unix_ms,
        0,
        "a project's rule leaves the other projects' alone"
    );
    assert_eq!(
        bob.storage.get_group(&gid).unwrap().unwrap().my_status,
        "member"
    );
    assert!(workspace_files_command(&bob,serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":folder})).is_err(),"late consent cannot resurrect a dismissed role");
    assert_eq!(
        details(&bob).folder.as_deref(),
        folder.to_str(),
        "demotion does not erase the local folder choice"
    );
}

#[test]
fn workspace_files_metadata_does_not_download_content_and_member_versions_wait_for_review() {
    let alice = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-review", "alice"),
        "Alice",
        "A",
    );
    let bob = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-review", "bob"),
        "Bob",
        "B",
    );
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-reviewed-files-{}", unique_suffix()));
    let folder = root.join("alice");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("note.txt"),
        "small contents must not travel as git metadata",
    )
    .unwrap();
    let ar = alice
        .workspace_add_files_repo(&gid, "files", &folder.to_string_lossy())
        .unwrap();
    let v1 = alice
        .workspace_save_version(&gid, "files", "v1")
        .unwrap()
        .unwrap();
    let record = fs::read_to_string(Path::new(&ar.published_path).join("versions").join(&v1)).unwrap();
    assert!(!record.contains("small contents"), "a version names content by hash only: {record}");
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let br = bob
        .storage
        .get_workspace_repo(&gid, "files")
        .unwrap()
        .unwrap();
    assert!(
        br.work_path.is_none(),
        "announcements create only the published metadata repository"
    );
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v1.as_str())
    );
    let no_stream = |_: omni_mesh_runtime::MeshNodeAddress| -> omni_types::OmniResult<omni_mesh_runtime::BulkStream> { panic!("metadata-only member requested file content") };
    let sync = crate::app_workspace::files_work(&bob.storage, &gid, "files", &[], &no_stream).unwrap();
    assert_eq!(sync.content_total_bytes, 0);
    let details = workspace_files_command(
        &bob,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    assert_eq!(details.files.len(), 1);
    assert!(!details.files[0].available_here);
    assert_eq!(
        details.files[0].bytes,
        "small contents must not travel as git metadata".len() as u64
    );

    // Supply only the locally selected working copy; normal members never acquire all history.
    let bob_folder = root.join("bob");
    bob.workspace_take_repo(&gid, "files", &bob_folder.to_string_lossy())
        .unwrap();
    assert!(details.policy.participants_need_approval);
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(
        fs::read_to_string(bob_folder.join("note.txt")).unwrap(),
        "small contents must not travel as git metadata"
    );
    fs::write(bob_folder.join("note.txt"), "participant v2").unwrap();
    let file_state = |app: &OmniApp| {
        let details = workspace_files_command(app, serde_json::json!({"action":"details","group_id":gid,"repo":"files"})).unwrap();
        details.files.iter().map(|f| (f.path.clone(), f.state)).collect::<Vec<_>>()
    };
    use crate::app_workspace::custody::WorkspaceFileState as FileState;
    assert_eq!(file_state(&bob), vec![("note.txt".to_string(), FileState::Changed)], "changed in the folder, not saved");
    let v2 = bob
        .workspace_save_version(&gid, "files", "v2")
        .unwrap()
        .unwrap();
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v1.as_str())
    );
    assert_eq!(file_state(&bob), vec![("note.txt".to_string(), FileState::Saved)], "saved, the shared version lacks it");
    let own = |app: &OmniApp| {
        let card = app.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
        (card.own.map(|o| (o.commit.oid, o.count, o.state)), card.waiting.len())
    };
    use crate::app_workspace::WorkspaceOwnVersionState as OwnState;
    assert_eq!(own(&bob), (Some((v2.clone(), 1, OwnState::Waiting)), 0), "our own line is not among the others' waiting ones");
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    let pending = alice.workspace_main(&gid, "files").unwrap();
    assert_eq!(pending.oid.as_deref(), Some(v1.as_str()));
    assert!(pending.waiting.iter().any(|(_, o)| o == &v2));
    assert!(workspace_files_command(&bob,serde_json::json!({"action":"review","group_id":gid,"repo":"files","oid":v2,"accepted":true})).is_err());
    workspace_files_command(&alice,serde_json::json!({"action":"review","group_id":gid,"repo":"files","oid":v2,"accepted":true})).unwrap();
    assert_eq!(
        alice.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v2.as_str())
    );
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v2.as_str())
    );
    assert_eq!(own(&bob), (None, 0), "accepted: nothing of ours is outside the shared version");
    assert_eq!(file_state(&bob), vec![("note.txt".to_string(), FileState::Accepted)]);
    fs::write(bob_folder.join("note.txt"), "participant v3").unwrap();
    let v3 = bob
        .workspace_save_version(&gid, "files", "v3")
        .unwrap()
        .unwrap();
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v2.as_str()),
        "approval is for an exact version, not later descendants"
    );
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    workspace_files_command(&alice,serde_json::json!({"action":"review","group_id":gid,"repo":"files","oid":v3,"accepted":false})).unwrap();
    assert_eq!(
        alice.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v2.as_str())
    );
    assert!(!alice
        .workspace_main(&gid, "files")
        .unwrap()
        .waiting
        .iter()
        .any(|(_, o)| o == &v3));
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    assert_eq!(own(&bob), (Some((v3.clone(), 1, OwnState::Rejected)), 0), "the author sees the version was turned down");
    // «Вернуть к общей»: the rejected version leaves Bob's line, his folder follows the shared one.
    let back = serde_json::json!({"action":"back_to_shared","group_id":gid,"repo":"files"});
    workspace_files_command(&bob, back.clone()).unwrap();
    assert_eq!(own(&bob), (None, 0));
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bob_folder.join("note.txt")).unwrap(), "participant v2");
    assert_eq!(file_state(&bob), vec![("note.txt".to_string(), FileState::Accepted)]);
    assert!(workspace_files_command(&bob, back).is_err(), "nothing of ours is left to withdraw");

    workspace_files_command(&alice,serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":false,"newcomers_download_current":false})).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    fs::write(bob_folder.join("note.txt"), "participant v4").unwrap();
    let v4 = bob
        .workspace_save_version(&gid, "files", "v4")
        .unwrap()
        .unwrap();
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v4.as_str()),
        "the admin may enable immediate participant changes"
    );
    let bob_view = workspace_files_command(
        &bob,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    let bob_account = bob_view
        .members
        .iter()
        .find(|m| m.is_me)
        .unwrap()
        .account_id
        .clone();
    workspace_files_command(
        &alice,
        serde_json::json!({"action":"nominate","group_id":gid,"repo":"files","account_id":bob_account}),
    )
    .unwrap();
    workspace_files_command(&alice,serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":true,"newcomers_download_current":false})).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    let bob_view = workspace_files_command(
        &bob,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    let nomination = bob_view
        .members
        .iter()
        .find(|m| m.is_me)
        .unwrap()
        .nomination
        .clone()
        .unwrap();
    fs::write(bob_folder.join("note.txt"), "participant v4b before consent").unwrap();
    let v4b = bob
        .workspace_save_version(&gid, "files", "v4b")
        .unwrap()
        .unwrap();
    workspace_files_command(&bob,serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":root.join("custody")})).unwrap();
    deliver_group_feed(&bob, &[&alice]);
    let after_promotion = bob.workspace_main(&gid, "files").unwrap();
    assert_eq!(
        after_promotion.oid.as_deref(),
        Some(v4.as_str()),
        "a promotion does not accept the proposal made before it"
    );
    assert!(after_promotion.waiting.iter().any(|(_, o)| o == &v4b));
    fs::write(bob_folder.join("note.txt"), "custodian v5").unwrap();
    let v5 = bob
        .workspace_save_version(&gid, "files", "v5")
        .unwrap()
        .unwrap();
    assert_eq!(
        bob.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v5.as_str()),
        "a consenting custodian's own versions are automatic under moderation too"
    );
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    assert_eq!(
        alice.workspace_main(&gid, "files").unwrap().oid.as_deref(),
        Some(v5.as_str())
    );
}

#[test]
fn workspace_downloads_only_the_requested_file_and_preserves_a_changed_destination() {
    let alice = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-download", "alice"),
        "Alice",
        "A",
    );
    let bob = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-download", "bob"),
        "Bob",
        "B",
    );
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-selected-file-{}", unique_suffix()));
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), "selected").unwrap();
    fs::write(source.join("b.prt"), vec![42_u8; 4096]).unwrap();
    alice
        .workspace_add_files_repo(&gid, "files", &source.to_string_lossy())
        .unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    bob.workspace_main(&gid, "files").unwrap();
    let br = bob
        .storage
        .get_workspace_repo(&gid, "files")
        .unwrap()
        .unwrap();
    let target = root.join("downloaded.txt");
    workspace_files_command(&bob,serde_json::json!({"action":"download","group_id":gid,"repo":"files","path":"a.txt","target":target})).unwrap();
    let open = workspace_stream_from(&alice, &gid, "files");
    let nodes = [omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20])];
    crate::app_workspace::process_download(&bob.storage, &br, &nodes, &open).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "selected");
    let view = workspace_files_command(
        &bob,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    assert!(view.work_path.is_none());
    // The downloaded copy is the person's own file: nothing is kept twice, nothing else came.
    assert!(
        !view
            .files
            .iter()
            .find(|f| f.path == "a.txt")
            .unwrap()
            .available_here
    );
    assert!(
        !view
            .files
            .iter()
            .find(|f| f.path == "b.prt")
            .unwrap()
            .available_here
    );
    let second = root.join("changed-destination.prt");
    workspace_files_command(&bob,serde_json::json!({"action":"download","group_id":gid,"repo":"files","path":"b.prt","target":second})).unwrap();
    fs::write(&second, "a new local file").unwrap();
    crate::app_workspace::process_download(&bob.storage, &br, &nodes, &open).unwrap();
    assert_eq!(fs::read_to_string(&second).unwrap(), "a new local file");
    workspace_files_command(
        &bob,
        serde_json::json!({"action":"cancel_download","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    assert_eq!(
        bob.storage
            .get_workspace_file_local(&gid, "files", "download")
            .unwrap()
            .as_deref(),
        Some("")
    );
}

#[test]
fn workspace_accepts_a_stale_proposal_without_overwriting_the_accepted_file() {
    let alice = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-stale", "alice"),
        "Alice",
        "A",
    );
    let bob = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-stale", "bob"),
        "Bob",
        "B",
    );
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-stale-file-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    fs::write(af.join("part.prt"), "original").unwrap();
    let ar = alice
        .workspace_add_files_repo(&gid, "files", &af.to_string_lossy())
        .unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    bob.workspace_main(&gid, "files").unwrap();
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy())
        .unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "original");
    fs::write(bf.join("part.prt"), "participant's proposal").unwrap();
    let proposal = bob
        .workspace_save_version(&gid, "files", "Bob's version")
        .unwrap()
        .unwrap();
    fs::write(af.join("part.prt"), "admin's accepted version").unwrap();
    let accepted = alice
        .workspace_save_version(&gid, "files", "Admin's version")
        .unwrap()
        .unwrap();
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    let before = workspace_files_command(
        &alice,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    assert!(before
        .proposals
        .iter()
        .any(|p| p.oid == proposal && p.stale));
    workspace_files_command(&alice,serde_json::json!({"action":"review","group_id":gid,"repo":"files","oid":proposal,"accepted":true})).unwrap();
    let merged = alice.workspace_main(&gid, "files").unwrap().oid.unwrap();
    let path = Path::new(&ar.published_path);
    let tree = |id: &str| crate::app_workspace::test_tree(path, "files", id);
    assert_eq!(tree(&merged)["part.prt"], tree(&accepted)["part.prt"]);
    let view = workspace_files_command(
        &alice,
        serde_json::json!({"action":"details","group_id":gid,"repo":"files"}),
    )
    .unwrap();
    let copy = view
        .files
        .iter()
        .find(|f| f.path != "part.prt")
        .expect("the participant's conflicting content is kept alongside");
    assert_eq!(tree(&merged)[&copy.path], tree(&proposal)["part.prt"]);
}

/// «Убрать папку с этого устройства»: the folder keeps its files and stops following; bound again,
/// it is not taken for the person's edits (the next version comes in), and a change made in it
/// meanwhile still counts as one.
#[test]
fn workspace_removed_folder_keeps_its_files_and_follows_again_when_bound_back() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-unbind", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-unbind", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-unbind-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    fs::write(af.join("part.prt"), "one").unwrap();
    fs::write(af.join("notes.txt"), "notes").unwrap();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "one");

    workspace_files_command(&bob, serde_json::json!({"action":"unbind","group_id":gid,"repo":"files"})).unwrap();
    assert!(bob.storage.get_workspace_repo(&gid, "files").unwrap().unwrap().work_path.is_none());
    fs::write(af.join("part.prt"), "two").unwrap();
    alice.workspace_save_version(&gid, "files", "v2").unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "one", "a removed folder is left as it is");
    // The admin's rule «newcomers get the current version» does not bring it back either.
    workspace_files_command(&alice, serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":true,"newcomers_download_current":true})).unwrap();
    workspace_files_command(&alice, serde_json::json!({"action":"root","group_id":gid,"folder":root.join("roots").to_string_lossy()})).unwrap();
    workspace_files_command(&bob, serde_json::json!({"action":"root","group_id":gid,"folder":root.join("bob-root").to_string_lossy()})).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert!(bob.storage.get_workspace_repo(&gid, "files").unwrap().unwrap().work_path.is_none(), "not bound again on its own");

    fs::write(bf.join("notes.txt"), "edited while away").unwrap();
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    let card = |app: &OmniApp| app.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
    assert!(!card(&bob).folder_current, "the folder still holds v1");
    let sync = workspace_exchange(&alice, &bob, &gid, "files");
    assert!(sync.waiting_changed.is_empty(), "the old files are not taken for edits: {sync:?}");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "two", "the next version comes in");
    assert_eq!(fs::read_to_string(bf.join("notes.txt")).unwrap(), "edited while away");
    let card = card(&bob);
    assert!(card.folder_current);
    assert_eq!(card.changed, vec!["notes.txt".to_string()], "the change made meanwhile is the person's own");
}

/// The project's history (stage 3 of docs/decisions/workspace-files-storage.md): an admin releases
/// revision A and keeps a version for good, members see both; a member may not release; a taken
/// designation is refused; «Восстановить» is a new version that goes through acceptance.
#[test]
fn workspace_history_carries_revisions_and_a_restore_is_a_new_version() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-history", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-history", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-history-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    let mut ids = Vec::new();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    for text in ["one", "two", "three"] {
        fs::write(af.join("part.prt"), text).unwrap();
        ids.push(alice.workspace_save_version(&gid, "files", text).unwrap().unwrap());
    }
    let history = |app: &OmniApp| workspace_files_command(app, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    let first = history(&alice);
    assert_eq!(first.history.iter().map(|h| (h.number, h.subject.as_str(), h.current)).collect::<Vec<_>>(), vec![(3, "three", true), (2, "two", false), (1, "one", false)]);
    assert_eq!(first.next_revision, "A");

    workspace_files_command(&alice, serde_json::json!({"action":"release","group_id":gid,"repo":"files","oid":ids[1],"label":"A","note":"для заказчика"})).unwrap();
    let kept = workspace_files_command(&alice, serde_json::json!({"action":"keep","group_id":gid,"repo":"files","oid":ids[0],"keep":true})).unwrap();
    assert_eq!(kept.next_revision, "B");
    assert!(workspace_files_command(&alice, serde_json::json!({"action":"release","group_id":gid,"repo":"files","oid":ids[2],"label":"a"})).is_err(), "A is taken");

    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let seen = history(&bob);
    let v2 = seen.history.iter().find(|h| h.number == 2).unwrap();
    assert_eq!((v2.revision.as_deref(), v2.revision_note.as_str()), (Some("A"), "для заказчика"));
    assert!(seen.history.iter().find(|h| h.number == 1).unwrap().kept);
    assert!(workspace_files_command(&bob, serde_json::json!({"action":"release","group_id":gid,"repo":"files","oid":ids[2],"label":"B"})).is_err(), "a member does not release");

    // Bob brings v1 back: a new version on top, waiting for the admin as any save of his.
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    workspace_files_command(&bob, serde_json::json!({"action":"restore","group_id":gid,"repo":"files","oid":ids[0]})).unwrap();
    let card = bob.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
    let own = card.own.expect("the restore is Bob's version outside the shared one");
    assert_eq!((own.commit.subject.as_str(), own.state), ("Восстановлена в.1 «one»", crate::app_workspace::WorkspaceOwnVersionState::Waiting));
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "one", "Bob's folder follows his version");
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    workspace_files_command(&alice, serde_json::json!({"action":"review","group_id":gid,"repo":"files","oid":own.commit.oid,"accepted":true})).unwrap();
    workspace_exchange(&bob, &alice, &gid, "files");
    assert_eq!(fs::read_to_string(af.join("part.prt")).unwrap(), "one", "accepted, the old files are the current ones");
    assert_eq!(history(&alice).history[0].number, 4, "nothing was overwritten: v4 on top");
}

#[test]
fn workspace_thinning_waits_for_the_admins_rule_and_spares_revisions_and_kept_versions() {
    use sha2::Digest as _;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-thinning", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-thinning", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-thinning-{}", unique_suffix()));
    let af = root.join("alice");
    fs::create_dir_all(&af).unwrap();
    let record = alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    fs::write(af.join("base.prt"), "base").unwrap();
    let mut ids = Vec::new();
    for text in ["one", "two", "three", "four", "five", "six"] {
        fs::write(af.join("part.prt"), text).unwrap();
        ids.push(alice.workspace_save_version(&gid, "files", text).unwrap().unwrap());
    }
    let stored = |text: &str| crate::app_workspace::content::existing_object_path(Path::new(&record.published_path), &hex::encode(sha2::Sha256::digest(text.as_bytes()))).is_file();
    let round = |app: &OmniApp| {
        let open = |_| Err(omni_types::OmniError::Transport("no members here".into()));
        crate::app_workspace::files_work(&app.storage, &gid, "files", &[], &open).unwrap();
    };
    let command = |app: &OmniApp, json: serde_json::Value| workspace_files_command(app, json);
    // v1 is kept for good, v3 is revision A.
    command(&alice, serde_json::json!({"action":"keep","group_id":gid,"repo":"files","oid":ids[0],"keep":true})).unwrap();
    command(&alice, serde_json::json!({"action":"release","group_id":gid,"repo":"files","oid":ids[2],"label":"A"})).unwrap();

    // No rule confirmed yet: the default is offered, nothing is thinned out.
    round(&alice);
    let history = command(&alice, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    assert_eq!((history.keep_last, history.keep_days, history.retention_confirmed), (10, 30, false));
    assert!(history.history.iter().all(|h| !h.thinned));
    assert!(["one", "two", "three", "four", "five", "six", "base"].iter().all(|t| stored(t)), "the archive holds every version until a rule is confirmed");

    // The report: the last two versions and no days keep v6, v5 and the protected v3, v1.
    let report = command(&alice, serde_json::json!({"action":"thinning","group_id":gid,"repo":"files","keep_last":2,"keep_days":0})).unwrap().thinning.unwrap();
    assert_eq!(report.versions.iter().map(|v| v.number).collect::<Vec<_>>(), vec![4, 2]);
    assert_eq!(report.freed_bytes, ("four".len() + "two".len()) as u64);
    assert!(command(&bob, serde_json::json!({"action":"thinning","group_id":gid,"repo":"files","keep_last":2,"keep_days":0})).is_err(), "only an admin");
    round(&alice);
    assert!(stored("two"), "a report removes nothing");

    let confirmed = command(&alice, serde_json::json!({"action":"retain","group_id":gid,"repo":"files","keep_last":2,"keep_days":0})).unwrap();
    assert_eq!((confirmed.keep_last, confirmed.keep_days, confirmed.retention_confirmed), (2, 0, true));
    assert_eq!(confirmed.history.iter().filter(|h| h.thinned).map(|h| h.number).collect::<Vec<_>>(), vec![4, 2]);
    round(&alice);
    assert!(!stored("two") && !stored("four"), "thinned-out content is removed");
    assert!(stored("one") && stored("three") && stored("five") && stored("six") && stored("base"), "kept, revision, recent and the file every version has stay");

    // Every node works the same out from the journal; an old version without content is not restored.
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let seen = command(&bob, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    assert_eq!(seen.history.iter().filter(|h| h.thinned).map(|h| h.number).collect::<Vec<_>>(), vec![4, 2]);
    assert!(command(&bob, serde_json::json!({"action":"restore","group_id":gid,"repo":"files","oid":ids[1]})).is_err());
    assert!(command(&bob, serde_json::json!({"action":"retain","group_id":gid,"repo":"files","keep_last":1,"keep_days":0})).is_err(), "only an admin");

    // A wider rule brings versions back under keeping.
    let wider = command(&alice, serde_json::json!({"action":"thinning","group_id":gid,"repo":"files","keep_last":10,"keep_days":30})).unwrap().thinning.unwrap();
    assert_eq!((wider.versions.len(), wider.restored_versions, wider.freed_bytes), (0, 2, 0));
}

#[test]
fn workspace_masked_files_keep_only_the_current_version_and_a_restore_leaves_them_as_they_are() {
    use sha2::Digest as _;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-masks", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-masks", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-masks-{}", unique_suffix()));
    let af = root.join("alice");
    fs::create_dir_all(&af).unwrap();
    let record = alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    let mut ids = Vec::new();
    for n in 1..=4 {
        fs::write(af.join("model.prt"), format!("model {n}")).unwrap();
        fs::write(af.join("Out.STL"), format!("export {n}")).unwrap();
        ids.push(alice.workspace_save_version(&gid, "files", &format!("v{n}")).unwrap().unwrap());
    }
    let stored = |text: &str| crate::app_workspace::content::existing_object_path(Path::new(&record.published_path), &hex::encode(sha2::Sha256::digest(text.as_bytes()))).is_file();
    let round = |app: &OmniApp| {
        let open = |_| Err(omni_types::OmniError::Transport("no members here".into()));
        crate::app_workspace::files_work(&app.storage, &gid, "files", &[], &open).unwrap();
    };
    let command = |app: &OmniApp, json: serde_json::Value| workspace_files_command(app, json);
    command(&alice, serde_json::json!({"action":"release","group_id":gid,"repo":"files","oid":ids[1],"label":"A"})).unwrap();

    // Every version stays under the rule; only the current one and revision A keep their export.
    let rule = serde_json::json!({"action":"thinning","group_id":gid,"repo":"files","keep_last":10,"keep_days":30,"only_current":[" *.stl ", "*.STL", ""]});
    let report = command(&alice, rule).unwrap().thinning.unwrap();
    assert_eq!((report.versions.len(), report.partial_versions, report.only_current.clone()), (0, 2, vec!["*.stl".to_string()]));
    assert_eq!(report.freed_bytes, ("export 1".len() + "export 3".len()) as u64);
    command(&alice, serde_json::json!({"action":"retain","group_id":gid,"repo":"files","keep_last":10,"keep_days":30,"only_current":["*.stl"]})).unwrap();
    round(&alice);
    assert!(!stored("export 1") && !stored("export 3"), "older exports are removed");
    assert!(stored("export 2") && stored("export 4"), "the revision's and the current export stay");
    assert!((1..=4).all(|n| stored(&format!("model {n}"))), "the models of every kept version stay");
    let history = command(&alice, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    assert_eq!(history.only_current, vec!["*.stl".to_string()]);
    assert_eq!(history.history.iter().filter(|h| h.partial).map(|h| h.number).collect::<Vec<_>>(), vec![3, 1]);
    assert!(history.history.iter().all(|h| !h.thinned));

    // The same on another node, from the journal alone.
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let seen = command(&bob, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    assert_eq!(seen.history.iter().filter(|h| h.partial).map(|h| h.number).collect::<Vec<_>>(), vec![3, 1]);

    // v1 comes back with its model; its export, gone, stays the current one.
    command(&alice, serde_json::json!({"action":"restore","group_id":gid,"repo":"files","oid":ids[0]})).unwrap();
    round(&alice);
    let history = command(&alice, serde_json::json!({"action":"history","group_id":gid,"repo":"files"})).unwrap();
    assert_eq!((history.history[0].number, history.history[0].changed), (5, 1));
    assert_eq!(fs::read_to_string(af.join("model.prt")).unwrap(), "model 1");
    assert_eq!(fs::read_to_string(af.join("Out.STL")).unwrap(), "export 4");
}

#[test]
fn workspace_a_folder_left_out_leaves_the_disk_skips_new_versions_and_comes_back() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-left-out", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-left-out", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-left-out-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    for dir in [af.join("model"), af.join("export")] {
        fs::create_dir_all(dir).unwrap();
    }
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    // A member's versions are taken at once here: the test is about Bob's folder.
    workspace_files_command(&alice, serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":false,"newcomers_download_current":false})).unwrap();
    fs::write(af.join("model/a.prt"), "model 1").unwrap();
    fs::write(af.join("export/a.stl"), "export 1").unwrap();
    fs::write(af.join("readme.txt"), "readme 1").unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap().unwrap();
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("export/a.stl")).unwrap(), "export 1");

    // Bob leaves «export» out: it goes from his disk, and nothing reads as changed.
    let set = workspace_files_command(&bob, serde_json::json!({"action":"leave_out","group_id":gid,"repo":"files","folders":["export/", "export/sub", " "]})).unwrap();
    assert_eq!(set.left_out, vec!["export".to_string()]);
    assert_eq!(set.folders.iter().map(|f| (f.path.as_str(), f.bytes)).collect::<Vec<_>>(), vec![("export", 8), ("model", 7)]);
    workspace_exchange(&alice, &bob, &gid, "files");
    assert!(!bf.join("export").exists(), "the emptied folder goes too");
    assert_eq!(fs::read_to_string(bf.join("model/a.prt")).unwrap(), "model 1");
    let card = |app: &OmniApp| app.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
    assert_eq!(card(&bob).changed_count, 0);
    let details = workspace_files_command(&bob, serde_json::json!({"action":"details","group_id":gid,"repo":"files"})).unwrap();
    assert!(details.files.iter().find(|f| f.path == "export/a.stl").unwrap().left_out);

    // A new version: Bob's folder takes all but «export».
    fs::write(af.join("model/a.prt"), "model 2").unwrap();
    fs::write(af.join("export/a.stl"), "export 2").unwrap();
    alice.workspace_save_version(&gid, "files", "v2").unwrap().unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("model/a.prt")).unwrap(), "model 2");
    assert!(!bf.join("export").join("a.stl").exists());
    assert!(card(&bob).folder_current);
    assert_eq!(card(&bob).left_out, vec!["export".to_string()]);

    // Bob's own save keeps Alice's export as the version has it.
    fs::write(bf.join("readme.txt"), "readme by Bob").unwrap();
    let bobs = bob.workspace_save_version(&gid, "files", "Bob").unwrap().unwrap();
    let tree = crate::app_workspace::test_tree(Path::new(&bob.storage.get_workspace_repo(&gid, "files").unwrap().unwrap().published_path), "files", &bobs);
    assert_eq!(tree.len(), 3, "nothing of «export» was dropped: {tree:?}");
    use sha2::Digest as _;
    assert_eq!(tree["export/a.stl"], hex::encode(sha2::Sha256::digest(b"export 2")));

    // Taken back: the current export comes.
    workspace_files_command(&bob, serde_json::json!({"action":"leave_out","group_id":gid,"repo":"files","folders":[]})).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("export/a.stl")).unwrap(), "export 2");
    assert_eq!(fs::read_to_string(bf.join("readme.txt")).unwrap(), "readme by Bob");
    assert_eq!(card(&bob).changed_count, 0);
}

#[test]
fn workspace_a_taken_file_is_read_only_for_the_others_until_given_back_and_a_later_change_goes_aside() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-locks", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-locks", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-locks-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    let policy = |strict: bool| serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":false,"newcomers_download_current":false,"edit_only_taken":strict});
    workspace_files_command(&alice, policy(false)).unwrap();
    fs::write(af.join("a.prt"), "a 1").unwrap();
    fs::write(af.join("b.prt"), "b 1").unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap().unwrap();
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    let read_only = |path: &Path| fs::metadata(path).unwrap().permissions().readonly();
    let command = |app: &OmniApp, action: serde_json::Value| {
        let mut json = serde_json::json!({"group_id":gid,"repo":"files"});
        json.as_object_mut().unwrap().extend(action.as_object().unwrap().clone());
        workspace_files_command(app, json)
    };
    let card = |app: &OmniApp| app.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
    let lock_of = |app: &OmniApp, path: &str| {
        command(app, serde_json::json!({"action":"details"})).unwrap().files.into_iter().find(|f| f.path == path).unwrap()
    };

    // Bob takes a.prt: at Alice it is his and read-only; b.prt stays as it was.
    command(&bob, serde_json::json!({"action":"take","paths":["a.prt"]})).unwrap();
    assert_eq!(card(&bob).taken, vec!["a.prt".to_string()]);
    assert!(!read_only(&bf.join("a.prt")));
    workspace_exchange(&bob, &alice, &gid, "files");
    let lock = lock_of(&alice, "a.prt").lock.unwrap();
    assert_eq!((lock.name.as_str(), lock.mine), ("Bob", false));
    assert!(read_only(&af.join("a.prt")), "someone else's file is read-only");
    assert!(!read_only(&af.join("b.prt")));
    let refused = command(&alice, serde_json::json!({"action":"take","paths":["a.prt"]})).unwrap_err().to_string();
    assert!(refused.contains("taken by Bob"), "{refused}");

    // Saving keeps the take; giving it back frees the file at Alice, with Bob's change in it.
    fs::write(bf.join("a.prt"), "a by Bob").unwrap();
    bob.workspace_save_version(&gid, "files", "Bob").unwrap().unwrap();
    assert_eq!(card(&bob).taken, vec!["a.prt".to_string()]);
    command(&bob, serde_json::json!({"action":"give","paths":["a.prt"]})).unwrap();
    workspace_exchange(&bob, &alice, &gid, "files");
    assert!(lock_of(&alice, "a.prt").lock.is_none());
    assert!(!read_only(&af.join("a.prt")));
    assert_eq!(fs::read_to_string(af.join("a.prt")).unwrap(), "a by Bob");

    // «Править, только взяв в работу»: everything Bob has not taken is read-only at him; an admin
    // gives back his take.
    workspace_files_command(&alice, policy(true)).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert!(read_only(&bf.join("a.prt")) && read_only(&bf.join("b.prt")));
    command(&bob, serde_json::json!({"action":"take","paths":["b.prt"]})).unwrap();
    assert!(!read_only(&bf.join("b.prt")) && read_only(&bf.join("a.prt")));
    workspace_exchange(&bob, &alice, &gid, "files");
    command(&alice, serde_json::json!({"action":"unlock","path":"b.prt","account_id":lock.account_id})).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert!(card(&bob).taken.is_empty());
    assert!(read_only(&bf.join("b.prt")));

    // Both took a.prt without hearing each other: Alice first. Bob's change goes aside and his a.prt
    // comes back to the version, so his folder can follow Alice's.
    workspace_files_command(&alice, policy(false)).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    command(&alice, serde_json::json!({"action":"take","paths":["a.prt"]})).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    command(&bob, serde_json::json!({"action":"take","paths":["a.prt"]})).unwrap();
    fs::write(bf.join("a.prt"), "a by Bob, offline").unwrap();
    deliver_group_feed(&bob, &[&alice]);
    workspace_exchange(&alice, &bob, &gid, "files");
    let at_bob = lock_of(&bob, "a.prt");
    assert_eq!(at_bob.lock.as_ref().map(|l| l.name.as_str()), Some("Alice"));
    assert!(at_bob.taken_after);
    assert_eq!(card(&bob).held_elsewhere.iter().map(|h| (h.path.as_str(), h.name.as_str())).collect::<Vec<_>>(), vec![("a.prt", "Alice")]);
    command(&bob, serde_json::json!({"action":"aside","path":"a.prt"})).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("a.prt")).unwrap(), "a by Bob");
    let aside: Vec<String> = fs::read_dir(&bf).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("a (Bob, ")).collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(fs::read_to_string(bf.join(&aside[0])).unwrap(), "a by Bob, offline");
    assert!(read_only(&bf.join("a.prt")));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_a_file_history_lists_its_versions_and_a_preview_comes_to_a_member_without_a_folder() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-preview", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-preview", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-preview-{}", unique_suffix()));
    let af = root.join("alice");
    fs::create_dir_all(&af).unwrap();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    fs::write(af.join("sheet.png"), "picture 1").unwrap();
    fs::write(af.join("other.txt"), "other").unwrap();
    alice.workspace_save_version(&gid, "files", "first").unwrap().unwrap();
    fs::write(af.join("other.txt"), "other 2").unwrap();
    alice.workspace_save_version(&gid, "files", "other only").unwrap().unwrap();
    fs::write(af.join("sheet.png"), "picture 2").unwrap();
    alice.workspace_save_version(&gid, "files", "second picture").unwrap().unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let command = |app: &OmniApp, action: serde_json::Value| {
        let mut json = serde_json::json!({"group_id":gid,"repo":"files"});
        json.as_object_mut().unwrap().extend(action.as_object().unwrap().clone());
        workspace_files_command(app, json).unwrap()
    };

    // Only the versions that changed the file, newest first.
    let history = command(&bob, serde_json::json!({"action":"file_history","path":"sheet.png"})).file_history;
    assert_eq!(history.iter().map(|h| (h.number, h.subject.as_str(), h.bytes)).collect::<Vec<_>>(), vec![(3, "second picture", Some(9)), (1, "first", Some(9))]);

    // At Alice the content is here at once; Bob has no folder: it comes with the next round.
    let here = command(&alice, serde_json::json!({"action":"preview","path":"sheet.png"})).preview.unwrap();
    assert_eq!(fs::read_to_string(here.file.unwrap()).unwrap(), "picture 2");
    let first = command(&bob, serde_json::json!({"action":"preview","path":"sheet.png","oid":history[1].oid})).preview.unwrap();
    assert!(first.pending && first.file.is_none());
    workspace_exchange(&alice, &bob, &gid, "files");
    let first = command(&bob, serde_json::json!({"action":"preview","path":"sheet.png","oid":history[1].oid})).preview.unwrap();
    assert!(!first.pending);
    assert_eq!(fs::read_to_string(first.file.unwrap()).unwrap(), "picture 1");

    // To drag out or open: the current file at Alice is her folder's own; an old version (and any at
    // Bob, who has no folder) is a read-only copy under the file's name.
    let current = command(&alice, serde_json::json!({"action":"export","path":"sheet.png"})).export.unwrap();
    assert_eq!(PathBuf::from(current.file.unwrap()), af.join("sheet.png"));
    let old = command(&bob, serde_json::json!({"action":"export","path":"sheet.png","oid":history[1].oid})).export.unwrap();
    let old = PathBuf::from(old.file.unwrap());
    assert_eq!(old.file_name().unwrap(), "sheet.png");
    assert_eq!(fs::read_to_string(&old).unwrap(), "picture 1");
    assert!(fs::metadata(&old).unwrap().permissions().readonly());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_choosing_the_final_file_of_a_conflict_is_a_version_that_removes_the_copies() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-choose", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-choose", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-choose-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    workspace_files_command(&alice, serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":false,"newcomers_download_current":false})).unwrap();
    fs::write(af.join("a.prt"), "a 1").unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap().unwrap();
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    let details = |app: &OmniApp| workspace_files_command(app, serde_json::json!({"action":"details","group_id":gid,"repo":"files"})).unwrap();
    let names = |dir: &Path| {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
        names.sort();
        names
    };

    // Both change a.prt from the same version: one line wins, the other's change is kept beside.
    let conflict = |alices: &str, bobs: &str| {
        fs::write(af.join("a.prt"), alices).unwrap();
        fs::write(bf.join("a.prt"), bobs).unwrap();
        alice.workspace_save_version(&gid, "files", "Alice").unwrap().unwrap();
        bob.workspace_save_version(&gid, "files", "Bob").unwrap().unwrap();
        for _ in 0..3 {
            workspace_exchange(&alice, &bob, &gid, "files");
            workspace_exchange(&bob, &alice, &gid, "files");
        }
        let conflicts = details(&alice).conflicts;
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].path, "a.prt");
        assert_eq!(details(&bob).conflicts.len(), 1);
        assert_eq!(names(&af), names(&bf));
        conflicts[0].copies[0].clone()
    };
    let copy = conflict("a by Alice", "a by Bob");
    let card = |app: &OmniApp| app.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap();
    assert_eq!(card(&bob).conflicts, vec!["a.prt".to_string()]);
    let kept_beside = fs::read_to_string(af.join(&copy.path)).unwrap();

    // Bob takes the copy as the final one: at both, a.prt holds it and the copy is gone.
    workspace_files_command(&bob, serde_json::json!({"action":"choose","group_id":gid,"repo":"files","path":"a.prt","chosen":copy.path})).unwrap();
    for _ in 0..2 {
        workspace_exchange(&bob, &alice, &gid, "files");
        workspace_exchange(&alice, &bob, &gid, "files");
    }
    for dir in [&af, &bf] {
        assert_eq!(fs::read_to_string(dir.join("a.prt")).unwrap(), kept_beside);
        assert_eq!(names(dir), vec!["a.prt".to_string()]);
    }
    assert!(details(&alice).conflicts.is_empty());
    assert!(card(&bob).conflicts.is_empty());

    // Again, and this time a.prt stays as it is.
    let copy = conflict("a by Alice 2", "a by Bob 2");
    let before = fs::read_to_string(af.join("a.prt")).unwrap();
    let refused = workspace_files_command(&alice, serde_json::json!({"action":"choose","group_id":gid,"repo":"files","path":"a.prt","chosen":"other.prt"})).unwrap_err().to_string();
    assert!(refused.contains("not a copy of a.prt"), "{refused}");
    workspace_files_command(&alice, serde_json::json!({"action":"choose","group_id":gid,"repo":"files","path":"a.prt","chosen":"a.prt"})).unwrap();
    for _ in 0..2 {
        workspace_exchange(&alice, &bob, &gid, "files");
        workspace_exchange(&bob, &alice, &gid, "files");
    }
    for dir in [&af, &bf] {
        assert_eq!(fs::read_to_string(dir.join("a.prt")).unwrap(), before);
        assert!(!dir.join(&copy.path).exists());
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_a_revision_under_the_check_rule_waits_for_another_reviewers_approval() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-check", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-check", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-workspace-check-{}", unique_suffix()));
    let af = root.join("alice");
    fs::create_dir_all(&af).unwrap();
    fs::write(af.join("sheet.pdf"), "rev 1").unwrap();
    alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    let v1 = alice.workspace_save_version(&gid, "files", "v1").unwrap().unwrap();
    let command = |app: &OmniApp, mut json: serde_json::Value| {
        json["group_id"] = gid.clone().into();
        json["repo"] = "files".into();
        workspace_files_command(app, json)
    };
    let policy = |checked: bool| serde_json::json!({"action":"policy","participants_need_approval":true,"newcomers_download_current":false,"revisions_checked":checked});
    // A revision released before the rule needs nothing.
    command(&alice, serde_json::json!({"action":"release","oid":v1,"label":"A"})).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    command(&alice, policy(true)).unwrap();
    let entry = |app: &OmniApp, oid: &str| command(app, serde_json::json!({"action":"history"})).unwrap().history.into_iter().find(|e| e.oid == oid).unwrap();
    assert!(!entry(&alice, &v1).revision_pending, "released before the rule");

    // Bob becomes the project's custodian.
    let bob_account = command(&bob, serde_json::json!({"action":"details"})).unwrap().members.into_iter().find(|m| m.is_me).unwrap().account_id;
    command(&alice, serde_json::json!({"action":"nominate","account_id":bob_account})).unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    deliver_group_feed(&alice, &[&bob]);
    let nomination = command(&bob, serde_json::json!({"action":"details"})).unwrap().members.into_iter().find(|m| m.is_me).unwrap().nomination.unwrap();
    command(&bob, serde_json::json!({"action":"consent","nomination":nomination,"accepted":true,"folder":root.join("custody")})).unwrap();
    deliver_group_feed(&bob, &[&alice]);

    // Under the rule: on check until someone other than the releaser approves it.
    fs::write(af.join("sheet.pdf"), "rev 2").unwrap();
    let v2 = alice.workspace_save_version(&gid, "files", "v2").unwrap().unwrap();
    command(&alice, serde_json::json!({"action":"release","oid":v2,"label":"B"})).unwrap();
    let mine = entry(&alice, &v2);
    assert!(mine.revision_pending && mine.revision_mine);
    assert!(command(&alice, serde_json::json!({"action":"approve_revision","oid":v2})).is_err(), "not by who released it");
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    deliver_group_feed(&alice, &[&bob]);
    let theirs = entry(&bob, &v2);
    assert!(theirs.revision_pending && !theirs.revision_mine);
    command(&bob, serde_json::json!({"action":"approve_revision","oid":v2})).unwrap();
    deliver_group_feed(&bob, &[&alice]);
    assert!(!entry(&alice, &v2).revision_pending, "approved");

    // A new designation needs its own approval.
    command(&alice, serde_json::json!({"action":"release","oid":v2,"label":"C"})).unwrap();
    assert!(entry(&alice, &v2).revision_pending);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_tasks_are_files_two_changes_of_one_task_both_stay_and_a_version_comment_closes_it() {
    const TASKS: &str = crate::app_workspace::tasks::TASKS_REPO;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-tasks", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-tasks", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let command = |app: &OmniApp, mut json: serde_json::Value| {
        json["group_id"] = gid.clone().into();
        workspace_files_command(app, json).map(|d| d.tasks.unwrap())
    };
    let both_ways = || {
        for _ in 0..2 {
            workspace_exchange(&alice, &bob, &gid, TASKS);
            workspace_exchange(&bob, &alice, &gid, TASKS);
        }
    };
    assert!(command(&bob, serde_json::json!({"action":"tasks_enable"})).is_err(), "an admin turns tasks on");
    assert!(command(&bob, serde_json::json!({"action":"task_new","title":"x"})).is_err(), "not on yet");
    let board = command(&alice, serde_json::json!({"action":"tasks_enable"})).unwrap();
    assert!(board.enabled && board.tasks.is_empty() && board.folder.is_some());
    let board = command(&alice, serde_json::json!({"action":"task_new","title":"Отверстие 12 мм","body":"По чертежу.","labels":["корпус"]})).unwrap();
    let id = board.tasks[0].id.clone();
    assert_eq!((board.tasks[0].number, board.tasks[0].status.as_str(), board.tasks[0].labels.clone()), (1, "new", vec!["корпус".to_string()]));
    deliver_group_feed(&alice, &[&bob]);
    both_ways();
    let seen = command(&bob, serde_json::json!({"action":"tasks"})).unwrap();
    assert_eq!(seen.tasks.len(), 1, "Bob's own tasks folder follows the project");
    let bob_account = seen.members.iter().find(|m| m.is_me).unwrap().account_id.clone();

    // At once: Bob takes it in work, Alice gives it to Bob — both changes stay.
    command(&bob, serde_json::json!({"action":"task_change","task":id,"status":"doing"})).unwrap();
    command(&alice, serde_json::json!({"action":"task_change","task":id,"assignee":bob_account})).unwrap();
    both_ways();
    for app in [&alice, &bob] {
        let task = command(app, serde_json::json!({"action":"tasks"})).unwrap().tasks.remove(0);
        assert_eq!((task.status.as_str(), task.assignee.as_deref(), task.conflict), ("doing", Some(bob_account.as_str()), false));
    }
    assert!(command(&alice, serde_json::json!({"action":"task_change","task":id,"status":"later"})).is_err());

    // A column's order: the latest on top, until a card is dragged between others; both see it.
    let column = |app: &OmniApp, status: &str| {
        let board = command(app, serde_json::json!({"action":"tasks"})).unwrap();
        let mut here: Vec<_> = board.tasks.iter().filter(|t| t.status == status).collect();
        here.sort_by(|a, b| a.place.total_cmp(&b.place));
        here.iter().map(|t| t.number).collect::<Vec<u32>>()
    };
    let second = command(&alice, serde_json::json!({"action":"task_new","title":"Второе"})).unwrap().tasks[1].id.clone();
    let third = command(&alice, serde_json::json!({"action":"task_new","title":"Третье"})).unwrap().tasks[2].id.clone();
    assert_eq!(column(&alice, "new"), vec![3, 2]);
    command(&alice, serde_json::json!({"action":"task_change","task":second,"before":third})).unwrap();
    assert_eq!(column(&alice, "new"), vec![2, 3]);
    command(&alice, serde_json::json!({"action":"task_change","task":id,"status":"new","before":""})).unwrap();
    command(&alice, serde_json::json!({"action":"task_change","task":third,"before":id})).unwrap();
    both_ways();
    for app in [&alice, &bob] {
        assert_eq!(column(app, "new"), vec![2, 3, 1]);
    }
    assert!(command(&alice, serde_json::json!({"action":"task_change","task":id,"before":"nope"})).is_err());

    command(&bob, serde_json::json!({"action":"task_comment","task":id,"text":"Сделал, посмотрите."})).unwrap();
    both_ways();
    let task = command(&alice, serde_json::json!({"action":"tasks"})).unwrap().tasks.remove(0);
    assert_eq!(task.comments.iter().map(|c| (c.author_name.as_str(), c.text.as_str(), c.mine)).collect::<Vec<_>>(), vec![("Bob", "Сделал, посмотрите.", false)]);

    // Two of Bob's saves that arrive together are two lines in Alice's thread, not just the last.
    // Alice's thread is quiet for a while after the group starts (the burst of what is already there).
    omni_storage::Storage::open_keyed_connection(alice.storage.db_path())
        .unwrap()
        .execute("UPDATE groups SET created_at_unix_ms = created_at_unix_ms - 600000 WHERE group_id = ?1", [&gid])
        .unwrap();
    command(&bob, serde_json::json!({"action":"task_change","task":id,"labels":["корпус","срочно"]})).unwrap();
    command(&bob, serde_json::json!({"action":"task_comment","task":id,"text":"Метку поставил."})).unwrap();
    both_ways();
    let lines: Vec<String> = alice.list_group_messages(&gid).unwrap().into_iter().map(|m| m.body).filter(|b| b.starts_with("Bob · задачи:")).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    // The same lines are «Что нового» on Alice's projects' page, newest first, each about its project;
    // Bob's own page has none of his.
    let news = alice.workspace_view(&gid).unwrap().news;
    let about_tasks: Vec<&str> = news.iter().filter(|n| n.text.starts_with("Bob · задачи:")).map(|n| n.about.as_str()).collect();
    assert_eq!(about_tasks, vec![crate::app_workspace::tasks::TASKS_REPO; 2], "{news:?}");
    assert!(news.windows(2).all(|w| w[0].at_unix_ms >= w[1].at_unix_ms), "newest first");
    assert!(bob.workspace_view(&gid).unwrap().news.iter().all(|n| !n.text.starts_with("Bob ·")));

    // A version of another project that says «Закрывает #1» closes it.
    let model = std::env::temp_dir().join(format!("omni-workspace-tasks-model-{}", unique_suffix()));
    fs::create_dir_all(&model).unwrap();
    fs::write(model.join("part.prt"), "v1").unwrap();
    alice.workspace_add_files_repo(&gid, "model", &model.to_string_lossy()).unwrap();
    let version = alice.workspace_save_version(&gid, "model", "Отверстие сделано, закрывает #1").unwrap().unwrap();
    let task = command(&alice, serde_json::json!({"action":"tasks"})).unwrap().tasks.remove(0);
    assert_eq!(task.status, "done");
    assert_eq!(task.mentions.iter().map(|m| (m.repo.as_str(), m.oid.as_str())).collect::<Vec<_>>(), vec![("model", version.as_str())]);
    let _ = fs::remove_dir_all(&model);
}

/// docs/bugs/workspace-keeper-save-rewrote-history.md: a member turned custodian saves from a folder
/// that still holds the version the admin turned down. The save goes onto the shared line without it:
/// the rejected version does not come back, the accepted one keeps its number and id, and the
/// revision on check stays with its version.
#[test]
fn workspace_a_custodians_save_from_a_stale_folder_leaves_the_accepted_history_as_it_was() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-stale-keeper", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-stale-keeper", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-stale-keeper-{}", unique_suffix()));
    let (af, bf) = (root.join("alice"), root.join("bob"));
    fs::create_dir_all(&af).unwrap();
    fs::write(af.join("part.prt"), "v1").unwrap();
    fs::write(af.join("notes.txt"), "n1").unwrap();
    let ar = alice.workspace_add_files_repo(&gid, "files", &af.to_string_lossy()).unwrap();
    let v1 = alice.workspace_save_version(&gid, "files", "v1").unwrap().unwrap();
    let command = |app: &OmniApp, mut json: serde_json::Value| {
        json["group_id"] = gid.clone().into();
        json["repo"] = "files".into();
        workspace_files_command(app, json)
    };
    command(&alice, serde_json::json!({"action":"policy","participants_need_approval":true,"newcomers_download_current":false,"revisions_checked":true})).unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    bob.workspace_take_repo(&gid, "files", &bf.to_string_lossy()).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "v1");

    // Bob's proposal is turned down; the admin saves and releases a revision on check.
    fs::write(bf.join("part.prt"), "holes").unwrap();
    let rejected = bob.workspace_save_version(&gid, "files", "holes").unwrap().unwrap();
    workspace_fetch_metadata(&bob, &alice, &gid, "files");
    command(&alice, serde_json::json!({"action":"review","oid":rejected,"accepted":false})).unwrap();
    fs::write(af.join("notes.txt"), "admin v2").unwrap();
    let v2 = alice.workspace_save_version(&gid, "files", "v2").unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    command(&alice, serde_json::json!({"action":"release","oid":v2,"label":"A"})).unwrap();

    // Bob becomes the custodian; his folder still holds the rejected version.
    let bob_account = command(&bob, serde_json::json!({"action":"details"})).unwrap().members.into_iter().find(|m| m.is_me).unwrap().account_id;
    command(&alice, serde_json::json!({"action":"nominate","account_id":bob_account})).unwrap();
    workspace_exchange(&alice, &bob, &gid, "files");
    let nomination = command(&bob, serde_json::json!({"action":"details"})).unwrap().members.into_iter().find(|m| m.is_me).unwrap().nomination.unwrap();
    command(&bob, serde_json::json!({"action":"consent","nomination":nomination,"accepted":true,"folder":root.join("custody")})).unwrap();
    deliver_group_feed(&bob, &[&alice]);
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "holes", "the folder is stale");

    fs::write(bf.join("drawing.pdf"), "%PDF").unwrap();
    bob.workspace_save_version(&gid, "files", "drawing").unwrap().unwrap();
    for _ in 0..2 {
        workspace_exchange(&bob, &alice, &gid, "files");
        workspace_exchange(&alice, &bob, &gid, "files");
    }

    let tree = |id: &str| crate::app_workspace::test_tree(Path::new(&ar.published_path), "files", id);
    for app in [&alice, &bob] {
        let history = command(app, serde_json::json!({"action":"history"})).unwrap().history;
        let numbers: Vec<(u32, String)> = history.iter().map(|e| (e.number, e.oid.clone())).collect();
        assert!(!history.iter().any(|e| e.oid == rejected), "the rejected version stays out: {numbers:?}");
        let accepted = history.iter().find(|e| e.oid == v2).unwrap_or_else(|| panic!("the accepted version keeps its id: {numbers:?}"));
        assert_eq!(accepted.number, 2, "and its number");
        assert_eq!((accepted.revision.as_deref(), accepted.revision_pending), (Some("A"), true), "the revision stays with its version");
        assert_eq!(history.iter().find(|e| e.oid == v1).map(|e| e.number), Some(1));
        assert_eq!(history.len(), 3, "{numbers:?}");
        let current = tree(&history[0].oid);
        assert_eq!(current["part.prt"], tree(&v1)["part.prt"], "the rejected content is not in the current version");
        assert_eq!(current["notes.txt"], tree(&v2)["notes.txt"]);
        assert!(current.contains_key("drawing.pdf"), "the custodian's change is in");
    }
    assert_eq!(fs::read_to_string(bf.join("part.prt")).unwrap(), "v1", "the custodian's folder follows the shared version");
    assert_eq!(fs::read_to_string(bf.join("notes.txt")).unwrap(), "admin v2");
    let on_check = bob.workspace_view(&gid).unwrap().repos.into_iter().find(|r| r.repo == "files").unwrap().revisions_on_check;
    assert_eq!(on_check, vec!["A".to_string()]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_files_folder_inside_a_git_repository_never_touches_it() {
    let app = create_app_with_active_profile(
        isolated_test_db_path("omni-workspace-nested", "alice"),
        "Alice",
        "A",
    );
    let gid = app.create_group("Project").unwrap().group_id;
    let parent = std::env::temp_dir().join(format!("omni-parent-repo-{}", unique_suffix()));
    let child = parent.join("project");
    fs::create_dir_all(&child).unwrap();
    workspace_git(&parent, &["init", "--quiet", "--initial-branch=main"]);
    fs::write(parent.join("keep.txt"), "parent content").unwrap();
    workspace_git(&parent, &["add", "keep.txt"]);
    workspace_git(
        &parent,
        &[
            "-c",
            "user.name=Parent",
            "-c",
            "user.email=parent@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "parent",
        ],
    );
    let config = fs::read(parent.join(".git/config")).unwrap();
    let index = fs::read(parent.join(".git/index")).unwrap();
    let head = workspace_git(&parent, &["rev-parse", "HEAD"]);
    fs::write(child.join("part.prt"), "project content").unwrap();
    // A folder's own git repository is not the project's content.
    fs::create_dir_all(child.join("sub/.git")).unwrap();
    fs::write(child.join("sub/.git/HEAD"), "ref: refs/heads/main").unwrap();
    let record = app
        .workspace_add_files_repo(&gid, "files", &child.to_string_lossy())
        .unwrap();
    assert!(!child.join(".git").exists(), "a files project makes no git repository");
    let v1 = app
        .workspace_save_version(&gid, "files", "v1")
        .unwrap()
        .unwrap();
    let tree = crate::app_workspace::test_tree(Path::new(&record.published_path), "files", &v1);
    assert_eq!(tree.keys().collect::<Vec<_>>(), vec!["part.prt"]);
    assert_eq!(fs::read(parent.join(".git/config")).unwrap(), config);
    assert_eq!(fs::read(parent.join(".git/index")).unwrap(), index);
    assert_eq!(workspace_git(&parent, &["rev-parse", "HEAD"]), head);
    let nested_code = parent.join("not-a-repo");
    fs::create_dir(&nested_code).unwrap();
    assert!(app
        .workspace_add_repo(&gid, "code", Some(&nested_code.to_string_lossy()))
        .is_err());
    assert_eq!(fs::read(parent.join(".git/config")).unwrap(), config);
}

#[test]
fn workspace_a_commit_on_the_shared_main_that_closes_a_task_closes_it_once() {
    const TASKS: &str = crate::app_workspace::tasks::TASKS_REPO;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-commit-task", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-commit-task", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let tasks = |app: &OmniApp, action: serde_json::Value| {
        let mut json = action;
        json["group_id"] = gid.clone().into();
        workspace_files_command(app, json).unwrap().tasks.unwrap()
    };
    tasks(&alice, serde_json::json!({"action":"tasks_enable"}));
    let id = tasks(&alice, serde_json::json!({"action":"task_new","title":"Утечка в api"})).tasks[0].id.clone();
    deliver_group_feed(&alice, &[&bob]);
    for _ in 0..2 {
        workspace_exchange(&alice, &bob, &gid, TASKS);
        workspace_exchange(&bob, &alice, &gid, TASKS);
    }

    let work = std::env::temp_dir().join("omni-workspace-commit-task").join(format!("work-{}", unique_suffix()));
    fs::create_dir_all(&work).unwrap();
    workspace_git(&work, &["init", "--quiet", "--initial-branch=main"]);
    let alice_published = alice.workspace_add_repo(&gid, "api", Some(&work.to_string_lossy())).unwrap().published_path;
    let bob_published = PathBuf::from(bob.workspace_add_repo(&gid, "api", None).unwrap().published_path);
    fs::write(work.join("a.txt"), "a").unwrap();
    workspace_git(&work, &["add", "a.txt"]);
    workspace_git(&work, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "Утечка закрыта, закрывает #1"]);
    workspace_git(&work, &["push", "--quiet", "omni", "main"]);

    // Alice's own main moves the shared one: her node writes the mention.
    assert!(alice.workspace_main(&gid, "api").unwrap().oid.is_some());
    let task = tasks(&alice, serde_json::json!({"action":"tasks"})).tasks.into_iter().find(|t| t.id == id).unwrap();
    assert_eq!((task.status.as_str(), task.mentions.len()), ("done", 1));
    assert_eq!(task.mentions[0].repo, "api");

    // Bob's node moves its shared main too, and writes nothing: one mention, not two.
    alice.workspace_publish(&gid, "api").unwrap();
    deliver_group_feed(&alice, &[&bob]);
    workspace_git(&bob_published, &["fetch", "--quiet", &alice_published, "+refs/heads/*:refs/omni/members/alice/heads/*"]);
    for b in bob.workspace_member_branches(&gid, "api").unwrap() {
        bob.storage.mark_workspace_member_ref_fetched(&gid, "api", &b.account_id, &b.ref_name, &b.oid).unwrap();
    }
    assert!(bob.workspace_main(&gid, "api").unwrap().oid.is_some());
    for _ in 0..2 {
        workspace_exchange(&alice, &bob, &gid, TASKS);
        workspace_exchange(&bob, &alice, &gid, TASKS);
    }
    let seen = tasks(&bob, serde_json::json!({"action":"tasks"})).tasks.into_iter().find(|t| t.id == id).unwrap();
    assert_eq!((seen.status.as_str(), seen.mentions.len()), ("done", 1));

    // A working folder half deleted (no git repository any more) does not take the screen down.
    fs::remove_file(work.join(".git").join("HEAD")).unwrap();
    let view = alice.workspace_view(&gid).unwrap();
    assert!(view.repos.iter().any(|r| r.repo == "api" && r.local_branches.is_empty()));
    let _ = fs::remove_dir_all(&work);
}

/// Leaving a group takes its projects from this computer (docs/decisions/workspace-files-storage.md,
/// «Выход из группы»): Omni's copy, the task folder Omni made, Omni's content in the custodian's
/// archive folder. The working folder and the person's own files stay. Every way out is the same —
/// left, removed, dissolved (the dissolver too) — and a group waiting to be restored keeps its data.
#[test]
fn leaving_a_group_takes_its_projects_from_this_computer_and_leaves_the_persons_files() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-left-projects", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-left-projects", "bob"), "Bob", "B");
    let workspaces = |app: &OmniApp| crate::app_workspace::own_folder(&app.storage, omni_storage::ProfileFolder::Workspaces).unwrap();
    let tasks = |app: &OmniApp| crate::app_workspace::own_folder(&app.storage, omni_storage::ProfileFolder::WorkspaceTasks).unwrap();
    let sweep = |app: &OmniApp| crate::app_workspace::remove_set_aside(&crate::app_workspace::set_aside_projects_of_groups_left(&app.storage));
    let root = std::env::temp_dir().join(format!("omni-left-projects-{}", unique_suffix()));

    // A files project Bob keeps the archive of.
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let source = root.join("alice");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("model.prt"), "v1").unwrap();
    alice.workspace_add_files_repo(&gid, "files", &source.to_string_lossy()).unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    let bob_account = bob.storage.get_local_account().unwrap().unwrap().descriptor.account_id;
    workspace_files_command(&alice, serde_json::json!({"action":"nominate","group_id":gid,"repo":"files","account_id":bob_account})).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    let nomination = workspace_files_command(&bob, serde_json::json!({"action":"details","group_id":gid,"repo":"files"}))
        .unwrap()
        .members
        .into_iter()
        .find(|m| m.is_me)
        .unwrap()
        .nomination
        .unwrap();
    let custody = root.join("custody");
    workspace_files_command(&bob, serde_json::json!({"action":"consent","group_id":gid,"repo":"files","nomination":nomination,"accepted":true,"folder":custody})).unwrap();
    deliver_group_feed(&bob, &[&alice]);

    // The leave dialog: what goes, and what goes for the group too.
    let notice = bob.workspace_leave_notice(&gid).unwrap();
    assert_eq!(notice.projects, vec!["files".to_string()]);
    assert_eq!(notice.only_custodian_of, vec!["files".to_string()], "nobody else keeps the whole history");
    assert!(alice.workspace_leave_notice(&gid).unwrap().only_custodian_of.is_empty(), "Alice keeps no archive");

    let published = PathBuf::from(bob.storage.get_workspace_repo(&gid, "files").unwrap().unwrap().published_path);
    assert!(published.starts_with(workspaces(&bob).join(&gid)));
    let bob_profile = bob.storage.get_active_profile_id().unwrap().unwrap().0;
    let archived = custody.join(".omni").join(&bob_profile).join(&gid).join("files").join("content").join("objects").join("ab").join("cd").join("abcd");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::write(&archived, "ARCHIVED").unwrap();
    fs::write(custody.join("notes.txt"), "the person's file").unwrap();
    let work = custody.join("files");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("model.prt"), "being edited").unwrap();
    let task_folder = tasks(&bob).join(&gid);
    fs::create_dir_all(&task_folder).unwrap();
    fs::write(task_folder.join("task.md"), "a task").unwrap();

    // Another group Bob stays in keeps its data while he is in it.
    let other = alice.create_group("Other").unwrap().group_id;
    group_join_for_real(&alice, &bob, &other, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let other_data = workspaces(&bob).join(&other).join("drawings.files");
    fs::create_dir_all(&other_data).unwrap();
    fs::write(other_data.join("kept"), "x").unwrap();

    // Still in both: nothing goes.
    sweep(&bob);
    assert!(published.exists() && other_data.exists() && task_folder.exists());

    // Left.
    bob.leave_group(&gid).unwrap();
    sweep(&bob);
    assert!(!workspaces(&bob).join(&gid).exists(), "Omni's copy goes");
    assert!(!task_folder.exists(), "the task folder Omni made goes");
    assert!(!custody.join(".omni").exists(), "the archive's content goes");
    assert_eq!(fs::read_to_string(custody.join("notes.txt")).unwrap(), "the person's file", "the chosen folder stays");
    assert_eq!(fs::read_to_string(work.join("model.prt")).unwrap(), "being edited", "the working folder stays");
    assert!(other_data.exists(), "a group Bob is in keeps its data");
    let left_over: Vec<_> = fs::read_dir(workspaces(&bob)).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with(".gone-")).collect();
    assert!(left_over.is_empty(), "nothing set aside stays: {left_over:?}");

    // Removed: the chat stays and says the data went; the projects' rows go with it.
    alice.remove_group_member(&other, &member_pub_of(&alice, &other, "Bob")).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    assert_eq!(bob.storage.get_group(&other).unwrap().unwrap().my_status, "removed");
    bob.storage
        .upsert_workspace_repo(&omni_storage::WorkspaceRepoRecord {
            group_id: other.clone(),
            repo: "drawings".into(),
            published_path: other_data.to_string_lossy().into_owned(),
            work_path: None,
            announced_refs_json: None,
            created_at_unix_ms: 1,
        })
        .unwrap();
    sweep(&bob);
    assert!(!workspaces(&bob).join(&other).exists());
    assert!(bob.storage.list_workspace_repos(&other).unwrap().is_empty());
    let said = |app: &OmniApp, group: &str| {
        app.list_group_messages(group).unwrap().iter().filter(|m| m.kind == "system" && m.body == "Данные проектов группы удалены с этого компьютера").count()
    };
    assert_eq!(said(&bob, &other), 1);
    sweep(&bob);
    assert_eq!(said(&bob, &other), 1, "said once");

    // Dissolved: every member's copy goes, the dissolver's too.
    let third = alice.create_group("Third").unwrap().group_id;
    group_join_for_real(&alice, &bob, &third, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    for app in [&alice, &bob] {
        fs::create_dir_all(workspaces(app).join(&third).join("specs.files")).unwrap();
    }
    alice.delete_group_for_everyone(&third).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    assert_eq!(bob.storage.get_group(&third).unwrap().unwrap().my_status, "dissolved");
    sweep(&alice);
    sweep(&bob);
    assert!(!workspaces(&alice).join(&third).exists(), "the dissolver's copy goes");
    assert!(!workspaces(&bob).join(&third).exists(), "a member's copy goes");
    assert_eq!(said(&bob, &third), 1);

    // A group whose rows were lost and that waits to be restored from its MLS state is not left.
    let restoring = alice.create_group("Restoring").unwrap().group_id;
    let restoring_data = workspaces(&alice).join(&restoring).join("plans.files");
    fs::create_dir_all(&restoring_data).unwrap();
    alice.storage.delete_group_rows(&restoring).unwrap();
    assert!(alice.storage.list_group_ids_without_row().unwrap().contains(&restoring));
    sweep(&alice);
    assert!(restoring_data.exists(), "a group being restored keeps its data");

    // Each local profile of the database has a folder of its own: the sweep of the active one never
    // touches another's, whatever groups the active one is in (blind audit 2026-11-12, pass 17).
    let shared = "group-of-another-profile";
    let shared_data = omni_storage::ProfileFolder::Workspaces
        .path(alice.storage.db_path())
        .unwrap()
        .join("another-profile")
        .join(shared)
        .join("cad.files");
    fs::create_dir_all(&shared_data).unwrap();
    sweep(&alice);
    assert!(shared_data.exists(), "another profile's group keeps its data");
}

/// A44 (audit 2026-11-09): a newcomer learns the admins' decisions that stand, whoever made them.
/// Each member told newcomers only its own events, and only while an admin: a policy set by an admin
/// demoted since stayed with the old members and never reached a newcomer.
#[test]
fn a_newcomer_learns_the_policy_of_an_admin_demoted_since() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-demoted", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-demoted", "bob"), "Bob", "B");
    let dave = create_app_with_active_profile(isolated_test_db_path("omni-workspace-demoted", "dave"), "Dave", "D");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let bob_pub = member_pub_of(&alice, &gid, "Bob");
    alice.set_group_admin(&gid, &bob_pub, true).unwrap();
    deliver_group_feed(&alice, &[&bob]);

    let policy_of = |app: &OmniApp| {
        app.storage.list_workspace_file_events(&gid).unwrap().into_iter().find_map(|r| {
            match serde_json::from_str::<omni_protocol::WorkspaceFilesEventV1>(&r.event_json) {
                Ok(omni_protocol::WorkspaceFilesEventV1::Policy { repo, policy }) if repo == "files" => Some(policy.newcomers_download_current),
                _ => None,
            }
        })
    };
    workspace_files_command(
        &bob,
        serde_json::json!({"action":"policy","group_id":gid,"repo":"files","participants_need_approval":true,"newcomers_download_current":true}),
    )
    .unwrap();
    deliver_group_feed(&bob, &[&alice]);
    assert_eq!(policy_of(&alice), Some(true));

    alice.set_group_admin(&gid, &bob_pub, false).unwrap();
    deliver_group_feed(&alice, &[&bob]);
    group_join_for_real(&alice, &dave, &gid, "dialog:dave");
    deliver_group_feed(&alice, &[&bob, &dave]);

    // The workspace tick of each: newcomers are told what stands. A told event a member does not
    // take (not its role to say) is dropped, as on the mesh.
    let apps = [&alice, &bob, &dave];
    for _ in 0..2 {
        for app in apps {
            let group = app.storage.get_group(&gid).unwrap().unwrap();
            let members = app.storage.list_group_members(&gid).unwrap();
            let repos = app.storage.list_workspace_repos(&gid).unwrap();
            crate::app_workspace::tell_newcomers(&app.storage, &group, &members, &repos).unwrap();
        }
        for from in apps {
            for (row, group_id, mls) in from.storage.list_group_feed_outbox(1_000).unwrap() {
                let item = omni_protocol::GroupFeedItemV2 {
                    version: omni_protocol::GROUP_WIRE_VERSION_V2,
                    group_id,
                    mls_b64: base64::engine::general_purpose::STANDARD.encode(mls),
                };
                let json = serde_json::to_string(&item).unwrap();
                for to in apps.iter().filter(|to| !std::ptr::eq(**to, from)) {
                    let _ = super::app_groups::ingest_group_feed_item(&to.storage, &to.transport, &json);
                }
                from.storage.delete_group_feed_outbox(row).unwrap();
            }
        }
    }
    assert_eq!(policy_of(&dave), Some(true), "the newcomer has the policy the old members keep");
    assert_eq!(policy_of(&alice), Some(true));
}

/// Audit 2026-11-09: what the store holds is checked against its id when read. A damaged copy here
/// is not handed to the person as the file they download, and a member whose copy is damaged does not
/// stop the download: the next member is asked.
#[test]
fn a_downloaded_file_is_never_a_damaged_copy_and_one_damaged_member_does_not_stop_it() {
    use sha2::Digest as _;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-damaged", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-damaged", "bob"), "Bob", "B");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    deliver_group_feed(&alice, &[&bob]);
    let root = std::env::temp_dir().join(format!("omni-damaged-copy-{}", unique_suffix()));
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), "selected").unwrap();
    alice.workspace_add_files_repo(&gid, "files", &source.to_string_lossy()).unwrap();
    alice.workspace_save_version(&gid, "files", "v1").unwrap();
    workspace_fetch_metadata(&alice, &bob, &gid, "files");
    bob.workspace_main(&gid, "files").unwrap();
    let br = bob.storage.get_workspace_repo(&gid, "files").unwrap().unwrap();
    let oid = hex::encode(sha2::Sha256::digest(b"selected"));

    // Bob's store holds the file, damaged on disk; a member's copy is damaged too.
    let here = crate::app_workspace::content::object_path(Path::new(&br.published_path), &oid);
    fs::create_dir_all(here.parent().unwrap()).unwrap();
    fs::write(&here, "SELECTED").unwrap();
    let damaged_member = root.join("damaged-member");
    let theirs = crate::app_workspace::content::object_path(&damaged_member, &oid);
    fs::create_dir_all(theirs.parent().unwrap()).unwrap();
    fs::write(&theirs, "selectex").unwrap();
    let (damaged_node, alices_node) =
        (omni_mesh_runtime::MeshNodeAddress::from_bytes([1; 20]), omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20]));
    let from_damaged = workspace_content_stream(damaged_member, None);
    let from_alice = workspace_stream_from(&alice, &gid, "files");
    let open = |node: omni_mesh_runtime::MeshNodeAddress| if node == damaged_node { from_damaged(node) } else { from_alice(node) };

    let target = root.join("downloaded.txt");
    workspace_files_command(&bob, serde_json::json!({"action":"download","group_id":gid,"repo":"files","path":"a.txt","target":target})).unwrap();
    crate::app_workspace::process_download(&bob.storage, &br, &[damaged_node, alices_node], &open).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "selected", "the file itself, from the member whose copy is whole");
    assert!(!here.exists(), "the damaged copy here goes");
}
