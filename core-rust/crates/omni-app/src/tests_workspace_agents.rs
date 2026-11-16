// Included in tests.rs: agents of a workspace (docs/decisions/omni-workspace-git.md section 5).

#[test]
fn an_old_card_does_not_bring_back_a_right_and_an_agent_cannot_drop_its_owner() {
    // Code audit 2026-11-09: OMNI-04 — a card late or sent again set the agent fields before its
    // version was looked at; OMNI-08 — an agent stopped being one by dropping `agent_of` from its card.
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-agent-cards", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-agent-cards", "bob"), "Bob", "B");
    let robo = create_app_with_active_profile(isolated_test_db_path("omni-agent-cards", "robo"), "Robo", "C");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    group_join_for_real(&alice, &robo, &gid, "dialog:robo");
    let all = [&alice, &bob, &robo];
    let deliver = || {
        for _ in 0..2 {
            for from in all {
                let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
                deliver_group_feed(from, &to);
            }
        }
    };
    deliver();
    let account = |app: &OmniApp| app.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
    let (alice_account, robo_account) = (account(&alice), account(&robo));
    robo.agent_init(&alice_account, "Robo").unwrap();
    alice.workspace_agent_connect(&robo.agent_code().unwrap(), "Robo", true, None).unwrap();
    deliver();
    let members_command_at_bob = || bob.workspace_agents(&gid).unwrap().agents.iter().find(|a| a.account_id == robo_account).map(|a| a.members_command);
    assert_eq!(members_command_at_bob(), Some(true));
    let old_card = serde_json::to_string(&crate::app_group_member_profiles::own_member_profile(&alice.storage).unwrap()).unwrap();

    // Alice takes the members' right away; her old card comes again: the right stays taken.
    std::thread::sleep(std::time::Duration::from_millis(5));
    alice.workspace_agent_connect(&robo.agent_code().unwrap(), "Robo", false, None).unwrap();
    deliver();
    assert_eq!(members_command_at_bob(), Some(false));
    let (_, alice_card) = super::app_groups::local_mls_identity(&alice.storage).unwrap();
    crate::app_group_member_profiles::receive_profile(&bob.storage, &alice_card, &old_card).unwrap();
    assert_eq!(members_command_at_bob(), Some(false), "an old card does not bring the right back");

    // Robo's card stops saying whose it is: to Bob it is still Alice's agent.
    let (_, robo_card) = super::app_groups::local_mls_identity(&robo.storage).unwrap();
    let mut dropped = crate::app_group_member_profiles::own_member_profile(&robo.storage).unwrap();
    dropped.agent_of = None;
    dropped.agents_version = i64::MAX - 1;
    crate::app_group_member_profiles::receive_profile(&bob.storage, &robo_card, &serde_json::to_string(&dropped).unwrap()).unwrap();
    assert!(crate::app_workspace::agents::is_agent(&bob.storage, &robo_account), "an agent cannot step out by its own card");

    // Only its owner lets it go.
    alice.workspace_agent_forget(&robo_account).unwrap();
    deliver();
    assert!(!crate::app_workspace::agents::is_agent(&bob.storage, &robo_account), "the owner took it off its list");
}

/// Blind audit 2026-11-11, pass 14: past the bound on discussions the oldest of all went, by a time
/// its opener names — a member opening them by the hundred, dated ahead, pushed out everyone else's
/// with their lines. And a line that aged out of a discussion left its reactions behind.
#[test]
fn a_member_opening_discussions_by_the_hundred_pushes_out_only_its_own() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-talk-bound", "alice"), "Alice", "A");
    let gid = alice.create_group("Project").unwrap().group_id;
    let card = |account: &str, name: &str| omni_group_mls::MemberCard {
        member_pub: format!("pub-{account}"),
        account_id: account.into(),
        display_name: name.into(),
        device_chain: String::new(),
    };
    let (honest, flooder) = (card("acc-honest", "Honest"), card("acc-flooder", "Flooder"));
    let now = crate::current_unix_ms();
    use crate::app_workspace::talk;
    talk::apply_open(&alice.storage, &gid, &honest, "design-review", "Design", None, now, false).unwrap();
    talk::apply_talk(&alice.storage, &gid, &honest, "line-0", "design-review", "the plan", None, &[], now).unwrap();
    for i in 0..250 {
        talk::apply_open(&alice.storage, &gid, &flooder, &format!("flood-{i:03}"), "x", None, 9_000_000_000_000_000, false).unwrap();
    }
    let threads = alice.workspace_talk_threads(&gid).unwrap();
    assert!(threads.iter().any(|t| t.id == "design-review"), "the others' discussion stays");
    assert_eq!(alice.workspace_talk_read(&gid, "design-review").unwrap().lines.len(), 1, "with its lines");
    assert!(threads.len() <= 200, "{}", threads.len());
    assert!(threads.iter().all(|t| t.at_unix_ms <= crate::current_unix_ms() + 5 * 60 * 1000), "none dated ahead of our clock");

    for i in 1..=520 {
        let id = format!("line-{i}");
        talk::apply_talk(&alice.storage, &gid, &flooder, &id, "design-review", "more", None, &[], now + i).unwrap();
        talk::apply_reaction(&alice.storage, &gid, &flooder, "design-review", &id, "👍", now + i).unwrap();
    }
    let kept = alice.workspace_talk_read(&gid, "design-review").unwrap().lines.len();
    assert!(talk::reacted_lines_for_tests(&alice.storage, &gid, "design-review") <= kept, "a line that went took its reactions");
}

