//! Who a line names (`@Имя`). A name is what a person calls themselves and two members may share one,
//! so a mention is bound to the member's account when the line is sent, and travels with the line:
//! everyone downstream, an agent deciding whether it got a command above all, reads the accounts and
//! never parses the text again. The composer's suggestion list says which member was meant; a name typed
//! by hand binds if only one member has it, and with several the line is refused — a guess here could
//! hand a command to the wrong agent.

use omni_storage::GroupMemberRecord;
use omni_types::{OmniError, OmniResult};

/// What the app shows for this refusal is keyed on these words (`WorkspaceErrorText`).
const AMBIGUOUS: &str = "several members have the name";

/// The accounts `text` names, in order, each once. `picked`: the accounts chosen in the suggestion list.
pub(crate) fn resolve_mentions(members: &[GroupMemberRecord], text: &str, picked: &[String]) -> OmniResult<Vec<String>> {
    let mut left: Vec<&str> = picked.iter().map(String::as_str).collect();
    let mut found: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('@') {
        // `a@b` is an address, not a mention.
        if text[..at].chars().next_back().is_some_and(is_name_char) {
            continue;
        }
        let tail = &text[at + 1..];
        // A longer name wins at the same place: `@Анна Мария` is not `@Анна`.
        let mut longest = 0;
        let mut here: Vec<&GroupMemberRecord> = Vec::new();
        for member in members.iter().filter(|m| m.status == "member" && !m.account_id.is_empty() && !m.display_name.is_empty()) {
            let Some(length) = name_at(tail, &member.display_name) else { continue };
            if length > longest {
                longest = length;
                here.clear();
            }
            if length == longest && !here.iter().any(|m| m.account_id == member.account_id) {
                here.push(member);
            }
        }
        let Some(first) = here.first() else { continue };
        let account = match here.iter().position(|m| left.contains(&m.account_id.as_str())) {
            Some(chosen) => {
                let account = here[chosen].account_id.clone();
                left.retain(|a| *a != account);
                account
            }
            None if here.len() == 1 => first.account_id.clone(),
            None => return Err(OmniError::InvalidInput(format!("{AMBIGUOUS} {}", first.display_name))),
        };
        if !found.contains(&account) {
            found.push(account);
        }
    }
    Ok(found)
}

/// How many bytes of `tail` the name takes when it stands there as a whole word, any case.
fn name_at(tail: &str, name: &str) -> Option<usize> {
    let mut rest = tail.char_indices();
    let mut end = 0;
    for wanted in name.chars() {
        let (at, got) = rest.next()?;
        if !got.to_lowercase().eq(wanted.to_lowercase()) {
            return None;
        }
        end = at + got.len_utf8();
    }
    (!tail[end..].chars().next().is_some_and(is_name_char)).then_some(end)
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(account: &str, name: &str) -> GroupMemberRecord {
        GroupMemberRecord {
            member_pub: format!("pub-{account}"),
            account_id: account.into(),
            display_name: name.into(),
            role: "member".into(),
            status: "member".into(),
            node_hint_hex: String::new(),
        }
    }

    #[test]
    fn a_name_binds_only_as_a_whole_word() {
        let people = [member("a", "Helper"), member("b", "HelperBot")];
        assert_eq!(resolve_mentions(&people, "@HelperBot собери", &[]).unwrap(), ["b"]);
        assert_eq!(resolve_mentions(&people, "@helper, собери", &[]).unwrap(), ["a"]);
        assert!(resolve_mentions(&people, "@Helper-2 и x@Helper", &[]).unwrap().is_empty());
    }

    #[test]
    fn the_longer_name_wins_at_one_place() {
        let people = [member("a", "Анна"), member("b", "Анна Мария")];
        assert_eq!(resolve_mentions(&people, "@Анна Мария, привет", &[]).unwrap(), ["b"]);
        assert_eq!(resolve_mentions(&people, "@Анна, привет", &[]).unwrap(), ["a"]);
    }

    #[test]
    fn two_members_of_one_name_need_a_choice_from_the_list() {
        let people = [member("a", "Lina"), member("b", "Lina"), member("c", "Bob")];
        let refused = resolve_mentions(&people, "@Lina удали всё", &[]).unwrap_err().to_string();
        assert!(refused.contains(AMBIGUOUS), "{refused}");
        assert_eq!(resolve_mentions(&people, "@Lina удали всё", &["b".to_string()]).unwrap(), ["b"]);
        assert_eq!(resolve_mentions(&people, "@Lina и @Lina", &["b".to_string(), "a".to_string()]).unwrap(), ["a", "b"]);
    }

    #[test]
    fn a_choice_with_no_name_in_the_text_names_nobody() {
        let people = [member("a", "Lina"), member("c", "Bob")];
        assert_eq!(resolve_mentions(&people, "всем привет", &["a".to_string()]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn who_left_is_not_named_and_one_account_with_two_devices_is_one() {
        let mut gone = member("a", "Lina");
        gone.status = "left".into();
        let mut second_device = member("b", "Bob");
        second_device.member_pub = "pub-b2".into();
        let people = [gone, member("b", "Bob"), second_device];
        assert!(resolve_mentions(&people, "@Lina", &[]).unwrap().is_empty());
        assert_eq!(resolve_mentions(&people, "@Bob @Bob", &[]).unwrap(), ["b"]);
    }
}
