use crate::models::{AutoPingSetting, QuotaInfo};
use chrono::{DateTime, Duration, Utc};

pub const RESET_GRACE_MINUTES: i64 = 3;
pub const ACCOUNT_GAP_SECONDS: u64 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PingDecision {
    PingNow { event_key: String },
    WaitUntil(String),
    Paused(&'static str),
}

pub fn should_ping(
    setting: &AutoPingSetting,
    quota: Option<&QuotaInfo>,
    now: DateTime<Utc>,
) -> PingDecision {
    if !setting.enabled {
        return PingDecision::Paused("disabled");
    }
    let Some(quota) = quota else {
        return PingDecision::Paused("quota unavailable");
    };
    if quota.error.is_some() {
        return PingDecision::Paused("quota unavailable");
    }
    if quota
        .weekly
        .percent_used
        .is_some_and(|value| value >= 100.0)
    {
        return PingDecision::Paused("weekly quota exhausted");
    }

    match quota.five_hour.reset_at.as_deref() {
        Some(raw) => match DateTime::parse_from_rfc3339(raw) {
            Ok(reset) if reset.with_timezone(&Utc) > now => {
                PingDecision::WaitUntil(raw.to_string())
            }
            Ok(reset) => {
                let due = reset.with_timezone(&Utc) + Duration::minutes(RESET_GRACE_MINUTES);
                let event_key = reset.to_rfc3339();
                if setting.last_attempted_reset.as_deref() == Some(event_key.as_str()) {
                    retry_decision(setting, now, event_key)
                } else if due > now {
                    PingDecision::WaitUntil(due.to_rfc3339())
                } else {
                    PingDecision::PingNow { event_key }
                }
            }
            Err(_) => PingDecision::Paused("invalid reset time"),
        },
        None => {
            let event_key = "no-active-window".to_string();
            if setting.last_attempted_reset.as_deref() == Some(event_key.as_str()) {
                retry_decision(setting, now, event_key)
            } else {
                PingDecision::PingNow { event_key }
            }
        }
    }
}

fn retry_decision(
    setting: &AutoPingSetting,
    now: DateTime<Utc>,
    event_key: String,
) -> PingDecision {
    if setting.retry_count == 0 || setting.retry_count >= 5 {
        return PingDecision::Paused("already attempted");
    }
    if let Some(next) = setting
        .next_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
    {
        if next > now {
            return PingDecision::WaitUntil(next.to_rfc3339());
        }
    }
    PingDecision::PingNow { event_key }
}

pub fn next_ping_time(decision: &PingDecision) -> Option<&str> {
    match decision {
        PingDecision::WaitUntil(value) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{QuotaInfo, QuotaWindow};

    fn quota(five_reset: Option<&str>, weekly: f64) -> QuotaInfo {
        QuotaInfo {
            five_hour: QuotaWindow {
                label: "5-hour limit".into(),
                percent_used: Some(0.0),
                reset_at: five_reset.map(str::to_string),
            },
            weekly: QuotaWindow {
                label: "Weekly limit".into(),
                percent_used: Some(weekly),
                reset_at: None,
            },
            plan: None,
            models: None,
            error: None,
            updated_at: None,
        }
    }

    #[test]
    fn startup_without_active_window_pings() {
        assert!(matches!(
            should_ping(
                &AutoPingSetting {
                    enabled: true,
                    ..Default::default()
                },
                Some(&quota(None, 10.0)),
                Utc::now()
            ),
            PingDecision::PingNow { .. }
        ));
    }

    #[test]
    fn active_future_window_waits() {
        let now = Utc::now();
        let reset = (now + Duration::hours(1)).to_rfc3339();
        assert_eq!(
            should_ping(
                &AutoPingSetting {
                    enabled: true,
                    ..Default::default()
                },
                Some(&quota(Some(&reset), 10.0)),
                now
            ),
            PingDecision::WaitUntil(reset)
        );
    }

    #[test]
    fn expired_window_waits_for_grace_then_pings() {
        let now = Utc::now();
        let reset = (now - Duration::minutes(4)).to_rfc3339();
        assert!(matches!(
            should_ping(
                &AutoPingSetting {
                    enabled: true,
                    ..Default::default()
                },
                Some(&quota(Some(&reset), 10.0)),
                now
            ),
            PingDecision::PingNow { .. }
        ));
    }

    #[test]
    fn attempted_reset_is_deduped_across_restart() {
        let now = Utc::now();
        let reset = (now - Duration::minutes(4)).to_rfc3339();
        let setting = AutoPingSetting {
            enabled: true,
            last_attempted_reset: Some(reset.clone()),
            ..Default::default()
        };
        assert_eq!(
            should_ping(&setting, Some(&quota(Some(&reset), 10.0)), now),
            PingDecision::Paused("already attempted")
        );
    }

    #[test]
    fn transient_failure_retries_with_bound() {
        let now = Utc::now();
        let reset = (now - Duration::minutes(4)).to_rfc3339();
        let retry = AutoPingSetting {
            enabled: true,
            last_attempted_reset: Some(reset.clone()),
            retry_count: 1,
            next_at: Some((now - Duration::seconds(1)).to_rfc3339()),
            ..Default::default()
        };
        assert!(matches!(
            should_ping(&retry, Some(&quota(Some(&reset), 10.0)), now),
            PingDecision::PingNow { .. }
        ));
        let exhausted = AutoPingSetting {
            retry_count: 5,
            ..retry
        };
        assert_eq!(
            should_ping(&exhausted, Some(&quota(Some(&reset), 10.0)), now),
            PingDecision::Paused("already attempted")
        );
    }

    #[test]
    fn weekly_exhaustion_pauses() {
        assert_eq!(
            should_ping(
                &AutoPingSetting {
                    enabled: true,
                    ..Default::default()
                },
                Some(&quota(None, 100.0)),
                Utc::now()
            ),
            PingDecision::Paused("weekly quota exhausted")
        );
    }

    #[test]
    fn invalid_or_missing_quota_fails_safely() {
        assert_eq!(
            should_ping(
                &AutoPingSetting {
                    enabled: true,
                    ..Default::default()
                },
                None,
                Utc::now()
            ),
            PingDecision::Paused("quota unavailable")
        );
    }
}
