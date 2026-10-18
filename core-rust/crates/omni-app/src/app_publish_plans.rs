// Where and how often each of our publications goes (docs/decisions/public-relay-directory.md §10,
// owner 2026-10-14: «один компонент везде»). Four publications — the beacons for contacts, the
// Omni-ID beacon, the name beacon, the relay directory entry — each with its own plan: steps of
// courier kinds (the kinds of one step at once, the next step only if no kind of this one took it)
// and a frequency on every block. «Авто» is a preset per publication; a manual edit switches to
// «Вручную», «Сбросить к Авто» goes back.
//
// Readers look at every courier whatever the plan says; the plan only decides where WE post.

use std::collections::HashMap;

use omni_types::{OmniError, OmniResult};

/// What is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Publication {
    /// The sealed beacons for each contact (pair topics).
    Contacts,
    /// The identity beacon under the Omni-ID («находить меня по Omni-ID»).
    OmniId,
    /// The name beacons («находить меня по имени»).
    Name,
    /// The public relay directory entry («быть релеем для всех»).
    Relay,
}

impl Publication {
    pub const ALL: [Publication; 4] = [Self::Contacts, Self::OmniId, Self::Name, Self::Relay];

    /// The kinds this publication may go to at all. Never the BT-DHT for a name (one record per key
    /// anyone can compute — a fight over who writes last, find-by-name.md §3) nor for the relay
    /// directory yet (Mainline `announce_peer` is stage 2 of the decision).
    pub fn allowed_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Contacts | Self::OmniId | Self::Relay => &["nostr", "dht", "ntfy"],
            Self::Name => &["nostr", "ntfy"],
        }
    }

    /// The kinds this publication cannot use, and why — the editor shows them switched off with the
    /// reason instead of leaving them out without a word.
    pub fn unavailable_kinds(self) -> Vec<UnavailableKindV1> {
        let reason = |kind: &str, why: &str| UnavailableKindV1 { kind: kind.to_string(), reason: why.to_string() };
        match self {
            Self::Contacts | Self::OmniId | Self::Relay => Vec::new(),
            Self::Name => vec![reason(
                "dht",
                "Не для имени: в Mainline под одним ключом одна запись — её перезапишет любой однофамилец.",
            )],
        }
    }

    /// The «Авто» preset.
    pub fn auto_plan(self) -> PublishPlanV1 {
        let block = |kind: &str, every_secs: u32| PublishBlockV1 { kind: kind.to_string(), every_secs };
        let steps = match self {
            // As before the editor: Nostr first (instant, no daily quota), then Mainline (no owner, no
            // quota), ntfy — the only one with a hard daily quota per IP — last; each once a window.
            Self::Contacts | Self::OmniId => vec![
                vec![block("nostr", WINDOW_SECS)],
                vec![block("dht", WINDOW_SECS)],
                vec![block("ntfy", WINDOW_SECS)],
            ],
            Self::Name => vec![vec![block("nostr", WINDOW_SECS)], vec![block("ntfy", WINDOW_SECS)]],
            // The directory: Nostr replaces the entry and it lives 30 min, so every 15 min; Mainline at
            // the same time — an announced address lives about half an hour in the swarm, and it costs
            // no quota (public-relay-directory.md, stage 2); ntfy keeps every message and is metered —
            // once a window, only if neither took it.
            Self::Relay => vec![
                vec![block("nostr", 15 * 60), block("dht", 15 * 60)],
                vec![block("ntfy", WINDOW_SECS)],
            ],
        };
        PublishPlanV1 { auto: true, steps }
    }
}

const WINDOW_SECS: u32 = omni_rendezvous::TOPIC_WINDOW_SECS as u32;

/// The frequencies a block may have (the editor's list): 5, 15, 30 min, 1 h, a window (12 h).
pub const PUBLISH_EVERY_CHOICES: [u32; 5] = [5 * 60, 15 * 60, 30 * 60, 60 * 60, WINDOW_SECS];

/// ntfy.sh allows 250 messages a day per IP, shared by everyone behind the router: no more often than this.
pub const NTFY_MIN_EVERY_SECS: u32 = 15 * 60;

