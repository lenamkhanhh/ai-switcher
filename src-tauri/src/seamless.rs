use crate::models::{Account, AccountState, ResumeJob, ResumeJobStatus, ToolId};
pub const DEFAULT_HANDOFF_THRESHOLD: f64 = 92.0;
pub const CANDIDATE_LIMIT: f64 = 95.0;

pub fn should_start_handoff(
    percent_used: Option<f64>,
    explicit_quota_error: bool,
    threshold: f64,
) -> bool {
    explicit_quota_error || percent_used.is_some_and(|value| value >= threshold)
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
pub fn rank_candidates<'a>(
    accounts: &'a [Account],
    current_id: &str,
    must_improve_on: Option<f64>,
) -> Vec<&'a Account> {
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
    eligible
}

/// Durable dedupe key for the transactional path. The old `(session,event)` key remains for
/// loading legacy state, but a retryable new recovery is bound to the exact failed turn and the
/// structured failure fingerprint.
pub fn is_recovery_duplicate(
    jobs: &[ResumeJob],
    thread_id: &str,
    failed_turn_id: &str,
    terminal_error_fingerprint: &str,
) -> bool {
    jobs.iter().any(|job| {
        job.thread_id == thread_id
            && job.failed_turn_id == failed_turn_id
            && job.terminal_error_fingerprint.as_deref() == Some(terminal_error_fingerprint)
            && !matches!(
                job.status,
                ResumeJobStatus::Cancelled | ResumeJobStatus::Failed
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_window_only_handoffs_after_an_explicit_quota_error() {
        assert!(!should_start_handoff(None, false, 92.0));
        assert!(should_start_handoff(None, true, 92.0));
        assert!(should_start_handoff(Some(92.0), false, 92.0));
    }

    #[test]
    fn recovery_dedupe_is_bound_to_thread_failed_turn_and_structured_fingerprint() {
        let mut job = ResumeJob {
            id: "job-1".into(),
            schema_version: 3,
            recovery_id: "recovery-1".into(),
            session_id: "thread-1".into(),
            thread_id: "thread-1".into(),
            failed_turn_id: "turn-1".into(),
            resumed_turn_id: None,
            event_key: "legacy".into(),
            cwd: "D:\\repo".into(),
            from_account_id: "old".into(),
            to_account_id: Some("new".into()),
            checkpoint_path: "checkpoint".into(),
            rollout_path: "rollout".into(),
            rollout_offset: 12,
            terminal_error_kind: Some(crate::models::RecoveryErrorKind::UsageLimitExceeded),
            terminal_error_fingerprint: Some("fingerprint".into()),
            manifest_path: Some("manifest".into()),
            recovery_marker: Some("marker".into()),
            model: Some("gpt-5".into()),
            reasoning_effort: Some("high".into()),
            personality: None,
            approval_policy: None,
            sandbox_policy: None,
            goal_snapshot: None,
            attempt: 1,
            status: ResumeJobStatus::Checkpointed,
            created_at: "now".into(),
            updated_at: "now".into(),
            last_ack_at: None,
            error: None,
        };
        assert!(is_recovery_duplicate(
            &[job.clone()],
            "thread-1",
            "turn-1",
            "fingerprint"
        ));
        job.failed_turn_id = "turn-2".into();
        assert!(!is_recovery_duplicate(
            &[job],
            "thread-1",
            "turn-1",
            "fingerprint"
        ));
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
            rank_candidates(&accounts, "current", Some(93.0)).is_empty(),
            "an account with less headroom is not an improvement"
        );

        // A switch forced by a real quota error can't trust the numbers and takes what's there.
        assert_eq!(
            rank_candidates(&accounts, "current", None)
                .first()
                .map(|a| a.id.as_str()),
            Some("worse")
        );

        let accounts = vec![
            codex_account("current", 93.0),
            codex_account("better", 10.0),
        ];
        assert_eq!(
            rank_candidates(&accounts, "current", Some(93.0))
                .first()
                .map(|a| a.id.as_str()),
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
            rank_candidates(&[account], "current", None)
                .first()
                .map(|a| a.id.as_str()),
            Some("candidate")
        );
    }
}
