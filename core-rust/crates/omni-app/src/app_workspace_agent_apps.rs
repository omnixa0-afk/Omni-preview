//! An agent connected from its own application, not from a command line: Omni writes itself into
//! that application's MCP settings as one server entry that runs this Omni as the agent's `omni-mcp`
//! (docs/decisions/omni-workspace-git.md section 5). Claude Code keeps its servers in `~/.claude.json`
//! (`mcpServers`), Codex in `~/.codex/config.toml` (`[mcp_servers.*]`, or under `CODEX_HOME`). Only
//! Omni's own entry changes; the file as it was is kept beside it once, as `*.before-omni`.

use std::path::{Path, PathBuf};

use omni_types::{OmniError, OmniResult};

/// The applications Omni writes itself into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApp {
    Claude,
    Codex,
}

impl AgentApp {
    pub fn parse(name: &str) -> OmniResult<Self> {
        match name {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => Err(OmniError::InvalidInput(format!("unknown agent application: {other}"))),
        }
    }
}

fn home() -> OmniResult<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| OmniError::InvalidInput("no home folder to find the agent's application settings in".into()))
}

/// Where the application reads its MCP servers from.
pub fn settings_path(app: AgentApp) -> OmniResult<PathBuf> {
    Ok(match app {
        AgentApp::Claude => home()?.join(".claude.json"),
        AgentApp::Codex => match std::env::var_os("CODEX_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("config.toml"),
            _ => home()?.join(".codex").join("config.toml"),
        },
    })
}

/// The server's name in the settings: one per agent, plain ASCII (a TOML bare key), e.g. `omni-coder`;
/// a name with other letters keeps a short hash of itself so two such agents do not share one entry.
pub fn server_key(agent: &str) -> String {
    let mut plain = String::new();
    for c in agent.trim().chars() {
        if c.is_ascii_alphanumeric() {
            plain.push(c.to_ascii_lowercase());
        } else if !plain.ends_with('-') && !plain.is_empty() {
            plain.push('-');
        }
    }
    let plain = plain.trim_end_matches('-').to_string();
    if !plain.is_empty() && agent.trim().is_ascii() {
        return format!("omni-{plain}");
    }
    let hash = agent.trim().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let plain = if plain.is_empty() { "agent".to_string() } else { plain };
    format!("omni-{plain}-{:06x}", hash & 0xff_ffff)
}

/// Write the agent's server into `app`'s settings (replacing an earlier entry of the same agent).
/// The file's path.
pub fn add_server(app: AgentApp, agent: &str, exe: &str, args: &[String]) -> OmniResult<PathBuf> {
    let path = settings_path(app)?;
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(OmniError::InvalidInput(format!("{}: {e}", path.display()))),
    };
    let key = server_key(agent);
    let after = match app {
        AgentApp::Claude => with_claude_server(before.as_deref().unwrap_or("{}"), &key, exe, args)?,
        AgentApp::Codex => with_codex_server(before.as_deref().unwrap_or(""), &key, exe, args)?,
    };
    write_settings(&path, before.as_deref(), &after)?;
    Ok(path)
}

/// The settings file, when `app` already starts the agent's server with this `exe`: the dialog shows
/// the step as done. Another exe there (an older copy of Omni) is not ours to rely on.
pub fn has_server(app: AgentApp, agent: &str, exe: &str) -> Option<PathBuf> {
    let path = settings_path(app).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let key = server_key(agent);
    let command = match app {
        AgentApp::Claude => serde_json::from_str::<serde_json::Value>(&text).ok()?.get("mcpServers")?.get(&key)?.get("command")?.as_str()?.to_string(),
        AgentApp::Codex => text.parse::<toml::Table>().ok()?.get("mcp_servers")?.get(&key)?.get("command")?.as_str()?.to_string(),
    };
    command.eq_ignore_ascii_case(exe).then_some(path)
}