#[test]
fn a_line_dated_ahead_does_not_hide_later_lines_from_an_agent() {
    // Code audit 2026-11-09, OMNI-03/07: the agent's place was the newest author time it had read —
    // one line dated years ahead put every later line behind it, and of two lines of one millisecond
    // the later was lost.
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-agent-place", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-agent-place", "bob"), "Bob", "B");
    let robo = create_app_with_active_profile(isolated_test_db_path("omni-agent-place", "robo"), "Robo", "C");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    group_join_for_real(&alice, &robo, &gid, "dialog:robo");
    let all = [&alice, &bob, &robo];
    let deliver = || {
        for _ in 0..2 {
            for from in all {
                let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
                deliver_group_feed(from, &to);
            }
        }
    };
    deliver();
    let alice_account = alice.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
    robo.agent_init(&alice_account, "Robo").unwrap();
    let mut place = crate::AgentInboxPlace::default();
    let mut look = || {
        let inbox = robo.agent_inbox(&gid, Some(place)).unwrap();
        place = inbox.place;
        inbox.items.into_iter().map(|i| i.text).collect::<Vec<_>>()
    };
    look();

    let ahead = crate::current_unix_ms() + 10 * 365 * 24 * 3600 * 1000;
    let line = |text: &str, at: i64| omni_protocol::GroupAppPayloadV1 {
        message_id: crate::app_groups::random_group_message_id(),
        kind: omni_protocol::GroupAppKindV1::Text,
        body: text.into(),
        authored_at_unix_ms: at,
        signature_b64: None,
        reply_to: None,
        mentions: Vec::new(),
    };
    crate::app_groups::send_app_payload(&bob.storage, &gid, &line("из будущего", ahead)).unwrap();
    deliver();
    assert_eq!(look(), vec!["из будущего".to_string()]);
    let kept = robo.list_group_messages(&gid).unwrap().into_iter().find(|m| m.body == "из будущего").unwrap();
    assert!(kept.created_at_unix_ms <= crate::current_unix_ms() + omni_storage::PIN_CLOCK_AHEAD_MS, "a line dated ahead is kept at most an hour ahead");

    alice.send_group_text(&gid, "@Robo дальше").unwrap();
    deliver();
    assert_eq!(look(), vec!["@Robo дальше".to_string()], "a later line still reaches the agent");

    let same_ms = crate::current_unix_ms();
    crate::app_groups::send_app_payload(&bob.storage, &gid, &line("первая", same_ms)).unwrap();
    deliver();
    assert_eq!(look(), vec!["первая".to_string()]);
    crate::app_groups::send_app_payload(&alice.storage, &gid, &line("вторая той же мс", same_ms)).unwrap();
    deliver();
    assert_eq!(look(), vec!["вторая той же мс".to_string()], "a line of the same millisecond is not lost");
    assert!(look().is_empty(), "nothing twice");
}

