use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::BufRead;
use std::path::{Path, PathBuf};

const MAX_RECENT_MESSAGES: usize = 12;
const MAX_MESSAGE_CHARS: usize = 1800;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffPrompt {
    pub path: String,
    pub prompt: String,
}

#[derive(Clone, Debug)]
struct HandoffContext {
    session_id: Option<String>,
    cwd: Option<String>,
    messages: Vec<ChatMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutCheckpoint {
    pub session_id: String,
    pub cwd: String,
    pub idle: bool,
    pub task_goal: Option<String>,
    pub recent_messages: Vec<String>,
    pub touched_files: Vec<String>,
    pub previous_account: String,
    pub current_account: Option<String>,
    pub work_completed: Vec<String>,
    pub work_in_progress: Option<String>,
    pub last_verification: Option<String>,
    pub next_intended_action: Option<String>,
    pub created_at: String,
}

pub fn latest_codex_checkpoint(
    codex_config_dir: &Path,
    handoff_dir: &Path,
    previous_account: &str,
) -> Result<Option<(RolloutCheckpoint, PathBuf)>> {
    let Some(file) = latest_rollout_file(&codex_config_dir.join("sessions")) else {
        return Ok(None);
    };
    checkpoint_from_rollout(&file, handoff_dir, previous_account)
}

pub fn checkpoint_from_rollout(
    file: &Path,
    handoff_dir: &Path,
    previous_account: &str,
) -> Result<Option<(RolloutCheckpoint, PathBuf)>> {
    let lines = read_lines(file)?;
    let context = parse_handoff_context(&lines);
    let (Some(session_id), Some(cwd)) = (context.session_id, context.cwd) else {
        return Ok(None);
    };
    let idle = rollout_is_idle(&lines);
    let recent_messages: Vec<String> = context
        .messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.text))
        .collect();
    let task_goal = context
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "User")
        .map(|m| m.text.clone());
    let touched_files = touched_files(&lines);
    let assistant_messages: Vec<String> = context
        .messages
        .iter()
        .filter(|m| m.role == "Assistant")
        .map(|m| m.text.clone())
        .collect();
    let checkpoint = RolloutCheckpoint {
        session_id,
        cwd,
        idle,
        task_goal,
        recent_messages,
        touched_files,
        previous_account: previous_account.to_string(),
        current_account: None,
        work_completed: assistant_messages.iter().rev().take(3).cloned().collect(),
        work_in_progress: assistant_messages.last().cloned(),
        last_verification: assistant_messages
            .iter()
            .rev()
            .find(|message| {
                let lower = message.to_ascii_lowercase();
                lower.contains("test") || lower.contains("build") || lower.contains("verified")
            })
            .cloned(),
        next_intended_action: assistant_messages.last().cloned(),
        created_at: last_event_timestamp(&lines).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
    };
    std::fs::create_dir_all(handoff_dir)?;
    let path = handoff_dir.join(format!(
        "checkpoint-{}-{}.json",
        checkpoint.session_id,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&checkpoint)?)?;
    Ok(Some((checkpoint, path)))
}

