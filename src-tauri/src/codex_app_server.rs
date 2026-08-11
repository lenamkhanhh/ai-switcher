//! A small, bounded JSON-RPC client for the Codex App Server.
//!
//! Recovery deliberately speaks the structured protocol instead of driving a terminal or a
//! desktop window. The client keeps payloads in memory only for the active request, never logs
//! them, and redacts the small amount of error text that is surfaced to the UI/state file.

use crate::models::{GoalSnapshot, RecoveryErrorKind, RecoveryRuntimeSnapshot};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const MAX_RPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone, Debug)]
pub struct AppServerConfig {
    pub binary: PathBuf,
    pub codex_home: PathBuf,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl AppServerConfig {
    pub fn new(binary: PathBuf, codex_home: PathBuf, cwd: PathBuf) -> Self {
        Self {
            binary,
            codex_home,
            cwd,
            timeout: DEFAULT_RPC_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TurnSnapshot {
    pub id: String,
    pub status: String,
    pub error: Option<Value>,
    pub items: Vec<Value>,
    pub raw: Value,
}

impl TurnSnapshot {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = object.get("id")?.as_str()?.to_string();
        let status = object
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let error = object.get("error").cloned();
        let items = object
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Some(Self {
            id,
            status,
            error,
            items,
            raw: value.clone(),
        })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "interrupted" | "cancelled"
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "inProgress" | "in_progress" | "started"
        )
    }
}

#[derive(Clone, Debug)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub turns: Vec<TurnSnapshot>,
    pub runtime: RecoveryRuntimeSnapshot,
}

impl ThreadSnapshot {
    pub fn from_response(response: Value, expected_thread_id: &str) -> Result<Self> {
        let thread = response.get("thread").unwrap_or(&response);
        let thread_id = thread
            .get("id")
            .or_else(|| response.get("threadId"))
            .and_then(Value::as_str)
            .context("App Server response did not include a thread id")?;
        let thread_id = thread_id.to_string();
        if thread_id != expected_thread_id {
            anyhow::bail!("App Server returned a different thread id");
        }
        let turns = thread
            .get("turns")
            .or_else(|| response.get("turns"))
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(TurnSnapshot::from_value).collect())
            .unwrap_or_default();
        let runtime = runtime_from_values(&response, thread);
        Ok(Self {
            thread_id,
            turns,
            runtime,
        })
    }

    pub fn active_turn(&self) -> Option<&TurnSnapshot> {
        self.turns.iter().rev().find(|turn| turn.is_active())
    }

    pub fn terminal_turn(&self) -> Option<&TurnSnapshot> {
        self.turns.iter().rev().find(|turn| turn.is_terminal())
    }

    pub fn turn(&self, turn_id: &str) -> Option<&TurnSnapshot> {
        self.turns.iter().find(|turn| turn.id == turn_id)
    }