#[test]
fn workspace_agent_is_its_owners_never_an_admin_stops_on_the_admins_word_and_waits_for_a_person() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-agents", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-agents", "bob"), "Bob", "B");
    let robo = create_app_with_active_profile(isolated_test_db_path("omni-workspace-agents", "robo"), "Robo", "C");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    group_join_for_real(&alice, &robo, &gid, "dialog:robo");
    let all = [&alice, &bob, &robo];
    let deliver = || {
        for _ in 0..2 {
            for from in all {
                let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
                deliver_group_feed(from, &to);
            }
        }
    };
    deliver();
    let account = |app: &OmniApp| app.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
    let (alice_account, robo_account) = (account(&alice), account(&robo));

    // Robo says it is Alice's agent; Alice names it in her card: both, so it is shown as hers.
    robo.agent_init(&alice_account, "Robo").unwrap();
    alice.workspace_agent_connect(&robo.agent_code().unwrap(), "Robo", false, None).unwrap();
    deliver();
    robo.agent_send(&gid, "Готов к работе.").unwrap();
    deliver();
    let line = bob.list_group_messages(&gid).unwrap().into_iter().find(|m| m.body == "Готов к работе.").unwrap();
    assert_eq!(line.sender_name, "Robo · агент Alice");
    let seen = bob.workspace_agents(&gid).unwrap();
    assert_eq!(seen.agents.iter().map(|a| (a.name.as_str(), a.owner_name.as_str(), a.mine)).collect::<Vec<_>>(), vec![("Robo", "Alice", false)]);
    assert!(alice.workspace_agents(&gid).unwrap().agents[0].mine);

    // Never an admin or a custodian.
    let robo_pub = alice.storage.list_group_members(&gid).unwrap().into_iter().find(|m| m.account_id == robo_account).unwrap().member_pub;
    assert!(alice.set_group_admin(&gid, &robo_pub, true).is_err());
    assert!(workspace_files_command(&alice, serde_json::json!({"action":"nominate","group_id":gid,"repo":"model","account_id":robo_account})).is_err());

    // Only the admin stops the agents; a stopped agent writes nothing until it is let go.
    assert!(bob.workspace_agents_stop(&gid, true).is_err());
    alice.workspace_agents_stop(&gid, true).unwrap();
    deliver();
    assert!(robo.workspace_agents(&gid).unwrap().stopped);
    assert!(robo.agent_send(&gid, "Ещё одно.").is_err());
    // A11 (audit 2026-11-09 #6): nor does it take new work — it hears it is stopped.
    assert!(robo.agent_inbox(&gid, None).is_err(), "a stopped agent reads no tasks");
    assert!(robo.agent_talk_inbox(&gid).is_err(), "nor discussions");
    // Blind audit 2026-11-11, M5: the stop is the receivers' to hold, not the agent's own client's —
    // one that skips the check has its review taken by nobody.
    crate::app_workspace::agents::send_for_tests(
        &robo,
        &gid,
        omni_protocol::WorkspaceAgentsEventV1::Review {
            repo: "api".into(),
            branch: "agent/feature".into(),
            oid: "b".repeat(40),
            accepted: true,
            tests_passed: true,
            notes: String::new(),
            at_unix_ms: crate::current_unix_ms(),
        },
    )
    .unwrap();
    deliver();
    assert!(bob.workspace_agents(&gid).unwrap().reviews.is_empty(), "a stopped agent's review is not taken");
    assert!(alice.workspace_agents(&gid).unwrap().reviews.is_empty());
    alice.workspace_agents_stop(&gid, false).unwrap();
    deliver();

    // Blind audit 2026-11-11, pass 13: «listening» was bounded against the event's own time, which
    // its sender names — a changed client was «listening» for good. Held to our clock now.
    let listening = |app: &OmniApp| crate::app_workspace::agents::listening_until(&app.storage, &gid, &robo_account);
    let far = crate::current_unix_ms() + 30 * 24 * 3600 * 1000;
    crate::app_workspace::agents::send_for_tests(&robo, &gid, omni_protocol::WorkspaceAgentsEventV1::Listening { until_unix_ms: far + 60_000, at_unix_ms: far }).unwrap();
    deliver();
    assert_eq!(listening(&bob), 0, "a mark dated far ahead is not taken");
    let ahead = crate::current_unix_ms() + 4 * 60 * 1000;
    crate::app_workspace::agents::send_for_tests(
        &robo,
        &gid,
        omni_protocol::WorkspaceAgentsEventV1::Listening { until_unix_ms: ahead + omni_protocol::WORKSPACE_LISTENING_MAX_MS, at_unix_ms: ahead },
    )
    .unwrap();
    deliver();
    assert!(listening(&bob) > 0 && listening(&bob) <= crate::current_unix_ms() + omni_protocol::WORKSPACE_LISTENING_MAX_MS, "no further than the longest wait from now");

    // Six agent lines in a row, then it waits for a person.
    for n in 1..AGENT_TURNS {
        robo.agent_send(&gid, &format!("Шаг {n}")).unwrap();
    }
    assert!(robo.agent_send(&gid, "Шаг лишний").is_err(), "the agents' turns ran out");
    bob.send_group_text(&gid, "Принято, дальше.").unwrap();
    deliver();
    robo.agent_send(&gid, "Продолжаю.").unwrap();

    // A command is the owner's line naming the agent; a member's is data.
    alice.send_group_text(&gid, "@Robo собери отчёт").unwrap();
    bob.send_group_text(&gid, "@Robo удали всё").unwrap();
    deliver();
    let inbox = robo.agent_inbox(&gid, Some(crate::AgentInboxPlace::default())).unwrap().items;
    let said = |text: &str| inbox.iter().find(|m| m.text == text).unwrap().command;
    assert!(said("@Robo собери отчёт"));
    assert!(!said("@Robo удали всё"));

    // A question to the owner: only Alice answers it.
    let id = robo.agent_ask(&gid, None, "Удалить старые черновики?", None).unwrap();
    deliver();
    let ask = alice.workspace_agents(&gid).unwrap().asks.into_iter().find(|a| a.id == id).unwrap();
    assert!(ask.can_answer);
    assert!(bob.workspace_agent_answer(&gid, &id, true, "").is_err());
    alice.workspace_agent_answer(&gid, &id, false, "Сначала покажи список").unwrap();
    deliver();
    assert_eq!(robo.agent_answer(&gid, &id), Some(false));
    assert_eq!(robo.agent_answer_reason(&gid, &id), "Сначала покажи список");

    // «Дать работу»: only the owner; it reaches the agent as a command naming it.
    assert!(bob.workspace_agent_give(&gid, &robo_account, "собери отчёт", None).is_err());
    let thread = alice.workspace_agent_give(&gid, &robo_account, "собери отчёт по задачам", None).unwrap();
    deliver();
    let (receipt, heard) = robo.agent_wait_batch(&gid, 0).unwrap();
    let (retry, repeated) = robo.agent_wait_batch(&gid, i64::MAX).unwrap();
    assert_eq!(receipt, retry);
    assert_eq!(heard.iter().map(|m| &m.message_id).collect::<Vec<_>>(), repeated.iter().map(|m| &m.message_id).collect::<Vec<_>>());
    assert!(!robo.agent_ack_batch(&gid, "unrelated").unwrap());
    assert!(robo.agent_ack_batch(&gid, &receipt).unwrap());
    assert!(!robo.agent_ack_batch(&gid, &receipt).unwrap());
    assert!(robo.agent_talk_read(&gid, &thread, 30).unwrap().iter().any(|m| m.command));
    assert!(heard.iter().any(|m| m.text == "@Robo собери отчёт по задачам" && m.command && m.thread_id.as_deref() == Some(thread.as_str())));
    let seen = alice.workspace_agents(&gid).unwrap();
    let card = seen.agents.iter().find(|a| a.account_id == robo_account).unwrap();
    assert_eq!(card.last_said, "Продолжаю.");
    assert_eq!(card.waiting_answer, 0);
    // A «main» ask needs an agent branch accepted by someone else.
    assert!(robo.agent_ask(&gid, Some(("api", "feature")), "", None).is_err());
    // «Принято» only of a commit the reviewer has; «не принято» needs nothing here.
    assert!(bob.workspace_review(&gid, "api", "agent/feature", &"a".repeat(40), true, true, "").is_err());
    bob.workspace_review(&gid, "api", "agent/feature", &"a".repeat(40), false, false, "не собирается").unwrap();

    // A branch the agent asks to have checked: a person sees it and gives the verdict, the agent hears it.
    let work = std::env::temp_dir().join("omni-workspace-agents").join(format!("work-{}", unique_suffix()));
    fs::create_dir_all(&work).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&work).args(args).output().expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    fs::write(work.join("a.txt"), "a").unwrap();
    git(&["add", "a.txt"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "a"]);
    git(&["checkout", "--quiet", "-b", "agent/div"]);
    fs::write(work.join("b.txt"), "b").unwrap();
    git(&["add", "b.txt"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "div"]);
    robo.workspace_add_repo(&gid, "calc", Some(&work.to_string_lossy())).unwrap();
    git(&["push", "--quiet", "omni", "main", "agent/div"]);
    let tip = robo.agent_review_ask(&gid, "calc", "agent/div", None).unwrap();
    deliver();
    let open = bob.workspace_agents(&gid).unwrap().review_asks;
    assert_eq!(open.iter().map(|r| (r.repo.as_str(), r.branch.as_str(), r.oid.as_str(), r.author_name.as_str(), r.can_review)).collect::<Vec<_>>(), vec![("calc", "agent/div", tip.as_str(), "Robo", true)]);
    assert!(!robo.workspace_agents(&gid).unwrap().review_asks[0].can_review, "not the author's own verdict");
    bob.workspace_review(&gid, "calc", "agent/div", &tip, false, false, "нет теста на деление на ноль").unwrap();
    deliver();
    assert!(alice.workspace_agents(&gid).unwrap().review_asks.is_empty(), "a verdict closes the ask");
    let heard = robo.agent_inbox(&gid, Some(crate::AgentInboxPlace::default())).unwrap().items;
    let verdict = heard.iter().find(|m| m.text.starts_with("review of calc")).expect("the agent hears the verdict");
    assert!(!verdict.command && verdict.text.ends_with("changes needed: нет теста на деление на ноль"), "{}", verdict.text);
    // The check was asked from the agent's only work: it and its verdict belong to that discussion.
    assert_eq!(verdict.thread_id.as_deref(), Some(thread.as_str()), "the verdict comes in the work's discussion");
    assert!(!robo.agent_inbox(&gid, Some(crate::AgentInboxPlace::default())).unwrap().items.iter().any(|m| m.text.starts_with("review of calc")), "a verdict is told once, whenever the agent reads");
    assert_eq!(alice.workspace_agents(&gid).unwrap().reviews[0].thread.as_deref(), Some(thread.as_str()));
    assert!(alice.workspace_talk_threads(&gid).unwrap()[0].last_text.contains("нужны правки"));
}