fn write_settings(path: &Path, before: Option<&str>, after: &str) -> OmniResult<()> {
    let io = |e: std::io::Error| OmniError::InvalidInput(format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    if let Some(before) = before {
        let kept = path.with_extension(format!("{}.before-omni", path.extension().and_then(|e| e.to_str()).unwrap_or("settings")));
        if !kept.exists() {
            std::fs::write(&kept, before).map_err(io)?;
        }
    }
    // Whole or not at all: the application never reads a half-written file.
    let fresh = path.with_extension("omni-tmp");
    std::fs::write(&fresh, after).map_err(io)?;
    std::fs::rename(&fresh, path).map_err(io)
}

/// `~/.claude.json` with our server in its top-level `mcpServers`. The rest of the file (the
/// application's own state) stays byte for byte as it was.
fn with_claude_server(text: &str, key: &str, exe: &str, args: &[String]) -> OmniResult<String> {
    let bad = |what: &str| OmniError::InvalidInput(format!("the Claude settings file is not JSON Omni can edit: {what}"));
    let server = serde_json::json!({ "type": "stdio", "command": exe, "args": args });
    let out = match top_member(text, "mcpServers").map_err(|e| bad(&e))? {
        Some((start, end)) => {
            let mut servers: serde_json::Value = serde_json::from_str(&text[start..end]).map_err(|e| bad(&e.to_string()))?;
            let map = servers.as_object_mut().ok_or_else(|| bad("mcpServers is not an object"))?;
            map.insert(key.to_string(), server);
            let pretty = serde_json::to_string_pretty(&servers).map_err(|e| bad(&e.to_string()))?.replace('\n', "\n  ");
            format!("{}{pretty}{}", &text[..start], &text[end..])
        }
        None => {
            let open = text.find('{').ok_or_else(|| bad("no object"))?;
            let empty = text[open + 1..].trim_start().starts_with('}');
            let pretty = serde_json::to_string_pretty(&serde_json::json!({ key: server })).map_err(|e| bad(&e.to_string()))?.replace('\n', "\n  ");
            let comma = if empty { "" } else { "," };
            format!("{}\n  \"mcpServers\": {pretty}{comma}{}", &text[..=open], &text[open + 1..])
        }
    };
    serde_json::from_str::<serde_json::Value>(&out).map_err(|e| bad(&e.to_string()))?;
    Ok(out)
}

/// The span of a top-level member's value in a JSON object's text.
fn top_member(text: &str, key: &str) -> Result<Option<(usize, usize)>, String> {
    let b = text.as_bytes();
    let ws = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let string_end = |i: usize| -> Result<usize, String> {
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return Ok(j + 1),
                _ => j += 1,
            }
        }
        Err("an unclosed string".into())
    };
    let value_end = |i: usize| -> Result<usize, String> {
        match b.get(i) {
            Some(b'"') => string_end(i),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0usize;
                let mut j = i;
                while j < b.len() {
                    match b[j] {
                        b'"' => {
                            j = string_end(j)?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(j + 1);
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                Err("an unclosed object".into())
            }
            Some(_) => {
                let mut j = i;
                while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace() {
                    j += 1;
                }
                Ok(j)
            }
            None => Err("a member without a value".into()),
        }
    };
    let mut i = ws(0);
    if b.get(i) != Some(&b'{') {
        return Err("not an object".into());
    }
    i += 1;
    loop {
        i = ws(i);
        match b.get(i) {
            Some(b'}') => return Ok(None),
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'"') => {}
            _ => return Err("a broken member".into()),
        }
        let name_end = string_end(i)?;
        let name: String = serde_json::from_str(&text[i..name_end]).map_err(|e| e.to_string())?;
        i = ws(name_end);
        if b.get(i) != Some(&b':') {
            return Err("a member without a colon".into());
        }
        i = ws(i + 1);
        let end = value_end(i)?;
        if name == key {
            return Ok(Some((i, end)));
        }
        i = end;
    }
}