/// One courier kind in a step, with how often it is posted to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishBlockV1 {
    /// `nostr` · `dht` · `ntfy`.
    pub kind: String,
    pub every_secs: u32,
}

/// A publication's plan: `steps[0]` first, the next only if no kind of the step took it. A kind in no
/// step is not used.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishPlanV1 {
    /// «Авто»: the publication's preset (steps are then that preset).
    pub auto: bool,
    pub steps: Vec<Vec<PublishBlockV1>>,
}

impl PublishPlanV1 {
    /// Kinds allowed for `publication`, each once; at least one block; known frequencies; ntfy not too
    /// often. Empty steps are dropped (the editor collapses them).
    pub fn validated(mut self, publication: Publication) -> OmniResult<Self> {
        if self.auto {
            return Ok(publication.auto_plan());
        }
        self.steps.retain(|step| !step.is_empty());
        let mut seen: Vec<&str> = Vec::new();
        for block in self.steps.iter().flatten() {
            if !publication.allowed_kinds().contains(&block.kind.as_str()) {
                return Err(OmniError::InvalidInput(format!("«{}» is not used for this publication", block.kind)));
            }
            if seen.contains(&block.kind.as_str()) {
                return Err(OmniError::InvalidInput(format!("«{}» is in the plan twice", block.kind)));
            }
            seen.push(&block.kind);
            if !PUBLISH_EVERY_CHOICES.contains(&block.every_secs) {
                return Err(OmniError::InvalidInput(format!("{} s is not one of the frequencies", block.every_secs)));
            }
            if block.kind == "ntfy" && block.every_secs < NTFY_MIN_EVERY_SECS {
                return Err(OmniError::InvalidInput("ntfy: no more often than every 15 minutes (its daily quota)".into()));
            }
        }
        if self.steps.is_empty() {
            return Err(OmniError::InvalidInput("at least one courier must stay on".into()));
        }
        Ok(self)
    }

    /// The steps with only the blocks due now: all of them when `force` (the content or the topic
    /// window changed), else those whose kind was last taken `every_secs` ago or never. A step with a
    /// block still fresh holds the announcement — the steps below it are a fallback for when it is not
    /// taken, so they are not reached. Nothing left — nothing to post.
    ///
    /// Until 2026-10-15 a fresh step was simply dropped, so the next one moved up and went out: Nostr
    /// took the beacon, and on the next sweep it went to Mainline and ntfy anyway — the fallback fired
    /// every time, spending ntfy's daily quota (stand: the PC relay posted to ntfy 100 s after Nostr
    /// and Mainline took its entry).
    pub fn due_steps(&self, last_taken_ms: &HashMap<String, u64>, now_ms: u64, force: bool) -> Vec<Vec<String>> {
        let is_due = |block: &PublishBlockV1| {
            force
                || last_taken_ms
                    .get(&block.kind)
                    .is_none_or(|at| now_ms.saturating_sub(*at) >= u64::from(block.every_secs) * 1000)
        };
        let mut steps = Vec::new();
        for step in &self.steps {
            let due: Vec<String> = step.iter().filter(|block| is_due(block)).map(|block| block.kind.clone()).collect();
            let holds = due.len() < step.len();
            if !due.is_empty() {
                steps.push(due);
            }
            if holds {
                break;
            }
        }
        steps
    }

    /// One line, as the editor shows it under the blocks.
    pub fn summary(&self) -> String {
        let name = |kind: &str| match kind {
            "nostr" => "Nostr",
            "dht" => "Mainline",
            "ntfy" => "ntfy",
            other => other,
        }
        .to_string();
        let mut parts: Vec<String> = Vec::new();
        for (n, step) in self.steps.iter().enumerate() {
            let kinds = step.iter().map(|block| name(&block.kind)).collect::<Vec<_>>().join(" и ");
            let took = if self.steps[n.saturating_sub(1)].len() > 1 { "не приняли" } else { "не принял" };
            parts.push(if n == 0 { format!("сразу в {kinds}") } else { format!("если {took} — в {kinds}") });
        }
        format!("Объявление уходит {}.", parts.join("; "))
    }
}