const AGENT_TURNS: usize = crate::app_workspace::agents::AGENT_TURNS_WITHOUT_HUMAN;

/// «Обсуждения»: the work's conversation — the owner's command, the agent's answer there, a member's
/// line as data; the agents' turns are counted per discussion; a line is taken once, and one that
/// comes before its discussion's opening is kept.
#[test]
fn workspace_a_discussion_carries_the_work_and_its_people_and_agents_lines() {
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-talk", "alice"), "Alice", "A");
    let bob = create_app_with_active_profile(isolated_test_db_path("omni-workspace-talk", "bob"), "Bob", "B");
    let robo = create_app_with_active_profile(isolated_test_db_path("omni-workspace-talk", "robo"), "Robo", "C");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &bob, &gid, "dialog:bob");
    group_join_for_real(&alice, &robo, &gid, "dialog:robo");
    let all = [&alice, &bob, &robo];
    let deliver = || {
        for _ in 0..2 {
            for from in all {
                let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
                deliver_group_feed(from, &to);
            }
        }
    };
    deliver();
    let account = |app: &OmniApp| app.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
    robo.agent_init(&account(&alice), "Robo").unwrap();
    alice.workspace_agent_connect(&robo.agent_code().unwrap(), "Robo", false, None).unwrap();
    deliver();

    // «Дать работу» opens the work's discussion and puts the command there, not into the chat.
    let thread = alice.workspace_agent_give(&gid, &account(&robo), "проверь размеры крышки по чертежу", None).unwrap();
    deliver();
    assert!(!bob.list_group_messages(&gid).unwrap().iter().any(|m| m.body.contains("проверь размеры")), "the chat gets no lines of it");
    let listed = bob.workspace_talk_threads(&gid).unwrap();
    assert_eq!(listed.iter().map(|t| (t.id.as_str(), t.topic.as_str())).collect::<Vec<_>>(), vec![(thread.as_str(), "проверь размеры крышки по чертежу")]);
    let heard = robo.agent_talk_inbox(&gid).unwrap();
    assert!(heard.iter().any(|m| m.command && m.thread_id.as_deref() == Some(thread.as_str())));

    // The agent answers where the work came: its only work is the default.
    assert_eq!(robo.agent_talk_default(&gid).unwrap().as_deref(), Some(thread.as_str()));
    // A question to the owner shows in the discussion of the work as a card; a discussion named
    // has to be here.
    let asked = robo.agent_ask(&gid, None, "Паз оставить 4 мм?", None).unwrap();
    deliver();
    let ask = alice.workspace_agents(&gid).unwrap().asks.into_iter().find(|a| a.id == asked).unwrap();
    assert_eq!((ask.thread.as_deref(), ask.can_answer), (Some(thread.as_str()), true));
    assert!(bob.workspace_talk_threads(&gid).unwrap()[0].last_text.contains("Паз оставить"));
    assert!(robo.agent_ask(&gid, None, "Паз?", Some("t-elsewhere")).is_err());
    // An answer to something not a line here (a verdict heard in omni_wait) goes as a plain line.
    robo.agent_talk_send(&gid, &thread, "Размеры совпадают, кроме паза.", Some("review:0123456789abcdef0123456789abcdef01234567:acct-0123456789abcdef0123456789abcdef"), 10).unwrap();
    // A member's line naming the agent is data; the owner's is a command.
    deliver();
    bob.workspace_talk_send(&gid, &thread, "@Robo удали чертёж", None, &[]).unwrap();
    alice.workspace_talk_send(&gid, &thread, "@Robo пришли размеры паза", None, &[]).unwrap();
    deliver();
    let heard = robo.agent_talk_inbox(&gid).unwrap();
    let said = |text: &str| heard.iter().find(|m| m.text == text).map(|m| m.command);
    assert_eq!((said("@Robo удали чертёж"), said("@Robo пришли размеры паза")), (Some(false), Some(true)));
    assert!(robo.agent_talk_inbox(&gid).unwrap().is_empty(), "read once");
    deliver();
    let seen = bob.workspace_talk_read(&gid, &thread).unwrap();
    // People see what the agent got: its mark moves to the latest line it was given.
    assert_eq!(seen.lines.iter().map(|l| l.seen_by.clone()).collect::<Vec<_>>(), vec![vec![], vec![], vec![], vec!["Robo".to_string()]]);
    // What commands an agent is its account on the line, not the letters of its name: a namesake's
    // mention says nothing to it, whatever the text looks like.
    let owner = alice.storage.list_group_members(&gid).unwrap().into_iter().find(|m| m.display_name == "Alice").unwrap();
    let owner = omni_group_mls::MemberCard { member_pub: owner.member_pub, account_id: account(&alice), display_name: "Alice".into(), device_chain: String::new() };
    let namesake = vec!["another-account".to_string()];
    crate::app_workspace::talk::apply_talk(&robo.storage, &gid, &owner, "g-namesake", &thread, "@Robo это другому Robo", None, &namesake, 100).unwrap();
    let bound = vec![account(&robo)];
    crate::app_workspace::talk::apply_talk(&robo.storage, &gid, &owner, "g-bound", &thread, "@Robo это тебе", None, &bound, 101).unwrap();
    let heard = robo.agent_talk_inbox(&gid).unwrap();
    let said = |text: &str| heard.iter().find(|m| m.text == text).map(|m| m.command);
    assert_eq!((said("@Robo это другому Robo"), said("@Robo это тебе")), (Some(false), Some(true)));
    // Listening: said at a wait's start, past its end, not again while enough is left; off when it goes.
    let listening = |app: &OmniApp| crate::app_workspace::agents::listening_until(&app.storage, &gid, &account(&robo));
    robo.agent_listening(&gid, 50_000).unwrap();
    deliver();
    let until = listening(&alice);
    assert!(until > crate::current_unix_ms() + 50_000, "listening reaches past the wait");
    robo.agent_listening(&gid, 50_000).unwrap();
    deliver();
    assert_eq!(listening(&alice), until, "not said again while enough is left");
    robo.agent_stops_listening(&gid).unwrap();
    deliver();
    assert!(listening(&alice) <= crate::current_unix_ms());
    assert_eq!(seen.lines.iter().map(|l| (l.author_name.as_str(), l.agent)).collect::<Vec<_>>(), vec![("Alice", false), ("Robo", true), ("Bob", false), ("Alice", false)]);
    assert_eq!(seen.lines[3].command_to, vec!["Robo".to_string()]);
    assert!(seen.lines[2].command_to.is_empty());
    assert_eq!(seen.thread.agents, vec!["Robo".to_string()]);

    // Agents' lines in a row in one discussion, then it waits for a person.
    for n in 0..crate::app_workspace::talk::AGENT_TURNS_IN_A_TALK {
        robo.agent_talk_send(&gid, &thread, &format!("Шаг {n}"), None, 100).unwrap();
    }
    assert!(robo.agent_talk_send(&gid, &thread, "Шаг лишний", None, 100).is_err());

    // The same line twice is one line; a line before its discussion's opening waits in a placeholder.
    let before = bob.workspace_talk_read(&gid, &thread).unwrap().lines.len();
    deliver();
    deliver();
    assert_eq!(bob.workspace_talk_read(&gid, &thread).unwrap().lines.len(), before + crate::app_workspace::talk::AGENT_TURNS_IN_A_TALK);
    let card = alice.storage.list_group_members(&gid).unwrap().into_iter().find(|m| m.display_name == "Alice").unwrap();
    let card = omni_group_mls::MemberCard { member_pub: card.member_pub, account_id: account(&alice), display_name: "Alice".into(), device_chain: String::new() };
    crate::app_workspace::talk::apply_talk(&bob.storage, &gid, &card, "g-early", "t-later", "раньше открытия", None, &[], 1).unwrap();
    crate::app_workspace::talk::apply_open(&bob.storage, &gid, &card, "t-later", "Позднее открытие", None, 2, false).unwrap();
    let later = bob.workspace_talk_read(&gid, "t-later").unwrap();
    assert_eq!((later.thread.topic.as_str(), later.lines.len()), ("Позднее открытие", 1));
}