fn last_event_timestamp(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|value| {
            value
                .get("timestamp")
                .or_else(|| value.get("created_at"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub fn continuation_prompt(checkpoint_path: &Path) -> String {
    format!("Continue the interrupted task using the existing session context and checkpoint. Do not scan the full repository. Inspect only files needed for the next incomplete step. Verify existing edits before changing more. Checkpoint: {}", checkpoint_path.display())
}

fn rollout_is_idle(lines: &[String]) -> bool {
    let mut busy = false;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let kind = value
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(kind, "task_started" | "turn_started") {
            busy = true;
        }
        if matches!(kind, "task_complete" | "turn_complete" | "turn_aborted") {
            busy = false;
        }
    }
    !busy
}

fn touched_files(lines: &[String]) -> Vec<String> {
    let mut files = Vec::new();
    for line in lines.iter().rev().take(200) {
        for token in
            line.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '(' | ')'))
        {
            let clean = token.trim_matches(|c: char| matches!(c, ':' | ';' | '`'));
            if clean.len() < 260
                && (clean.contains('/') || clean.contains('\\'))
                && clean.rsplit('.').next().is_some_and(|ext| {
                    matches!(
                        ext,
                        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "toml" | "json" | "md" | "css"
                    )
                })
                && !files.iter().any(|f| f == clean)
            {
                files.push(clean.to_string());
                if files.len() == 20 {
                    return files;
                }
            }
        }
    }
    files
}

#[derive(Clone, Debug)]
struct ChatMessage {
    role: &'static str,
    text: String,
}

pub fn create_latest_codex_handoff(
    codex_config_dir: &Path,
    handoff_dir: &Path,
    from_account: &str,
    to_account: &str,
) -> Result<Option<HandoffPrompt>> {
    let Some(file) = latest_rollout_file(&codex_config_dir.join("sessions")) else {
        return Ok(None);
    };
    let lines = read_lines(&file).with_context(|| format!("couldn't read {}", file.display()))?;
    let Some(prompt) = build_handoff_prompt_from_lines(&lines, from_account, to_account) else {
        return Ok(None);
    };

    std::fs::create_dir_all(handoff_dir)?;
    let filename = format!(
        "codex-handoff-{}.md",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let path = handoff_dir.join(filename);
    std::fs::write(&path, &prompt)?;
    Ok(Some(HandoffPrompt {
        path: path.to_string_lossy().to_string(),
        prompt,
    }))
}

fn latest_rollout_file(sessions_dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![sessions_dir.to_path_buf()];
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_rollout = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
            if !is_rollout {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(current, _)| modified > *current) {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Rollout files routinely reach hundreds of megabytes, and this runs on the 10s watcher thread.
/// Only the head (`session_meta` with the session id) and the tail (the recent turns, the latest
/// `turn_context`, and the task-started/complete pairs that decide whether the session is idle)
/// carry anything a handoff needs, so the middle is never loaded or parsed.
const HEAD_SCAN_BYTES: u64 = 256 * 1024;
const TAIL_SCAN_BYTES: u64 = 8 * 1024 * 1024;

fn read_lines(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= HEAD_SCAN_BYTES + TAIL_SCAN_BYTES {
        let reader = std::io::BufReader::new(file);
        return Ok(reader.lines().map_while(Result::ok).collect());
    }

    let mut lines = read_byte_range_lines(&file, 0, HEAD_SCAN_BYTES)?;
    // The head ends mid-line, and the tail starts mid-line; drop both fragments.
    lines.pop();
    let mut tail = read_byte_range_lines(&file, len - TAIL_SCAN_BYTES, TAIL_SCAN_BYTES)?;
    if !tail.is_empty() {
        tail.remove(0);
    }
    lines.extend(tail);
    Ok(lines)
}

fn read_byte_range_lines(file: &std::fs::File, offset: u64, len: u64) -> Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::new();
    handle.take(len).read_to_end(&mut buffer)?;
    // Lossy: a byte range cut at an arbitrary offset can split a multi-byte character.
    Ok(String::from_utf8_lossy(&buffer)
        .lines()
        .map(ToString::to_string)
        .collect())
}

fn build_handoff_prompt_from_lines(
    lines: &[String],
    from_account: &str,
    to_account: &str,
) -> Option<String> {
    let context = parse_handoff_context(lines);
    if context.messages.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("Continue the interrupted Codex task after an account auto-switch.\n\n");
    out.push_str("Context:\n");
    out.push_str(&format!("- Previous account: {from_account}\n"));
    out.push_str(&format!("- Current account: {to_account}\n"));
    if let Some(session_id) = context.session_id {
        out.push_str(&format!("- Previous session: {session_id}\n"));
    }
    if let Some(cwd) = context.cwd {
        out.push_str(&format!("- Workspace: {cwd}\n"));
    }
    out.push_str("\nInstructions:\n");
    out.push_str("- Inspect the current workspace before editing.\n");
    out.push_str("- Continue from the latest useful state below.\n");
    out.push_str(
        "- If the previous task was mid-edit, verify files/tests before changing more.\n\n",
    );
    out.push_str("Recent conversation:\n");
    for message in context.messages {
        if message.text.lines().count() == 1 {
            out.push_str(&format!("\n{}: {}\n", message.role, message.text));
        } else {
            out.push_str(&format!("\n{}:\n{}\n", message.role, message.text));
        }
    }
    Some(out)
}

fn parse_handoff_context(lines: &[String]) -> HandoffContext {
    let mut session_id = None;
    let mut cwd = None;
    let mut messages: VecDeque<ChatMessage> = VecDeque::new();

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                session_id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(session_id);
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(cwd);
            }
            Some("turn_context") => {
                cwd = value
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(cwd);
            }
            Some("event_msg") => {
                if let Some(message) = event_message(&value) {
                    push_limited(&mut messages, message);
                }
            }
            Some("response_item") => {
                if let Some(message) = response_item_message(&value) {
                    push_limited(&mut messages, message);
                }
            }
            _ => {}
        }
    }

    HandoffContext {
        session_id,
        cwd,
        messages: messages.into_iter().collect(),
    }
}

