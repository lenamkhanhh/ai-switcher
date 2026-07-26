use crate::models::{Account, AccountState, ResumeJob, ResumeJobStatus, ToolId};
use std::path::Path;

pub const DEFAULT_HANDOFF_THRESHOLD: f64 = 92.0;
pub const CANDIDATE_LIMIT: f64 = 95.0;

pub fn should_checkpoint(percent_used: Option<f64>, session_idle: bool) -> bool {
    session_idle && percent_used.is_some_and(|value| value >= DEFAULT_HANDOFF_THRESHOLD)
}

pub fn should_start_handoff(
    percent_used: Option<f64>,
    explicit_quota_error: bool,
    threshold: f64,
) -> bool {
    explicit_quota_error || percent_used.is_some_and(|value| value >= threshold)
}

pub fn resume_args<'a>(session_id: &'a str, prompt: &'a str) -> [&'a str; 3] {
    ["resume", session_id, prompt]
}

/// Pick the Codex account to hand the session over to.
///
/// `must_improve_on` is the current account's 5-hour usage, and is set whenever the switch was
/// decided from quota numbers rather than a hard failure. Candidates are allowed up to
/// `CANDIDATE_LIMIT`, which sits above the default switch threshold, so without this the app can
/// hand off to an account that is itself already over the threshold and then hand off again on
/// the next poll — ping-ponging between two nearly-exhausted accounts, spawning a resume console
/// each time. When the switch is forced by a real quota error the reported numbers can't be
/// trusted, so no improvement is required.
pub fn select_candidate<'a>(
    accounts: &'a [Account],
    current_id: &str,
    must_improve_on: Option<f64>,
) -> Option<&'a Account> {
    let mut eligible: Vec<&Account> = accounts
        .iter()
        .filter(|account| {
            account.tool_id == ToolId::Codex
                && account.id != current_id
                && account.api_provider.is_none()
                && account.state != AccountState::NeedsLogin
                && account.quota.as_ref().is_some_and(|quota| {
                    quota.error.is_none()
                        // OpenAI may temporarily omit the 5-hour window. In that state the
                        // weekly window plus a healthy auth response is the only trustworthy
                        // signal, so keep the account eligible and rank unknown 5h last.
                        && quota.five_hour.percent_used.is_none_or(|v| v < CANDIDATE_LIMIT)
                        && quota.weekly.percent_used.is_some_and(|v| v < 100.0)
                        && must_improve_on.is_none_or(|current| {
                            quota.five_hour.percent_used.is_none_or(|v| v < current)
                        })
                })
        })
        .collect();
    eligible.sort_by(|left, right| {
        let lq = left.quota.as_ref().expect("filtered");
        let rq = right.quota.as_ref().expect("filtered");
        lq.five_hour
            .percent_used
            .unwrap_or(CANDIDATE_LIMIT)
            .total_cmp(&rq.five_hour.percent_used.unwrap_or(CANDIDATE_LIMIT))
            .then_with(|| {
                lq.weekly
                    .percent_used
                    .unwrap_or(100.0)
                    .total_cmp(&rq.weekly.percent_used.unwrap_or(100.0))
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    eligible.into_iter().next()
}

pub fn can_transition(from: &ResumeJobStatus, to: &ResumeJobStatus) -> bool {
    use ResumeJobStatus::*;
    matches!(
        (from, to),
        (Queued, Checkpointed)
            | (Queued, Cancelled)
            | (Queued, Failed)
            | (Checkpointed, Switching)
            | (Checkpointed, Cancelled)
            | (Checkpointed, Failed)
            | (Switching, Resumed)
            | (Switching, Failed)
            | (Failed, Queued)
            | (Failed, Cancelled)
    )
}

pub fn is_duplicate(jobs: &[ResumeJob], session_id: &str, event_key: &str) -> bool {
    jobs.iter().any(|job| {
        job.session_id == session_id
            && job.event_key == event_key
            && !matches!(
                job.status,
                ResumeJobStatus::Cancelled | ResumeJobStatus::Failed
            )
    })
}

pub fn spawn_exact_resume(
    binary: &Path,
    code_home: &Path,
    cwd: &Path,
    session_id: &str,
    prompt: &str,
) -> Result<u32, String> {
    let mut command = std::process::Command::new(binary);
    command
        .args(resume_args(session_id, prompt))
        .env("CODEX_HOME", code_home)
        .current_dir(cwd)
        // Keep the TUI attached to a real console on Windows. This is required for the
        // follow-up `/app` command; a detached process can only prove that it started.
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000010); // CREATE_NEW_CONSOLE
    }
    #[cfg(not(windows))]
    crate::tools::hide_command_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("resume launch failed: {error}"))?;
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                return Err(format!(
                    "resume exited immediately with {}",
                    status.code().unwrap_or(-1)
                ))
            }
            Ok(Some(_)) => return Ok(pid),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(error) => return Err(format!("resume health check failed: {error}")),
        }
    }
    crate::tools::open_codex_session_in_app_after_resume(pid);
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_jobs_cannot_run_again() {
        assert!(!can_transition(
            &ResumeJobStatus::Resumed,
            &ResumeJobStatus::Queued
        ));
    }

    #[test]
    fn failed_jobs_can_be_retried() {
        assert!(can_transition(
            &ResumeJobStatus::Failed,
            &ResumeJobStatus::Queued
        ));
    }

    #[test]
    fn threshold_requires_92_percent_and_idle() {
        assert!(!should_checkpoint(Some(91.0), true));
        assert!(should_checkpoint(Some(92.0), true));
        assert!(!should_checkpoint(Some(92.0), false));
    }

    #[test]
    fn unknown_window_only_handoffs_after_an_explicit_quota_error() {
        assert!(!should_start_handoff(None, false, 92.0));
        assert!(should_start_handoff(None, true, 92.0));
        assert!(should_start_handoff(Some(92.0), false, 92.0));
    }

    #[test]
    fn resume_is_exact_and_never_uses_last() {
        assert_eq!(
            resume_args("session-uuid", "checkpoint"),
            ["resume", "session-uuid", "checkpoint"]
        );
        assert!(!resume_args("session-uuid", "checkpoint").contains(&"--last"));
    }

    fn codex_account(id: &str, five_hour: f64) -> Account {
        Account {
            id: id.into(),
            name: id.into(),
            tool_id: ToolId::Codex,
            state: AccountState::Idle,
            fingerprint: id.into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_used_at: None,
            launcher_command: None,
            launcher_paths: None,
            is_default: false,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            quota_status: None,
            codex_login_method: crate::models::CodexLoginMethod::DeviceCode,
            quota: Some(crate::models::QuotaInfo {
                five_hour: crate::models::QuotaWindow {
                    label: "5-hour".into(),
                    percent_used: Some(five_hour),
                    reset_at: None,
                },
                weekly: crate::models::QuotaWindow {
                    label: "Weekly".into(),
                    percent_used: Some(40.0),
                    reset_at: None,
                },
                models: None,
                plan: None,
                updated_at: None,
                error: None,
            }),
        }
    }

    /// Candidates are allowed above the switch threshold, so a quota-driven switch must still
    /// pick an account with more headroom — otherwise the app ping-pongs between two accounts
    /// that are both over the threshold, spawning a resume console every poll.
    #[test]
    fn quota_driven_switch_requires_an_account_with_more_headroom() {
        let accounts = vec![codex_account("current", 93.0), codex_account("worse", 94.0)];

        assert!(
            select_candidate(&accounts, "current", Some(93.0)).is_none(),
            "an account with less headroom is not an improvement"
        );

        // A switch forced by a real quota error can't trust the numbers and takes what's there.
        assert_eq!(
            select_candidate(&accounts, "current", None).map(|a| a.id.as_str()),
            Some("worse")
        );

        let accounts = vec![
            codex_account("current", 93.0),
            codex_account("better", 10.0),
        ];
        assert_eq!(
            select_candidate(&accounts, "current", Some(93.0)).map(|a| a.id.as_str()),
            Some("better")
        );
    }

    #[test]
    fn unknown_five_hour_quota_can_be_a_candidate_when_weekly_is_known() {
        let account = Account {
            id: "candidate".into(),
            name: "candidate".into(),
            tool_id: ToolId::Codex,
            state: AccountState::Idle,
            fingerprint: "candidate".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_used_at: None,
            launcher_command: None,
            launcher_paths: None,
            is_default: false,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            quota_status: None,
            codex_login_method: crate::models::CodexLoginMethod::DeviceCode,
            quota: Some(crate::models::QuotaInfo {
                five_hour: crate::models::QuotaWindow {
                    label: "5-hour".into(),
                    percent_used: None,
                    reset_at: None,
                },
                weekly: crate::models::QuotaWindow {
                    label: "Weekly".into(),
                    percent_used: Some(40.0),
                    reset_at: None,
                },
                models: None,
                plan: None,
                updated_at: None,
                error: None,
            }),
        };
        assert_eq!(
            select_candidate(&[account], "current", None).map(|a| a.id.as_str()),
            Some("candidate")
        );
    }
}