    /// Search structured thread/item content for the exact recovery marker. This is used after a
    /// crash or a lost ACK before any new `turn/start` is attempted.
    pub fn contains_marker(&self, marker: &str) -> bool {
        self.turns
            .iter()
            .any(|turn| json_contains_string(&turn.raw, marker))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub reasoning_efforts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitWindow {
    pub used_percent: Option<f64>,
    pub limit_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitSnapshot {
    pub primary: RateLimitWindow,
    pub secondary: RateLimitWindow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidatePreflight {
    pub eligible: bool,
    pub reason: Option<String>,
    pub rate_limits: RateLimitSnapshot,
    pub model_supported: bool,
    pub effort_supported: bool,
}

#[derive(Clone, Debug)]
pub struct TurnStartedAck {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug)]
pub struct CodexAppServerClient {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
    next_id: u64,
    timeout: Duration,
}

impl CodexAppServerClient {
    pub fn connect(config: &AppServerConfig) -> Result<Self> {
        let mut command = Command::new(&config.binary);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", &config.codex_home)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "Couldn't start Codex App Server from {}",
                config.binary.display()
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("Codex App Server stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex App Server stdout was unavailable")?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.len() <= MAX_RPC_LINE_BYTES => {
                        let message = serde_json::from_str::<Value>(&line)
                            .map_err(|error| format!("invalid App Server JSON: {error}"));
                        if sender.send(message).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {
                        let _ =
                            sender.send(Err("App Server message exceeded the safety bound".into()));
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!("App Server stdout failed: {error}")));
                        break;
                    }
                }
            }
            let _ = sender.send(Err("App Server closed its stdout".into()));
        });
        let mut client = Self {
            child,
            stdin,
            messages: receiver,
            pending: VecDeque::new(),
            next_id: 1,
            timeout: config.timeout,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "ai-account-switcher",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        // Codex 0.141 uses the v1 empty initialized notification. Keep this as a notification,
        // never as a request that could be mistaken for a turn ACK.
        write_json_line(&mut self.stdin, &json!({"method": "initialized"}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({"id": id, "method": method, "params": params});
        write_json_line(&mut self.stdin, &request)?;
        loop {
            let message = self.next_message(self.timeout)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    anyhow::bail!(
                        "App Server request {method} failed: {}",
                        safe_rpc_error(error)
                    );
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.pending.push_back(message);
        }
    }

    fn next_message(&mut self, timeout: Duration) -> Result<Value> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        match self.messages.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(RecvTimeoutError::Timeout) => Err(anyhow!("App Server response timed out")),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("App Server reader disconnected")),
        }
    }

    pub fn read_thread(&mut self, thread_id: &str) -> Result<ThreadSnapshot> {
        let result = self.request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )?;
        ThreadSnapshot::from_response(result, thread_id)
    }

    pub fn resume_thread(
        &mut self,
        thread_id: &str,
        runtime: &RecoveryRuntimeSnapshot,
    ) -> Result<ThreadSnapshot> {
        let mut params = Map::new();
        params.insert("threadId".into(), Value::String(thread_id.into()));
        if !runtime.cwd.is_empty() {
            params.insert("cwd".into(), Value::String(runtime.cwd.clone()));
        }
        insert_optional_string(&mut params, "model", runtime.model.as_deref());
        insert_optional_string(&mut params, "personality", runtime.personality.as_deref());
        insert_optional_json(
            &mut params,
            "approvalPolicy",
            runtime.approval_policy.clone(),
        );
        insert_optional_json(
            &mut params,
            "approvalsReviewer",
            runtime.approvals_reviewer.clone(),
        );
        insert_optional_json(&mut params, "sandbox", runtime.sandbox_policy.clone());
        let result = self.request("thread/resume", Value::Object(params))?;
        let snapshot = ThreadSnapshot::from_response(result, thread_id)?;
        verify_runtime_compatibility(runtime, &snapshot.runtime)?;
        Ok(snapshot)
    }

    pub fn read_goal(&mut self, thread_id: &str) -> Result<Option<GoalSnapshot>> {
        let result = self.request("thread/goal/get", json!({"threadId": thread_id}))?;
        let Some(goal) = result.get("goal") else {
            return Ok(None);
        };
        if goal.is_null() {
            return Ok(None);
        }
        serde_json::from_value(goal.clone())
            .map(Some)
            .context("Codex Goal response was not structured")
    }

    pub fn set_goal(&mut self, thread_id: &str, goal: &GoalSnapshot) -> Result<()> {
        let mut params = Map::new();
        params.insert("threadId".into(), Value::String(thread_id.into()));
        if !goal.objective.is_empty() {
            params.insert("objective".into(), Value::String(goal.objective.clone()));
        }
        if !goal.status.is_empty() {
            params.insert("status".into(), Value::String(goal.status.clone()));
        }
        if let Some(token_budget) = goal.token_budget {
            params.insert("tokenBudget".into(), json!(token_budget));
        }
        self.request("thread/goal/set", Value::Object(params))?;
        Ok(())
    }

    pub fn read_rate_limits(&mut self) -> Result<RateLimitSnapshot> {
        let result = self.request("account/rateLimits/read", json!({}))?;
        Ok(parse_rate_limits(&result))
    }

    pub fn list_models(&mut self) -> Result<Vec<ModelInfo>> {
        let result = self.request("model/list", json!({}))?;
        let values = result
            .get("data")
            .or_else(|| result.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(values
            .iter()
            .filter_map(|value| {
                let object = value.as_object()?;
                let id = object
                    .get("model")
                    .or_else(|| object.get("id"))
                    .and_then(Value::as_str)?
                    .to_string();
                let reasoning_efforts = object
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|effort| {
                        effort
                            .get("reasoningEffort")
                            .or_else(|| effort.get("effort"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect();
                Some(ModelInfo {
                    id,
                    reasoning_efforts,
                })
            })
            .collect())
    }

    /// Verify all account-local and thread-local preconditions before any credential switch.
    pub fn preflight_candidate(
        &mut self,
        model: Option<&str>,
        effort: Option<&str>,
        outgoing_five_hour_used: Option<f64>,
    ) -> Result<CandidatePreflight> {
        let rate_limits = self.read_rate_limits()?;
        let models = self.list_models()?;
        let model_supported =
            model.is_none_or(|requested| models.iter().any(|candidate| candidate.id == requested));
        let effort_supported = match effort {
            None => true,
            Some(requested) => model
                .and_then(|requested_model| {
                    models
                        .iter()
                        .find(|candidate| candidate.id == requested_model)
                })
                .is_some_and(|candidate| {
                    candidate
                        .reasoning_efforts
                        .iter()
                        .any(|candidate_effort| candidate_effort == requested)
                }),
        };
        let primary_used = rate_limits.primary.used_percent;
        let weekly_used = rate_limits.secondary.used_percent;
        let improves_headroom = match (outgoing_five_hour_used, primary_used) {
            (Some(outgoing), Some(candidate)) => candidate < outgoing,
            (Some(_), None) => false,
            _ => true,
        };
        let eligible = primary_used
            .is_some_and(|used| used < crate::seamless::DEFAULT_HANDOFF_THRESHOLD)
            && weekly_used.is_none_or(|used| used < 100.0)
            && model_supported
            && effort_supported
            && improves_headroom;
        let reason = if eligible {
            None
        } else if primary_used.is_none() {
            Some("five-hour quota was unavailable".into())
        } else if primary_used
            .is_some_and(|used| used >= crate::seamless::DEFAULT_HANDOFF_THRESHOLD)
        {
            Some("five-hour quota is at or above the recovery threshold".into())
        } else if weekly_used.is_some_and(|used| used >= 100.0) {
            Some("weekly quota is exhausted".into())
        } else if !model_supported {
            Some("the original model is not available on this account".into())
        } else if !effort_supported {
            Some("the original reasoning effort is not available on this account".into())
        } else {
            Some("candidate does not improve five-hour headroom".into())
        };
        Ok(CandidatePreflight {
            eligible,
            reason,
            rate_limits,
            model_supported,
            effort_supported,
        })
    }

    pub fn start_turn_and_wait_ack(
        &mut self,
        thread_id: &str,
        prompt: &str,
        runtime: &RecoveryRuntimeSnapshot,
        client_message_id: &str,
    ) -> Result<TurnStartedAck> {
        let mut params = Map::new();
        params.insert("threadId".into(), Value::String(thread_id.into()));
        params.insert(
            "input".into(),
            json!([{"type":"text", "text": bounded_text(prompt, 32 * 1024)}]),
        );
        params.insert(
            "clientUserMessageId".into(),
            Value::String(client_message_id.into()),
        );
        insert_optional_string(&mut params, "cwd", non_empty(&runtime.cwd));
        insert_optional_string(&mut params, "model", runtime.model.as_deref());
        insert_optional_string(&mut params, "effort", runtime.reasoning_effort.as_deref());
        insert_optional_json(
            &mut params,
            "approvalPolicy",
            runtime.approval_policy.clone(),
        );
        insert_optional_json(
            &mut params,
            "approvalsReviewer",
            runtime.approvals_reviewer.clone(),
        );
        insert_optional_json(&mut params, "sandboxPolicy", runtime.sandbox_policy.clone());
        let result = self.request("turn/start", Value::Object(params))?;
        let expected_turn_id = result
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .context("turn/start did not return a turn id")?
            .to_string();
        self.wait_for_turn_started(thread_id, &expected_turn_id)
    }

    fn wait_for_turn_started(&mut self, thread_id: &str, turn_id: &str) -> Result<TurnStartedAck> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                anyhow::bail!("turn/started ACK timed out");
            }
            let message = self.next_message(remaining)?;
            if message.get("method").and_then(Value::as_str) != Some("turn/started") {
                continue;
            }
            let params = message.get("params").unwrap_or(&Value::Null);
            let actual_thread = params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let actual_turn = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if actual_thread != thread_id || actual_turn != turn_id {
                anyhow::bail!("turn/started ACK did not match the recovery thread and turn");
            }
            return Ok(TurnStartedAck {
                thread_id: actual_thread.to_string(),
                turn_id: actual_turn.to_string(),
            });
        }
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn classify_codex_error(error: &Value, status: Option<&str>) -> RecoveryErrorKind {
    let info = error.get("codexErrorInfo").or_else(|| {
        error
            .get("error")
            .and_then(|nested| nested.get("codexErrorInfo"))
    });
    if let Some(kind) = info.and_then(structured_error_kind) {
        return kind;
    }
    if matches!(status, Some("interrupted" | "cancelled")) {
        return RecoveryErrorKind::UserCancelled;
    }
    RecoveryErrorKind::Unknown
}

fn structured_error_kind(value: &Value) -> Option<RecoveryErrorKind> {
    let key = value.as_str().map(str::to_string).or_else(|| {
        value
            .as_object()
            .and_then(|object| object.keys().next().cloned())
    })?;
    Some(match key.as_str() {
        "usageLimitExceeded" => RecoveryErrorKind::UsageLimitExceeded,
        "serverOverloaded" => RecoveryErrorKind::ServerOverloaded,
        "responseStreamDisconnected" => RecoveryErrorKind::ResponseStreamDisconnected,
        "responseStreamConnectionFailed" => RecoveryErrorKind::ResponseStreamConnectionFailed,
        "httpConnectionFailed" => RecoveryErrorKind::HttpConnectionFailed,
        "unauthorized" => RecoveryErrorKind::Unauthorized,
        "contextWindowExceeded" => RecoveryErrorKind::ContextWindowExceeded,
        "sandboxError" => RecoveryErrorKind::SandboxError,
        "userCancelled" => RecoveryErrorKind::UserCancelled,
        _ => return None,
    })
}

pub fn classify_terminal_turn(turn: &TurnSnapshot) -> RecoveryErrorKind {
    classify_codex_error(
        turn.error.as_ref().unwrap_or(&Value::Null),
        Some(&turn.status),
    )
}

pub fn terminal_error_fingerprint(kind: RecoveryErrorKind, message: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(redact_text(message.unwrap_or_default()).as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn recovery_error_message(turn: &TurnSnapshot) -> Option<String> {
    turn.error
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(redact_text)
}

pub fn verify_runtime_compatibility(
    expected: &RecoveryRuntimeSnapshot,
    actual: &RecoveryRuntimeSnapshot,
) -> Result<()> {
    if !expected.cwd.is_empty() && expected.cwd != actual.cwd {
        anyhow::bail!("resumed thread changed its working directory");
    }
    if expected.model.is_some() && expected.model != actual.model {
        anyhow::bail!("resumed thread changed its model");
    }
    if expected.reasoning_effort.is_some() && expected.reasoning_effort != actual.reasoning_effort {
        anyhow::bail!("resumed thread changed its reasoning effort");
    }
    if expected.personality.is_some() && expected.personality != actual.personality {
        anyhow::bail!("resumed thread changed its personality");
    }
    if expected.approval_policy.is_some() && expected.approval_policy != actual.approval_policy {
        anyhow::bail!("resumed thread changed its approval policy");
    }
    if expected.approvals_reviewer.is_some()
        && expected.approvals_reviewer != actual.approvals_reviewer
    {
        anyhow::bail!("resumed thread changed its approvals reviewer");
    }
    if expected.sandbox_policy.is_some() && expected.sandbox_policy != actual.sandbox_policy {
        anyhow::bail!("resumed thread changed its sandbox policy");
    }
    Ok(())
}

/// Deterministic protocol fixture helper used by tests and by startup healing logic. It accepts
/// only the exact structured notification; a text line that happens to mention a marker cannot
/// produce an ACK.
#[cfg(test)]
pub fn find_exact_turn_started_ack(
    messages: &[Value],
    expected_thread_id: &str,
    expected_turn_id: &str,
) -> Result<TurnStartedAck> {
    for message in messages {
        if message.get("method").and_then(Value::as_str) != Some("turn/started") {
            continue;
        }
        let params = message.get("params").unwrap_or(&Value::Null);
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let turn_id = params
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if thread_id == expected_thread_id && turn_id == expected_turn_id {
            return Ok(TurnStartedAck {
                thread_id: thread_id.into(),
                turn_id: turn_id.into(),
            });
        }
    }
    anyhow::bail!("turn/started ACK was not found")
}

fn runtime_from_values(response: &Value, thread: &Value) -> RecoveryRuntimeSnapshot {
    let string_field = |name: &str| {
        response
            .get(name)
            .or_else(|| thread.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    RecoveryRuntimeSnapshot {
        cwd: string_field("cwd").unwrap_or_default(),
        model: string_field("model"),
        reasoning_effort: string_field("reasoningEffort").or_else(|| string_field("effort")),
        personality: string_field("personality"),
        approval_policy: response
            .get("approvalPolicy")
            .or_else(|| thread.get("approvalPolicy"))
            .cloned(),
        approvals_reviewer: response
            .get("approvalsReviewer")
            .or_else(|| thread.get("approvalsReviewer"))
            .cloned(),
        sandbox_policy: response
            .get("sandbox")
            .or_else(|| response.get("sandboxPolicy"))
            .or_else(|| thread.get("sandbox"))
            .cloned(),
    }
}

fn parse_rate_limits(value: &Value) -> RateLimitSnapshot {
    let limits = value.get("rateLimits").unwrap_or(value);
    RateLimitSnapshot {
        primary: parse_window(limits.get("primary")),
        secondary: parse_window(limits.get("secondary")),
    }
}

fn parse_window(value: Option<&Value>) -> RateLimitWindow {
    let Some(value) = value else {
        return RateLimitWindow::default();
    };
    RateLimitWindow {
        used_percent: value
            .get("usedPercent")
            .or_else(|| value.get("used_percent"))
            .and_then(number_value),
        limit_id: value
            .get("limitId")
            .or_else(|| value.get("limit_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
}

fn insert_optional_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        map.insert(key.into(), Value::String(value.into()));
    }
}

fn insert_optional_json(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        if !value.is_null() {
            map.insert(key.into(), value);
        }
    }
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn write_json_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let line = serde_json::to_string(value)?;
    if line.len() > MAX_RPC_LINE_BYTES {
        anyhow::bail!("App Server request exceeded the safety bound");
    }
    stdin.write_all(line.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn safe_rpc_error(value: &Value) -> String {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown App Server error");
    redact_text(message)
}

pub fn redact_text(text: &str) -> String {
    let lower = text.to_lowercase();
    if [
        "authorization:",
        "bearer ",
        "access_token",
        "refresh_token",
        "id_token",
        "api_key",
        "apikey",
        "client_secret",
        "password",
        "cookie:",
        "set-cookie",
        "openai_api_key",
        "sk-proj-",
        "sk-",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "[redacted]".into();
    }
    bounded_text(text, 600)
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            output.push('…');
            break;
        }
        output.push(ch);
    }
    output
}

fn json_contains_string(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text == needle,
        Value::Array(values) => values.iter().any(|item| json_contains_string(item, needle)),
        Value::Object(values) => values
            .values()
            .any(|item| json_contains_string(item, needle)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_usage_limit_is_the_only_quota_switch_signal() {
        let error = json!({
            "codexErrorInfo": "usageLimitExceeded",
            "message": "usage limit reached"
        });
        assert_eq!(
            classify_codex_error(&error, Some("failed")),
            RecoveryErrorKind::UsageLimitExceeded
        );
        assert_eq!(
            classify_codex_error(
                &json!({"message": "rate limit exceeded in the user prompt"}),
                Some("failed")
            ),
            RecoveryErrorKind::Unknown
        );
    }

    #[test]
    fn structured_capacity_and_cancel_are_not_account_quota() {
        assert_eq!(
            classify_codex_error(
                &json!({"codexErrorInfo": "serverOverloaded"}),
                Some("failed")
            ),
            RecoveryErrorKind::ServerOverloaded
        );
        assert_eq!(
            classify_codex_error(&json!({}), Some("interrupted")),
            RecoveryErrorKind::UserCancelled
        );
    }

    #[test]
    fn fixture_requires_exact_thread_and_turn_started_ack() {
        let messages = vec![
            json!({"method":"turn/started","params":{"threadId":"other","turn":{"id":"turn-1"}}}),
            json!({"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}),
        ];
        let ack = find_exact_turn_started_ack(&messages, "thread-1", "turn-1").unwrap();
        assert_eq!(ack.thread_id, "thread-1");
        assert_eq!(ack.turn_id, "turn-1");
        assert!(find_exact_turn_started_ack(&messages, "thread-1", "turn-2").is_err());
    }

    #[test]
    fn thread_fixture_rejects_missing_thread_identity() {
        assert!(
            ThreadSnapshot::from_response(json!({"turns": []}), "thread-1").is_err(),
            "a missing protocol thread id must not be replaced with a local hint"
        );
    }

    #[test]
    fn resume_runtime_verification_rejects_changed_security_settings() {
        let expected = RecoveryRuntimeSnapshot {
            cwd: "D:\\repo".into(),
            model: Some("gpt-5".into()),
            reasoning_effort: Some("high".into()),
            personality: None,
            approval_policy: Some(json!("on-request")),
            approvals_reviewer: None,
            sandbox_policy: Some(json!("workspace-write")),
        };
        let mut actual = expected.clone();
        actual.sandbox_policy = Some(json!("danger-full-access"));
        assert!(verify_runtime_compatibility(&expected, &actual).is_err());
    }

    #[test]
    fn fingerprint_is_stable_and_redacts_credentials() {
        let first = terminal_error_fingerprint(
            RecoveryErrorKind::UsageLimitExceeded,
            Some("Authorization: Bearer secret"),
        );
        let second = terminal_error_fingerprint(
            RecoveryErrorKind::UsageLimitExceeded,
            Some("Authorization: Bearer different"),
        );
        assert_eq!(first, second);
        assert!(!redact_text("sk-proj-secret").contains("secret"));
    }

    #[test]
    fn app_server_rate_limit_fixture_preserves_primary_and_weekly_windows() {
        let limits = parse_rate_limits(&json!({
            "rateLimits": {
                "primary": {"usedPercent": 12.5, "limitId": "five-hour"},
                "secondary": {"usedPercent": "41", "limitId": "weekly"}
            }
        }));
        assert_eq!(limits.primary.used_percent, Some(12.5));
        assert_eq!(limits.primary.limit_id.as_deref(), Some("five-hour"));
        assert_eq!(limits.secondary.used_percent, Some(41.0));
        assert_eq!(limits.secondary.limit_id.as_deref(), Some("weekly"));
    }

    #[test]
    fn thread_fixture_keeps_completed_and_uncertain_items_separate() {
        let thread = ThreadSnapshot::from_response(
            json!({
                "thread": {
                    "id": "thread-1",
                    "cwd": "D:\\repo",
                    "model": "gpt-5",
                    "reasoningEffort": "high",
                    "turns": [{
                        "id": "turn-1",
                        "status": "failed",
                        "items": [
                            {"id":"item-done","type":"commandExecution","status":"completed"},
                            {"id":"item-uncertain","type":"fileChange","status":"started"}
                        ]
                    }]
                }
            }),
            "thread-1",
        )
        .unwrap();
        assert_eq!(thread.thread_id, "thread-1");
        assert_eq!(thread.turns[0].items.len(), 2);
        assert_eq!(thread.runtime.model.as_deref(), Some("gpt-5"));
        assert_eq!(thread.runtime.reasoning_effort.as_deref(), Some("high"));
    }
}