/// The helper's spread token for `steps` (`<reach>:nostr+dht,ntfy`).
pub(crate) fn spread_token(steps: &[Vec<String>], reach: u8) -> String {
    let steps: Vec<String> = steps.iter().map(|step| step.join("+")).collect();
    format!("{}:{}", reach.max(1), steps.join(","))
}

/// All four plans of a profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishPlansV1 {
    pub contacts: PublishPlanV1,
    pub omni_id: PublishPlanV1,
    pub name: PublishPlanV1,
    pub relay: PublishPlanV1,
}

impl Default for PublishPlansV1 {
    fn default() -> Self {
        Self {
            contacts: Publication::Contacts.auto_plan(),
            omni_id: Publication::OmniId.auto_plan(),
            name: Publication::Name.auto_plan(),
            relay: Publication::Relay.auto_plan(),
        }
    }
}

impl PublishPlansV1 {
    pub fn get(&self, publication: Publication) -> &PublishPlanV1 {
        match publication {
            Publication::Contacts => &self.contacts,
            Publication::OmniId => &self.omni_id,
            Publication::Name => &self.name,
            Publication::Relay => &self.relay,
        }
    }

    fn slot(&mut self, publication: Publication) -> &mut PublishPlanV1 {
        match publication {
            Publication::Contacts => &mut self.contacts,
            Publication::OmniId => &mut self.omni_id,
            Publication::Name => &mut self.name,
            Publication::Relay => &mut self.relay,
        }
    }
}

fn publish_plans_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "publish-plans.json")
}

/// The profile's plans; a plan that does not validate (an older file, a hand edit) is its «Авто».
pub(crate) fn load_publish_plans(db_path: &str) -> PublishPlansV1 {
    let mut plans: PublishPlansV1 = std::fs::read_to_string(publish_plans_path(db_path))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    for publication in Publication::ALL {
        let checked = plans.get(publication).clone().validated(publication).unwrap_or_else(|_| publication.auto_plan());
        *plans.slot(publication) = checked;
    }
    plans
}

/// A kind a publication cannot use, with the reason shown next to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnavailableKindV1 {
    pub kind: String,
    pub reason: String,
}

/// One publication's plan as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishPlanViewV1 {
    pub publication: Publication,
    pub plan: PublishPlanV1,
    /// Kinds this publication may use.
    pub allowed_kinds: Vec<String>,
    /// Kinds it may not, with why (shown switched off, not hidden).
    pub unavailable: Vec<UnavailableKindV1>,
    pub summary: String,
}

impl crate::OmniApp {
    /// The plan of each publication, for the editor.
    pub fn publish_plans(&self) -> Vec<PublishPlanViewV1> {
        let plans = load_publish_plans(&self.storage.db_path().to_string_lossy());
        Publication::ALL
            .iter()
            .map(|publication| {
                let plan = plans.get(*publication).clone();
                PublishPlanViewV1 {
                    publication: *publication,
                    allowed_kinds: publication.allowed_kinds().iter().map(|kind| kind.to_string()).collect(),
                    unavailable: publication.unavailable_kinds(),
                    summary: plan.summary(),
                    plan,
                }
            })
            .collect()
    }