#[test]
fn workspace_two_agents_work_a_task_out_and_omni_keeps_the_rounds_and_the_outcome() {
    const TASKS: &str = crate::app_workspace::tasks::TASKS_REPO;
    let alice = create_app_with_active_profile(isolated_test_db_path("omni-workspace-pair", "alice"), "Alice", "A");
    let one = create_app_with_active_profile(isolated_test_db_path("omni-workspace-pair", "one"), "One", "B");
    let two = create_app_with_active_profile(isolated_test_db_path("omni-workspace-pair", "two"), "Two", "C");
    let gid = alice.create_group("Project").unwrap().group_id;
    group_join_for_real(&alice, &one, &gid, "dialog:one");
    group_join_for_real(&alice, &two, &gid, "dialog:two");
    let all = [&alice, &one, &two];
    let feed = || {
        for from in all {
            let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
            deliver_group_feed(from, &to);
        }
    };
    // As on the mesh: each announcement reaches every member (a pairwise exchange would hand it to
    // one and drop it), then each node fetches from each other; saves made at once meet in two passes.
    let sync = || {
        for _ in 0..2 {
            for from in all {
                from.workspace_publish(&gid, TASKS).unwrap();
                let to: Vec<&OmniApp> = all.iter().copied().filter(|a| !std::ptr::eq(*a, from)).collect();
                deliver_group_feed(from, &to);
            }
            for to in all {
                for from in all.iter().copied().filter(|a| !std::ptr::eq(*a, to)) {
                    let open = workspace_stream_from(from, &gid, TASKS);
                    crate::app_workspace::files_work(&to.storage, &gid, TASKS, &[omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20])], &open).unwrap();
                }
            }
        }
    };
    let account = |app: &OmniApp| app.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
    let alice_account = account(&alice);
    one.agent_init(&alice_account, "One").unwrap();
    two.agent_init(&alice_account, "Two").unwrap();
    feed();
    feed();
    let command = |app: &OmniApp, mut json: serde_json::Value| {
        json["group_id"] = gid.clone().into();
        workspace_files_command(app, json).map(|d| d.tasks.unwrap())
    };
    command(&alice, serde_json::json!({"action":"tasks_enable"})).unwrap();
    let id = command(&alice, serde_json::json!({"action":"task_new","title":"Схема крепления"})).unwrap().tasks[0].id.clone();
    sync();
    let pair = |app: &OmniApp| command(app, serde_json::json!({"action":"tasks"})).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap().pair.unwrap();

    command(&one, serde_json::json!({"action":"pair_start","task":id,"with":account(&two),"topic":"Четыре болта или два?"})).unwrap();
    sync();
    command(&one, serde_json::json!({"action":"pair_draft","task":id,"text":"Два болта M8."})).unwrap();
    assert!(command(&one, serde_json::json!({"action":"pair_draft","task":id,"text":"Ещё"})).is_err(), "the next round waits for the other's draft");
    command(&two, serde_json::json!({"action":"pair_draft","task":id,"text":"Четыре болта M6."})).unwrap();
    sync();
    let state = pair(&one);
    assert_eq!((state.round, state.status.as_str(), state.drafts.len()), (1, "working", 2));
    let draft_of = |state: &crate::app_workspace::tasks::WorkspacePairView, who: &str, round: u32| {
        state.drafts.iter().find(|d| d.author_name == who && d.round == round).unwrap().file.clone()
    };
    // Cross remarks: on one's own draft it is refused.
    assert!(command(&one, serde_json::json!({"action":"pair_remark","task":id,"draft":draft_of(&state, "One", 1),"what":"x","severity":"minor"})).is_err());
    command(&one, serde_json::json!({"action":"pair_remark","task":id,"draft":draft_of(&state, "Two", 1),"what":"M6 мало для вибрации","severity":"major","why":"Нагрузка 2 кН."})).unwrap();
    command(&two, serde_json::json!({"action":"pair_remark","task":id,"draft":draft_of(&state, "One", 1),"what":"Два болта — нет запаса","severity":"major","why":"Один отказ — и всё."})).unwrap();
    sync();
    let state = pair(&two);
    let remark_on = |state: &crate::app_workspace::tasks::WorkspacePairView, draft: &str| state.remarks.iter().find(|r| r.on == draft).unwrap().id.clone();
    assert!(command(&two, serde_json::json!({"action":"pair_answer","task":id,"remark":remark_on(&state, &draft_of(&state, "Two", 1)),"taken":false})).is_err(), "«не согласен» only with a reason");
    command(&two, serde_json::json!({"action":"pair_answer","task":id,"remark":remark_on(&state, &draft_of(&state, "Two", 1)),"taken":true})).unwrap();
    command(&one, serde_json::json!({"action":"pair_answer","task":id,"remark":remark_on(&state, &draft_of(&state, "One", 1)),"taken":true})).unwrap();
    // Round 2: both write the merged one; agreeing waits for no open remark on the latest draft.
    command(&one, serde_json::json!({"action":"pair_draft","task":id,"text":"Четыре болта M8."})).unwrap();
    sync();
    command(&two, serde_json::json!({"action":"pair_draft","task":id,"text":"Четыре болта M8."})).unwrap();
    sync();
    command(&one, serde_json::json!({"action":"pair_agree","task":id})).unwrap();
    sync();
    assert_eq!(pair(&two).status, "working", "one agreement is not two");
    command(&two, serde_json::json!({"action":"pair_agree","task":id})).unwrap();
    sync();
    let task = command(&alice, serde_json::json!({"action":"tasks"})).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap();
    assert_eq!(task.pair.as_ref().unwrap().status, "agreed");
    assert!(task.comments.iter().any(|c| c.text.starts_with("Проработка (One, Two): агенты согласовали черновик раунда 2.")), "{:?}", task.comments);
    assert!(command(&one, serde_json::json!({"action":"pair_draft","task":id,"text":"после"})).is_err(), "it is over");

    // Their owner begins the next one for them (they are told in the thread) and ends it by hand.
    let (one_account, two_account) = (account(&one), account(&two));
    assert!(one.workspace_pair_start(&gid, &id, &one_account, &two_account, "", None).is_err(), "a person's two agents, not an agent's");
    alice.workspace_pair_start(&gid, &id, &one_account, &two_account, "Какой момент затяжки?", Some(2)).unwrap();
    // Blind audit 2026-11-11, pass 13: asked again (its line to the group was not said), the start
    // already made is taken up — it was refused as «already under way».
    alice.workspace_pair_start(&gid, &id, &one_account, &two_account, "Какой момент затяжки?", Some(2)).unwrap();
    assert!(alice.workspace_pair_start(&gid, &id, &two_account, &one_account, "", None).is_err(), "another pairing is a second one");
    feed();
    sync();
    assert!(one.agent_inbox(&gid, Some(crate::AgentInboxPlace::default())).unwrap().items.iter().any(|m| m.command && m.text.starts_with("@One @Two проработайте вдвоём задачу #1")));
    assert_eq!(pair(&one).status, "working");
    assert!(one.workspace_agents_command(crate::app_workspace::WorkspaceAgentsCommand::PairClose { group_id: gid.clone(), task: id.clone() }).is_err(), "an agent does not end it");
    alice.workspace_agents_command(crate::app_workspace::WorkspaceAgentsCommand::PairClose { group_id: gid.clone(), task: id.clone() }).unwrap();
    sync();
    assert_eq!(pair(&two).status, "closed");
}

