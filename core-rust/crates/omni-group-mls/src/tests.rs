use super::*;
use base64::Engine as _;

/// A member with its own database (in memory) and account key.
struct Node {
    conn: Connection,
    seed: [u8; 32],
    card: MemberCard,
}

impl Node {
    fn new(name: &str) -> Self {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let mut conn = Connection::open_in_memory().unwrap();
        Engine::<JsonCodec>::migrate(&mut conn).unwrap();
        Self {
            conn,
            seed: key.to_bytes(),
            card: MemberCard {
                member_pub: base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes()),
                account_id: format!("acct-{name}"),
                display_name: name.to_string(),
            },
        }
    }

    fn engine(&self) -> Engine<'_, JsonCodec> {
        Engine::new(&self.conn, self.seed, self.card.clone())
    }
}

const GID: &[u8] = b"omni-test-group-0001";

fn text(inbound: Inbound) -> (String, String) {
    match inbound {
        Inbound::Application { sender, payload, .. } => {
            (sender.display_name, String::from_utf8(payload).unwrap())
        }
        other => panic!("expected an application message, got {other:?}"),
    }
}

fn outcome(inbound: Inbound) -> CommitOutcome {
    match inbound {
        Inbound::Commit { outcome, .. } => outcome,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// `names[0]` creates the group and adds the rest in one commit.
fn group_of(names: &[&str]) -> Vec<Node> {
    let nodes: Vec<Node> = names.iter().map(|n| Node::new(n)).collect();
    nodes[0].engine().create_group(GID, "Друзья").unwrap();
    if nodes.len() > 1 {
        let kps: Vec<Vec<u8>> = nodes[1..].iter().map(|n| n.engine().key_package().unwrap()).collect();
        let out = nodes[0].engine().add_members(GID, &kps).unwrap();
        for n in &nodes[1..] {
            assert_eq!(n.engine().join(&out.welcome).unwrap(), GID);
        }
    }
    nodes
}

#[test]
fn members_talk_see_each_other_and_a_removed_one_reads_nothing_new() {
    let g = group_of(&["alice", "bob", "carol"]);
    let view = g[1].engine().view(GID).unwrap();
    assert_eq!(view.name, "Друзья");
    assert_eq!(view.members.len(), 3);
    assert!(view.members.iter().any(|m| m.card.display_name == "alice" && m.is_admin));
    assert!(view.members.iter().any(|m| m.card.display_name == "bob" && m.is_me && !m.is_admin));

    let (_, from_bob) = g[1].engine().encrypt(GID, b"hi all").unwrap();
    assert_eq!(text(g[0].engine().receive(GID, &from_bob).unwrap()), ("bob".into(), "hi all".into()));
    assert_eq!(text(g[2].engine().receive(GID, &from_bob).unwrap()), ("bob".into(), "hi all".into()));

    let commit = g[0].engine().remove_members(GID, &[g[2].card.member_pub.clone()]).unwrap();
    assert_eq!(outcome(g[1].engine().receive(GID, &commit).unwrap()), CommitOutcome::Merged);
    assert_eq!(outcome(g[2].engine().receive(GID, &commit).unwrap()), CommitOutcome::Merged);
    assert!(!g[2].engine().view(GID).unwrap().active, "carol knows she was removed");

    // Negative half: what is said after the removal does not open for Carol.
    let (_, after) = g[0].engine().encrypt(GID, b"carol is gone").unwrap();
    assert_eq!(text(g[1].engine().receive(GID, &after).unwrap()).1, "carol is gone");
    assert!(matches!(g[2].engine().receive(GID, &after).unwrap(), Inbound::Ignored(_)));
}

#[test]
fn anyone_adds_but_only_an_admin_removes_or_renames() {
    let g = group_of(&["alice", "bob", "carol"]);
    // Bob (not an admin) adds Dave — allowed.
    let dave = Node::new("dave");
    let out = g[1].engine().add_members(GID, &[dave.engine().key_package().unwrap()]).unwrap();
    assert_eq!(outcome(g[0].engine().receive(GID, &out.commit).unwrap()), CommitOutcome::Merged);
    dave.engine().join(&out.welcome).unwrap();

    // Negative half: Bob may not remove or rename — refused locally…
    assert!(matches!(
        g[1].engine().remove_members(GID, &[g[2].card.member_pub.clone()]),
        Err(GroupError::NotAllowed(_))
    ));
    assert!(matches!(g[1].engine().update_meta(GID, |m| m.name = "Bob's".into()), Err(GroupError::NotAllowed(_))));

    // …and an admin's rename goes through for everyone.
    let rename = g[0].engine().update_meta(GID, |m| m.name = "Семья".into()).unwrap();
    // (Bob authored the add — his own commit echoes back as "already merged".)
    assert!(matches!(g[1].engine().receive(GID, &out.commit).unwrap(), Inbound::Ignored(_)));
    assert_eq!(outcome(g[1].engine().receive(GID, &rename).unwrap()), CommitOutcome::Merged);
    assert_eq!(g[1].engine().view(GID).unwrap().name, "Семья");
}

/// A non-admin who forges a removal with a patched client is refused by every honest member.
#[test]
fn a_removal_by_a_non_admin_is_rejected_by_the_others() {
    let g = group_of(&["alice", "bob", "carol"]);
    // Bob bypasses his own check by calling OpenMLS directly.
    let commit = {
        let engine = g[1].engine();
        let mut group = engine.load(GID).unwrap();
        let carol = group
            .members()
            .find(|m| Engine::<JsonCodec>::card_of(&m.credential).unwrap().display_name == "carol")
            .unwrap()
            .index;
        let (commit, _, _) = group.remove_members(&engine.provider, &engine.signer, &[carol]).unwrap();
        wire(&commit).unwrap()
    };
    match outcome(g[0].engine().receive(GID, &commit).unwrap()) {
        CommitOutcome::Rejected(reason) => assert!(reason.contains("non-admin"), "{reason}"),
        other => panic!("must be rejected, got {other:?}"),
    }
    assert_eq!(g[0].engine().view(GID).unwrap().members.len(), 3, "carol stays");
    // The group still works for alice ↔ carol.
    let (_, hi) = g[0].engine().encrypt(GID, b"still here").unwrap();
    assert_eq!(text(g[2].engine().receive(GID, &hi).unwrap()).1, "still here");
}

#[test]
fn a_member_leaves_and_an_admin_removes_them() {
    let g = group_of(&["alice", "bob", "carol"]);
    // Carol may leave; she forgets the group; an admin removes her with an ordinary commit.
    g[2].engine().can_leave(GID).unwrap();
    g[2].engine().delete_group(GID).unwrap();
    assert!(matches!(g[2].engine().view(GID), Err(GroupError::UnknownGroup)));
    let commit = g[0].engine().remove_members(GID, &[g[2].card.member_pub.clone()]).unwrap();
    assert_eq!(outcome(g[1].engine().receive(GID, &commit).unwrap()), CommitOutcome::Merged);
    assert_eq!(g[1].engine().view(GID).unwrap().members.len(), 2);

    // Negative half: the only admin cannot walk out of a group that still has members…
    assert!(matches!(g[0].engine().can_leave(GID), Err(GroupError::NotAllowed(_))));
    // …but can after naming another admin.
    let promote = g[0].engine().update_meta(GID, |m| m.admins.push(g[1].card.member_pub.clone())).unwrap();
    assert_eq!(outcome(g[1].engine().receive(GID, &promote).unwrap()), CommitOutcome::Merged);
    g[0].engine().can_leave(GID).unwrap();
    assert!(g[1].engine().view(GID).unwrap().members.iter().any(|m| m.is_me && m.is_admin));
}

#[test]
fn a_newcomer_does_not_read_what_was_said_before_joining() {
    let g = group_of(&["alice", "bob"]);
    let (_, before) = g[0].engine().encrypt(GID, b"before dave").unwrap();
    let dave = Node::new("dave");
    let out = g[1].engine().add_members(GID, &[dave.engine().key_package().unwrap()]).unwrap();
    assert_eq!(outcome(g[0].engine().receive(GID, &out.commit).unwrap()), CommitOutcome::Merged);
    dave.engine().join(&out.welcome).unwrap();
    assert!(matches!(dave.engine().receive(GID, &before).unwrap(), Inbound::Ignored(_)));
    let (_, after) = g[0].engine().encrypt(GID, b"welcome dave").unwrap();
    assert_eq!(text(dave.engine().receive(GID, &after).unwrap()), ("alice".into(), "welcome dave".into()));
}

/// Store-and-forward reorders: a message from the epoch before a Commit still opens after it, and a
/// message from an epoch not reached yet is reported as such (to be offered again).
#[test]
fn late_and_early_messages_around_a_commit() {
    let g = group_of(&["alice", "bob", "carol"]);
    let (_, old) = g[1].engine().encrypt(GID, b"sent before the commit").unwrap();
    let dave = Node::new("dave");
    let out = g[0].engine().add_members(GID, &[dave.engine().key_package().unwrap()]).unwrap();
    let (_, new) = g[0].engine().encrypt(GID, b"sent after the commit").unwrap();

    // Carol gets the new-epoch message before the commit: not yet.
    assert!(matches!(g[2].engine().receive(GID, &new).unwrap(), Inbound::Future { .. }));
    assert_eq!(outcome(g[2].engine().receive(GID, &out.commit).unwrap()), CommitOutcome::Merged);
    assert_eq!(text(g[2].engine().receive(GID, &new).unwrap()).1, "sent after the commit");
    // The old-epoch message, arriving late, still opens.
    assert_eq!(text(g[2].engine().receive(GID, &old).unwrap()).1, "sent before the commit");
}

/// Two members change the group in the same epoch; whatever order each member sees the Commits in,
/// all end on the SAME branch (the smaller rank), and the loser learns its Commit was lost.
#[test]
fn concurrent_commits_converge_on_one_winner_in_any_arrival_order() {
    let g = group_of(&["alice", "bob", "carol"]);
    let start = g[0].engine().view(GID).unwrap().epoch;
    let dave = Node::new("dave");
    let erin = Node::new("erin");
    let a = g[0].engine().add_members(GID, &[dave.engine().key_package().unwrap()]).unwrap();
    let b = g[1].engine().add_members(GID, &[erin.engine().key_package().unwrap()]).unwrap();
    let a_wins = commit_rank(&a.commit) < commit_rank(&b.commit);

    let alice_saw = outcome(g[0].engine().receive(GID, &b.commit).unwrap());
    let bob_saw = outcome(g[1].engine().receive(GID, &a.commit).unwrap());
    assert_eq!(outcome(g[2].engine().receive(GID, &b.commit).unwrap()), CommitOutcome::Merged);
    let carol_second = outcome(g[2].engine().receive(GID, &a.commit).unwrap());
    if a_wins {
        assert_eq!(alice_saw, CommitOutcome::LostIgnored);
        assert_eq!(bob_saw, CommitOutcome::ReplacedLoser { our_commit_lost: true });
        assert_eq!(carol_second, CommitOutcome::ReplacedLoser { our_commit_lost: false });
    } else {
        assert_eq!(alice_saw, CommitOutcome::ReplacedLoser { our_commit_lost: true });
        assert_eq!(bob_saw, CommitOutcome::LostIgnored);
        assert_eq!(carol_second, CommitOutcome::LostIgnored);
    }
    let views: Vec<GroupView> = g.iter().map(|n| n.engine().view(GID).unwrap()).collect();
    assert!(views.iter().all(|v| v.epoch == start + 1 && v.members.len() == 4), "one branch, 4 members");
    let names: Vec<Vec<String>> = views
        .iter()
        .map(|v| v.members.iter().map(|m| m.card.display_name.clone()).collect())
        .collect();
    assert!(names.windows(2).all(|w| w[0] == w[1]), "{names:?}");

    // The loser re-adds its newcomer in the next epoch; the group of five works.
    let (winner, loser, winner_new, loser_new, winner_welcome) =
        if a_wins { (0, 1, &dave, &erin, a.welcome) } else { (1, 0, &erin, &dave, b.welcome) };
    winner_new.engine().join(&winner_welcome).unwrap();
    let readd = g[loser].engine().add_members(GID, &[loser_new.engine().key_package().unwrap()]).unwrap();
    for (i, n) in g.iter().enumerate() {
        if i != loser {
            assert_eq!(outcome(n.engine().receive(GID, &readd.commit).unwrap()), CommitOutcome::Merged);
        }
    }
    assert_eq!(outcome(winner_new.engine().receive(GID, &readd.commit).unwrap()), CommitOutcome::Merged);
    loser_new.engine().join(&readd.welcome).unwrap();
    let (_, hello) = g[winner].engine().encrypt(GID, b"all five?").unwrap();
    for (i, n) in g.iter().enumerate() {
        if i != winner {
            assert_eq!(text(n.engine().receive(GID, &hello).unwrap()).1, "all five?");
        }
    }
    assert_eq!(text(winner_new.engine().receive(GID, &hello).unwrap()).1, "all five?");
    assert_eq!(text(loser_new.engine().receive(GID, &hello).unwrap()).1, "all five?");
}

/// A KeyPackage whose card claims someone else's account key is refused — a leaf cannot pose as
/// another person.
#[test]
fn a_member_card_must_match_its_signing_key() {
    let g = group_of(&["alice"]);
    let mut mallory = Node::new("mallory");
    let victim = Node::new("bob");
    mallory.card.member_pub = victim.card.member_pub.clone();
    mallory.card.display_name = "bob".into();
    let forged = mallory.engine().key_package().unwrap();
    match g[0].engine().add_members(GID, &[forged]) {
        Err(GroupError::Invalid(reason)) => assert!(reason.contains("does not match"), "{reason}"),
        other => panic!("must be refused, got {other:?}"),
    }
    // Negative half: the real Bob is accepted.
    g[0].engine().add_members(GID, &[victim.engine().key_package().unwrap()]).unwrap();
}

/// Measure (release only): a 100-member group, one add and one remove.
#[test]
#[cfg_attr(debug_assertions, ignore = "measurement: run with --release")]
fn a_hundred_members_cost_a_small_commit() {
    let names: Vec<String> = (0..100).map(|i| format!("m{i:03}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let g = group_of(&refs);
    let setup = started.elapsed();
    let newcomer = Node::new("m100");
    let t = std::time::Instant::now();
    let out = g[5].engine().add_members(GID, &[newcomer.engine().key_package().unwrap()]).unwrap();
    let add_ms = t.elapsed();
    let t = std::time::Instant::now();
    assert_eq!(outcome(g[0].engine().receive(GID, &out.commit).unwrap()), CommitOutcome::Merged);
    let process_ms = t.elapsed();
    let remove = g[0].engine().remove_members(GID, &[g[50].card.member_pub.clone()]).unwrap();
    println!(
        "100 members (sqlite): setup {setup:?}; add commit {} B + welcome {} B, author {add_ms:?}, receiver {process_ms:?}; remove commit {} B",
        out.commit.len(),
        out.welcome.len(),
        remove.len()
    );
    assert!(out.commit.len() < 16 * 1024 && remove.len() < 16 * 1024);
}

/// The group splits for longer than one epoch — {alice} and {bob, carol}, two renames on each side.
/// When they meet, the side whose first Commit ranks higher learns it lost (`ForkSeen`), asks to be
/// put back with a fresh KeyPackage on the feed, a member of the winning branch swaps it in, and it
/// comes back from the Welcome: one branch, one name, everyone reads everyone. Whichever side wins.
#[test]
fn a_group_that_went_on_in_two_branches_comes_back_to_one() {
    for _ in 0..6 {
        let g = group_of(&["alice", "bob", "carol"]);
        // Carol is an admin too, so both sides can rename.
        let make_admin = g[0]
            .engine()
            .update_meta(GID, |m| m.admins.push(g[2].card.member_pub.clone()))
            .unwrap();
        for n in &g[1..] {
            assert_eq!(outcome(n.engine().receive(GID, &make_admin).unwrap()), CommitOutcome::Merged);
        }
        let split = g[0].engine().view(GID).unwrap().epoch;

        // Apart: alice renames twice, carol renames twice and bob follows her.
        let x1 = g[0].engine().update_meta(GID, |m| m.name = "Икс-1".into()).unwrap();
        let x2 = g[0].engine().update_meta(GID, |m| m.name = "Икс-2".into()).unwrap();
        let y1 = g[2].engine().update_meta(GID, |m| m.name = "Игрек-1".into()).unwrap();
        assert_eq!(outcome(g[1].engine().receive(GID, &y1).unwrap()), CommitOutcome::Merged);
        let y2 = g[2].engine().update_meta(GID, |m| m.name = "Игрек-2".into()).unwrap();
        assert_eq!(outcome(g[1].engine().receive(GID, &y2).unwrap()), CommitOutcome::Merged);
        let x_wins = commit_rank(&x1) < commit_rank(&y1);

        // They meet: everyone gets everything the other side wrote.
        let mut lost: Vec<usize> = Vec::new();
        for (i, n) in g.iter().enumerate() {
            let foreign: &[&Vec<u8>] = if i == 0 { &[&y1, &y2] } else { &[&x1, &x2] };
            for commit in foreign {
                match n.engine().receive(GID, commit).unwrap() {
                    Inbound::ForkSeen { epoch, ours_wins } => {
                        assert_eq!(epoch, split, "the split is where they are compared");
                        if !ours_wins && !lost.contains(&i) {
                            lost.push(i);
                        }
                    }
                    Inbound::Commit { outcome: CommitOutcome::ReplacedLoser { .. }, .. } => {}
                    _ => {}
                }
            }
        }
        let losers: Vec<usize> = if x_wins { vec![1, 2] } else { vec![0] };
        assert_eq!(lost, losers, "x_wins={x_wins}");

        // Each loser asks; the first member of the winning branch puts it back.
        let winner = if x_wins { 0 } else { 1 };
        let winners: Vec<usize> = (0..3).filter(|i| !losers.contains(i)).collect();
        for &loser in &losers {
            let request = g[loser].engine().rejoin_request().unwrap();
            let Inbound::RejoinRequest { card, key_package } = g[winner].engine().receive(GID, &request).unwrap() else {
                panic!("a rejoin request");
            };
            assert_eq!(card.member_pub, g[loser].card.member_pub);
            let back = g[winner].engine().readd_member(GID, &key_package).unwrap();
            // The other winners (and losers already back) take the swap: a re-add is not a removal.
            for (i, n) in g.iter().enumerate() {
                if i != winner && i != loser && (winners.contains(&i) || n.engine().view(GID).unwrap().name == g[winner].engine().view(GID).unwrap().name) {
                    assert_eq!(outcome(n.engine().receive(GID, &back.commit).unwrap()), CommitOutcome::Merged);
                }
            }
            let Inbound::Welcome(welcome) = g[loser].engine().receive(GID, &back.welcome).unwrap() else {
                panic!("the welcome rides the feed");
            };
            assert_eq!(g[loser].engine().rejoin(&welcome).unwrap(), GID);
        }

        let views: Vec<GroupView> = g.iter().map(|n| n.engine().view(GID).unwrap()).collect();
        let name = if x_wins { "Икс-2" } else { "Игрек-2" };
        assert!(views.iter().all(|v| v.name == name && v.epoch == views[0].epoch && v.members.len() == 3), "{views:?}");
        for (i, n) in g.iter().enumerate() {
            let (_, hello) = n.engine().encrypt(GID, format!("от {i}").as_bytes()).unwrap();
            for (j, m) in g.iter().enumerate() {
                if i != j {
                    assert_eq!(text(m.engine().receive(GID, &hello).unwrap()).1, format!("от {i}"));
                }
            }
        }
    }
}

/// A KeyPackage on the feed from someone who is not in the group, or a Welcome that is not for us,
/// changes nothing.
#[test]
fn a_rejoin_from_a_stranger_or_a_welcome_for_someone_else_changes_nothing() {
    let g = group_of(&["alice", "bob"]);
    let stranger = Node::new("mallory");
    stranger.engine().create_group(b"another-group-0001", "x").unwrap();
    let request = stranger.engine().rejoin_request().unwrap();
    assert!(matches!(g[0].engine().receive(GID, &request).unwrap(), Inbound::Ignored(_)));
    let before = g[1].engine().view(GID).unwrap();
    let carol = Node::new("carol");
    let out = g[0].engine().add_members(GID, &[carol.engine().key_package().unwrap()]).unwrap();
    assert!(g[1].engine().rejoin(&out.welcome).is_err(), "not for bob");
    assert_eq!(g[1].engine().view(GID).unwrap(), before, "bob's group is as it was");
}