/// `config.toml` with our `[mcp_servers.<key>]` table in place of an earlier one. Without the last
/// two lines Codex refuses every Omni tool when nobody is there to approve it, and a long omni_wait
/// outlives its default tool timeout; what reaches `main` is still the owner's «Применить» in Omni.
fn with_codex_server(text: &str, key: &str, exe: &str, args: &[String]) -> OmniResult<String> {
    let bad = |what: String| OmniError::InvalidInput(format!("the Codex settings file is not TOML Omni can edit: {what}"));
    // A TOML basic string escapes as JSON does.
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let header = format!("[mcp_servers.{key}]");
    let nested = format!("[mcp_servers.{key}.");
    let mut kept: Vec<&str> = Vec::new();
    let mut ours = false;
    for line in text.lines() {
        let head = line.trim_start();
        if head.starts_with('[') {
            ours = head.trim_end() == header || head.starts_with(&nested);
        }
        if !ours {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n").trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "{header}\ncommand = {}\nargs = [{}]\ndefault_tools_approval_mode = \"approve\"\ntool_timeout_sec = 660\n",
        quoted(exe),
        args.iter().map(|a| quoted(a)).collect::<Vec<_>>().join(", ")
    ));
    out.parse::<toml::Table>().map_err(|e| bad(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        ["--run-helper", "mcp", "--dir", r"D:\Omni Agents\Coder", "--name", "Coder", "--owner", "acct-1"].map(String::from).to_vec()
    }

    #[test]
    fn an_agents_server_goes_into_claudes_settings_and_leaves_the_rest_as_it_was() {
        let state = "{\n  \"numStartups\": 7,\n  \"projects\": {\"D:/x\": {\"a\": [1, \"}\"]}},\n  \"zeta\": true\n}\n";
        let once = with_claude_server(state, "omni-coder", r"C:\Omni\Omni.exe", &args()).unwrap();
        assert!(once.contains("\"projects\": {\"D:/x\": {\"a\": [1, \"}\"]}},\n  \"zeta\": true"), "the application's state is untouched: {once}");
        let v: serde_json::Value = serde_json::from_str(&once).unwrap();
        assert_eq!(v["mcpServers"]["omni-coder"]["command"], r"C:\Omni\Omni.exe");
        assert_eq!(v["mcpServers"]["omni-coder"]["args"][3], r"D:\Omni Agents\Coder");
        // Again: the same entry is replaced, another server stays.
        let other = once.replace("\"omni-coder\"", "\"other\"");
        let twice = with_claude_server(&other, "omni-coder", r"C:\Omni2\Omni.exe", &args()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&twice).unwrap();
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 2);
        assert_eq!(v["mcpServers"]["omni-coder"]["command"], r"C:\Omni2\Omni.exe");
        // A fresh file.
        let fresh: serde_json::Value = serde_json::from_str(&with_claude_server("{}", "omni-coder", "Omni.exe", &args()).unwrap()).unwrap();
        assert!(fresh["mcpServers"]["omni-coder"].is_object());
        assert!(with_claude_server("[1]", "omni-coder", "Omni.exe", &args()).is_err());
    }

    #[test]
    fn an_agents_server_goes_into_codexs_settings_once_and_leaves_the_rest_as_it_was() {
        let config = "model = \"gpt-6-astra\"\n\n[mcp_servers.omni-coder]\ncommand = \"old.exe\"\n\n[mcp_servers.omni-coder.env]\nX = \"1\"\n\n[desktop]\nfollowUpQueueMode = \"steer\"\n";
        let out = with_codex_server(config, "omni-coder", r"C:\Omni\Omni.exe", &args()).unwrap();
        assert!(out.starts_with("model = \"gpt-6-astra\"") && out.contains("[desktop]\nfollowUpQueueMode = \"steer\""), "{out}");
        assert!(!out.contains("old.exe") && !out.contains("X = \"1\""), "the earlier entry is gone: {out}");
        let table: toml::Table = out.parse().unwrap();
        let ours = &table["mcp_servers"]["omni-coder"];
        assert_eq!(ours["command"].as_str(), Some(r"C:\Omni\Omni.exe"));
        assert_eq!(ours["tool_timeout_sec"].as_integer(), Some(660));
        assert!(with_codex_server("", "omni-coder", "Omni.exe", &args()).unwrap().parse::<toml::Table>().is_ok());
    }

    #[test]
    fn an_agents_server_key_is_plain_and_its_own() {
        assert_eq!(server_key("Codex"), "omni-codex");
        assert_eq!(server_key("Claude Code"), "omni-claude-code");
        assert!(server_key("Кодер").starts_with("omni-agent-"));
        assert_ne!(server_key("Кодер"), server_key("Лина"));
    }
}