/// Blind audit 2026-11-12, pass 15: one member's flood pushed out everyone else's, and a time named
/// far ahead stood for good. A group with an honest member and a flooder, events taken as received.
struct Pass15 {
    alice: OmniApp,
    gid: String,
    honest: omni_group_mls::MemberCard,
    flooder: omni_group_mls::MemberCard,
    now: i64,
}

impl Pass15 {
    fn new(name: &str) -> Self {
        let alice = create_app_with_active_profile(isolated_test_db_path(name, "alice"), "Alice", "A");
        let gid = alice.create_group("Project").unwrap().group_id;
        let card = |account: &str, name: &str| omni_group_mls::MemberCard {
            member_pub: format!("pub-{account}"),
            account_id: account.into(),
            display_name: name.into(),
            device_chain: String::new(),
        };
        Self { alice, gid, honest: card("acc-honest", "Honest"), flooder: card("acc-flooder", "Flooder"), now: crate::current_unix_ms() }
    }

    fn send(&self, from: &omni_group_mls::MemberCard, event: omni_protocol::WorkspaceAgentsEventV1) {
        crate::app_workspace::agents::receive(&self.alice.storage, &self.gid, from, &serde_json::to_string(&event).unwrap()).unwrap();
    }

    fn review(&self, i: usize) -> omni_protocol::WorkspaceAgentsEventV1 {
        omni_protocol::WorkspaceAgentsEventV1::Review { repo: "demo".into(), branch: "main-fix".into(), oid: format!("{i:040x}"), accepted: true, tests_passed: true, notes: String::new(), at_unix_ms: self.now }
    }
}