    /// Set one publication's plan (`auto: true` — back to its preset). Applies at once: the rendezvous
    /// runtime restarts, as on a change of the courier set.
    pub fn set_publish_plan(&self, publication: Publication, plan: PublishPlanV1) -> OmniResult<PublishPlanViewV1> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let plan = plan.validated(publication)?;
        let mut plans = load_publish_plans(&db_path);
        *plans.slot(publication) = plan.clone();
        let json = serde_json::to_string_pretty(&plans).map_err(|error| OmniError::Internal(error.to_string()))?;
        let path = publish_plans_path(&db_path);
        std::fs::write(&path, json)
            .map_err(|error| OmniError::Storage(format!("write publish plans '{}': {error}", path.display())))?;
        crate::clear_rendezvous_runtime_for_profile(&db_path);
        crate::app_relay_directory::clear_relay_directory_for_profile(&db_path);
        Ok(PublishPlanViewV1 {
            publication,
            allowed_kinds: publication.allowed_kinds().iter().map(|kind| kind.to_string()).collect(),
            unavailable: publication.unavailable_kinds(),
            summary: plan.summary(),
            plan,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(kind: &str, every_secs: u32) -> PublishBlockV1 {
        PublishBlockV1 { kind: kind.into(), every_secs }
    }

    #[test]
    fn a_plan_is_checked_against_its_publication() {
        let manual = |steps| PublishPlanV1 { auto: false, steps };
        assert!(manual(vec![vec![block("nostr", 900), block("dht", 900)], vec![block("ntfy", WINDOW_SECS)]])
            .validated(Publication::Contacts)
            .is_ok());
        assert!(manual(vec![vec![block("dht", 900)]]).validated(Publication::Name).is_err(), "never the DHT for a name");
        assert!(manual(vec![vec![block("nostr", 900)], vec![block("nostr", 900)]]).validated(Publication::Contacts).is_err());
        assert!(manual(vec![vec![block("ntfy", 300)]]).validated(Publication::Contacts).is_err(), "ntfy's quota");
        assert!(manual(vec![vec![block("nostr", 7)]]).validated(Publication::Contacts).is_err());
        assert!(manual(vec![vec![], vec![]]).validated(Publication::Contacts).is_err(), "one block stays on");
        let collapsed = manual(vec![vec![], vec![block("nostr", 900)]]).validated(Publication::Contacts).unwrap();
        assert_eq!(collapsed.steps.len(), 1, "empty steps collapse");
        let auto = PublishPlanV1 { auto: true, steps: vec![] }.validated(Publication::Relay).unwrap();
        assert_eq!(auto, Publication::Relay.auto_plan());
    }

    #[test]
    fn only_the_blocks_due_go_and_a_change_sends_them_all() {
        let plan = PublishPlanV1 {
            auto: false,
            steps: vec![vec![block("nostr", 900), block("dht", WINDOW_SECS)], vec![block("ntfy", WINDOW_SECS)]],
        };
        let now = 10_000_000_000;
        let mut last = HashMap::new();
        assert_eq!(plan.due_steps(&last, now, false).len(), 2, "never posted: all due");
        last.insert("nostr".to_string(), now - 60_000);
        last.insert("dht".to_string(), now - 60_000);
        last.insert("ntfy".to_string(), now - 60_000);
        assert!(plan.due_steps(&last, now, false).is_empty(), "posted a minute ago");
        assert_eq!(plan.due_steps(&last, now + 900_000, false), vec![vec!["nostr".to_string()]]);
        assert_eq!(plan.due_steps(&last, now, true).len(), 2, "changed: everything");
        assert_eq!(spread_token(&plan.due_steps(&last, now, true), 1), "1:nostr+dht,ntfy");
    }

    /// The step below is a fallback: while the first one holds the announcement it is not reached,
    /// even if it was never posted to (it used to go out on the very next sweep, spending ntfy).
    #[test]
    fn a_fallback_step_is_not_reached_while_the_one_above_holds() {
        let plan = PublishPlanV1 {
            auto: false,
            steps: vec![vec![block("nostr", 900), block("dht", 900)], vec![block("ntfy", WINDOW_SECS)]],
        };
        let now = 10_000_000_000;
        let mut last = HashMap::new();
        last.insert("nostr".to_string(), now - 60_000);
        last.insert("dht".to_string(), now - 60_000);
        assert!(plan.due_steps(&last, now, false).is_empty(), "taken a minute ago: nothing, ntfy included");
        // The first step due again: it goes first, ntfy stays behind it as the fallback.
        assert_eq!(
            plan.due_steps(&last, now + 900_000, false),
            vec![vec!["nostr".to_string(), "dht".to_string()], vec!["ntfy".to_string()]]
        );
    }

    #[test]
    fn the_summary_reads_as_the_owner_wrote_it() {
        let plan = PublishPlanV1 {
            auto: false,
            steps: vec![vec![block("nostr", 900), block("dht", 900)], vec![block("ntfy", WINDOW_SECS)]],
        };
        assert_eq!(plan.summary(), "Объявление уходит сразу в Nostr и Mainline; если не приняли — в ntfy.");
    }
}