fn event_message(value: &Value) -> Option<ChatMessage> {
    let payload = value.get("payload")?;
    match payload.get("type").and_then(Value::as_str)? {
        "user_message" => Some(ChatMessage {
            role: "User",
            text: sanitize_message(payload.get("message")?.as_str()?),
        }),
        "agent_message" => Some(ChatMessage {
            role: "Assistant",
            text: sanitize_message(payload.get("message")?.as_str()?),
        }),
        _ => None,
    }
}

fn response_item_message(value: &Value) -> Option<ChatMessage> {
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    let role = match payload.get("role").and_then(Value::as_str)? {
        "user" => "User",
        "assistant" => "Assistant",
        _ => return None,
    };
    let text = content_text(payload.get("content")?)?;
    Some(ChatMessage {
        role,
        text: sanitize_message(&text),
    })
}

fn content_text(content: &Value) -> Option<String> {
    let array = content.as_array()?;
    let mut parts = Vec::new();
    for item in array {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text.to_string());
        } else if let Some(text) = item.get("type").and_then(Value::as_str).and_then(|kind| {
            if matches!(kind, "input_text" | "output_text") {
                item.get("text")?.as_str()
            } else {
                None
            }
        }) {
            parts.push(text.to_string());
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn push_limited(messages: &mut VecDeque<ChatMessage>, message: ChatMessage) {
    if message.text.trim().is_empty() {
        return;
    }
    messages.push_back(message);
    while messages.len() > MAX_RECENT_MESSAGES {
        messages.pop_front();
    }
}

fn sanitize_message(text: &str) -> String {
    let text = text
        .lines()
        .map(redact_secret_line)
        .collect::<Vec<_>>()
        .join("\n");
    truncate_chars(&text, MAX_MESSAGE_CHARS)
}

fn redact_secret_line(line: &str) -> String {
    let lower = line.to_lowercase();
    if lower.contains("authorization:")
        || lower.contains("bearer ")
        || lower.contains("access_token")
        || lower.contains("refresh_token")
        || lower.contains("id_token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("client_secret")
        || lower.contains("password")
        || lower.contains("cookie:")
        || lower.contains("set-cookie")
        || lower.contains("openai_api_key")
        || lower.contains("sk-proj-")
        || lower.contains("sk-")
    {
        "[redacted]".to_string()
    } else {
        line.to_string()
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("\n...[truncated]");
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_handoff_prompt_from_recent_codex_messages() {
        let lines = vec![
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"D:\\Work\\repo"}}"#.to_string(),
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix bug A"}}"#.to_string(),
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"tool output must be skipped"}}"#.to_string(),
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I changed file X."}}"#.to_string(),
        ];
        let prompt = build_handoff_prompt_from_lines(&lines, "acc-1", "acc-2").unwrap();

        assert!(prompt.contains("Continue the interrupted Codex task"));
        assert!(prompt.contains("session-1"));
        assert!(prompt.contains("D:\\Work\\repo"));
        assert!(prompt.contains("User: fix bug A"));
        assert!(prompt.contains("Assistant: I changed file X."));
        assert!(!prompt.contains("tool output"));
    }

    #[test]
    fn redacts_secret_like_text_from_handoff_prompt() {
        let lines = vec![
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Authorization: Bearer secret-token\naccess_token=abc\nsk-proj-secret"}}"#.to_string(),
        ];
        let prompt = build_handoff_prompt_from_lines(&lines, "old", "new").unwrap();

        assert!(!prompt.contains("secret-token"));
        assert!(!prompt.contains("access_token=abc"));
        assert!(!prompt.contains("sk-proj-secret"));
        assert!(prompt.contains("[redacted]"));
    }
}