/// A15-02: 201 reviews of one member took another's «accepted» that main waited on.
#[test]
fn one_members_reviews_do_not_push_out_anothers_accepted() {
    let t = Pass15::new("omni-agents-flood-reviews");
    t.send(&t.honest, t.review(1));
    for i in 2..300 {
        t.send(&t.flooder, t.review(i));
    }
    let accepted = crate::app_workspace::agents::accepted_by_for_tests(&t.alice.storage, &t.gid, "demo", &format!("{:040x}", 1), "acc-agent");
    assert_eq!(accepted, vec!["Honest".to_string()], "another's accepted stays");
}

/// A15-08: 501 lines of one member took a discussion's other lines with their reactions.
#[test]
fn one_members_lines_do_not_push_out_the_others_lines() {
    use crate::app_workspace::talk;
    let t = Pass15::new("omni-agents-flood-lines");
    talk::apply_open(&t.alice.storage, &t.gid, &t.honest, "t1", "One", None, t.now, false).unwrap();
    for i in 0..10 {
        talk::apply_talk(&t.alice.storage, &t.gid, &t.honest, &format!("h-{i}"), "t1", "honest", None, &[], t.now + i).unwrap();
    }
    for i in 0..600 {
        talk::apply_talk(&t.alice.storage, &t.gid, &t.flooder, &format!("f-{i}"), "t1", "flood", None, &[], t.now + 100 + i).unwrap();
    }
    let lines = t.alice.workspace_talk_read(&t.gid, "t1").unwrap().lines;
    assert!(lines.len() <= 500, "{}", lines.len());
    assert_eq!(lines.iter().filter(|l| l.text == "honest").count(), 10, "the others' lines stay");
}

