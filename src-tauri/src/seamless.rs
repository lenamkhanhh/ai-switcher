use crate::models::{Account, AccountState, ResumeJob, ResumeJobStatus, ToolId};
use std::path::Path;

pub const DEFAULT_HANDOFF_THRESHOLD: f64 = 92.0;
pub const CANDIDATE_LIMIT: f64 = 95.0;

pub fn should_checkpoint(percent_used: Option<f64>, session_idle: bool) -> bool {
    session_idle && percent_used.is_some_and(|value| value >= DEFAULT_HANDOFF_THRESHOLD)
}

pub fn resume_args<'a>(session_id: &'a str, prompt: &'a str) -> [&'a str; 3] {
    ["resume", session_id, prompt]
}

pub fn select_candidate<'a>(accounts: &'a [Account], current_id: &str) -> Option<&'a Account> {
    let mut eligible: Vec<&Account> = accounts
        .iter()
        .filter(|account| {
            account.tool_id == ToolId::Codex
                && account.id != current_id
                && account.api_provider.is_none()
                && account.state != AccountState::NeedsLogin
                && account.quota.as_ref().is_some_and(|quota| {
                    quota.error.is_none()
                        && quota
                            .five_hour
                            .percent_used
                            .is_some_and(|v| v < CANDIDATE_LIMIT)
                        && quota.weekly.percent_used.is_some_and(|v| v < 100.0)
                })
        })
        .collect();
    eligible.sort_by(|left, right| {
        let lq = left.quota.as_ref().expect("filtered");
        let rq = right.quota.as_ref().expect("filtered");
        lq.five_hour
            .percent_used
            .unwrap_or(100.0)
            .total_cmp(&rq.five_hour.percent_used.unwrap_or(100.0))
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
    fn resume_is_exact_and_never_uses_last() {
        assert_eq!(
            resume_args("session-uuid", "checkpoint"),
            ["resume", "session-uuid", "checkpoint"]
        );
        assert!(!resume_args("session-uuid", "checkpoint").contains(&"--last"));
    }
}