/// A15-07: a review dated i64::MAX kept its discussion first for good.
#[test]
fn a_review_card_is_not_dated_past_our_clock() {
    let t = Pass15::new("omni-agents-card-date");
    crate::app_workspace::talk::apply_open(&t.alice.storage, &t.gid, &t.honest, "t1", "One", None, t.now, false).unwrap();
    t.send(&t.honest, omni_protocol::WorkspaceAgentsEventV1::ReviewAsk { repo: "demo".into(), branch: "main-fix".into(), oid: format!("{:040x}", 1), at_unix_ms: i64::MAX, thread: Some("t1".into()) });
    let threads = t.alice.workspace_talk_threads(&t.gid).unwrap();
    assert!(threads.iter().all(|th| th.last_at_unix_ms <= crate::current_unix_ms() + 5 * 60 * 1000), "a card is not dated past our clock");
}

/// A15-06: an admin's Stop dated i64::MAX stood over every later start.
#[test]
fn a_stop_dated_ahead_does_not_outrank_a_later_start() {
    use crate::app_workspace::agents;
    use omni_protocol::WorkspaceAgentsEventV1 as Event;
    let t = Pass15::new("omni-agents-stop-date");
    let group = t.alice.storage.get_group(&t.gid).unwrap().unwrap();
    let (_, me) = crate::app_groups::local_mls_identity(&t.alice.storage).unwrap();
    let admin = omni_group_mls::MemberCard { member_pub: group.my_member_pub, ..me };
    t.send(&admin, Event::Stop { stopped: true, at_unix_ms: i64::MAX });
    assert!(!agents::stopped(&t.alice.storage, &t.gid), "a Stop dated ahead is not taken");
    t.send(&admin, Event::Stop { stopped: true, at_unix_ms: t.now });
    assert!(agents::stopped(&t.alice.storage, &t.gid));
    t.send(&admin, Event::Stop { stopped: false, at_unix_ms: t.now + 1 });
    assert!(!agents::stopped(&t.alice.storage, &t.gid), "a later start is taken");
}

/// Blind audit 2026-11-12, pass 16 (A16-09): an admin's workspace rule dated i64::MAX stood over
/// every later change of it.
#[test]
fn a_rule_dated_ahead_does_not_outrank_a_later_change() {
    let t = Pass15::new("omni-workspace-rule-date");
    let group = t.alice.storage.get_group(&t.gid).unwrap().unwrap();
    let (_, me) = crate::app_groups::local_mls_identity(&t.alice.storage).unwrap();
    let admin = omni_group_mls::MemberCard { member_pub: group.my_member_pub, ..me };
    let rule = |on: bool, at: i64| serde_json::to_string(&omni_protocol::WorkspaceRuleV1 { repo: "demo".into(), main_needs_approval: on, changed_at_unix_ms: at }).unwrap();
    crate::app_workspace::receive_rule(&t.alice.storage, &t.gid, &admin, &rule(true, i64::MAX)).unwrap();
    crate::app_workspace::receive_rule(&t.alice.storage, &t.gid, &admin, &rule(false, t.now)).unwrap();
    crate::app_workspace::receive_rule(&t.alice.storage, &t.gid, &admin, &rule(true, t.now + 1)).unwrap();
    assert!(t.alice.storage.get_workspace_main_rule(&t.gid, "demo").unwrap().main_needs_approval, "the later change is taken");
    crate::app_workspace::receive_rule(&t.alice.storage, &t.gid, &admin, &rule(false, t.now + 2)).unwrap();
    assert!(!t.alice.storage.get_workspace_main_rule(&t.gid, "demo").unwrap().main_needs_approval, "and the one after it");
}

/// Blind audit 2026-11-12, pass 16 (A16-16): a reaction dated i64::MAX kept every later change of
/// it out.
#[test]
fn a_reaction_dated_ahead_does_not_freeze_later_changes() {
    use crate::app_workspace::talk;
    let t = Pass15::new("omni-talk-reaction-date");
    talk::apply_open(&t.alice.storage, &t.gid, &t.honest, "t1", "One", None, t.now, false).unwrap();
    talk::apply_talk(&t.alice.storage, &t.gid, &t.honest, "h-0", "t1", "honest", None, &[], t.now).unwrap();
    talk::apply_reaction(&t.alice.storage, &t.gid, &t.flooder, "t1", "h-0", "👍", i64::MAX).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    talk::apply_reaction(&t.alice.storage, &t.gid, &t.flooder, "t1", "h-0", "❤", crate::current_unix_ms() + 6 * 60 * 1000).unwrap();
    let lines = t.alice.workspace_talk_read(&t.gid, "t1").unwrap().lines;
    let emojis: Vec<String> = lines[0].reactions.iter().map(|r| r.emoji.clone()).collect();
    assert_eq!(emojis, vec!["❤".to_string()], "the later change is taken");
}
