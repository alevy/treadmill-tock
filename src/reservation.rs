//! Reservation decision: whether to reuse a held job or create a new one.
//!
//! Reaching a device under test means holding a Treadmill job (a
//! "reservation") on a host wired to it. [`choose`] is the pure decision at
//! the heart of that: given the caller's current jobs and a report of which
//! hosts match the board's predicate ([`predicate_for`]), it decides whether
//! an existing job already serves the board, or whether a new one is needed.
//! Reusing a job is what makes the second `tock` command fast instead of
//! re-provisioning hardware every time.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::hosts::HostRequirementsReport;
use treadmill_rs::api::switchboard::jobs::JobSummary;

/// What a caller should do to get a job serving a board right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Reuse the job named by `job_id`. `extend` is true when its lease
    /// should be pushed out before relying on it further.
    Reuse { job_id: Uuid, extend: bool },
    /// No usable job exists; create a new one.
    Create,
}

/// Decides whether an existing job already serves the board `report` was
/// computed for, or whether a new one is needed.
///
/// Picks the first job in `jobs` that is [`JobState::Ready`] and whose
/// `dispatched_on_host_id` is among `report.schedulable`. `jobs` should be
/// the caller's **active and reclaimable** job listings, concatenated: a
/// reclaimable job has an expired preempt lease but is still running and
/// usable, so it is just as good a reservation as an active one — until the
/// scheduler actually reclaims its host.
///
/// For the job picked, `extend` is true when its `lease_expires_at` is
/// `None` (no lease yet) or falls within `extend_within` of `now` —
/// including already in the past, which is exactly the reclaimable case.
pub fn choose(
    jobs: &[JobSummary],
    report: &HostRequirementsReport,
    now: DateTime<Utc>,
    extend_within: Duration,
) -> Decision {
    jobs.iter()
        .find(|job| {
            job.state == JobState::Ready
                && job
                    .dispatched_on_host_id
                    .is_some_and(|host_id| report.schedulable.contains(&host_id))
        })
        .map(|job| Decision::Reuse {
            job_id: job.job_id,
            extend: job
                .lease_expires_at
                .is_none_or(|expires_at| expires_at - now <= extend_within),
        })
        .unwrap_or(Decision::Create)
}

/// The host predicate that selects a host with `board` wired to it.
///
/// Evaluated server-side against `host.duts` (see
/// [`HostRequirementsReport`]), so its result is exactly what `choose`'s
/// `report.schedulable` is computed from.
pub fn predicate_for(board: &str) -> String {
    format!("host.duts.exists(d, d.board == '{board}')")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::TimeDelta;
    use uuid::Uuid;

    use treadmill_rs::api::switchboard::jobs::{JobImage, JobImageReference, JobLeaseExpiryAction};

    use super::*;

    /// A `JobSummary` fixture with every field but the ones under test
    /// pinned to an arbitrary-but-valid value.
    fn job(
        state: JobState,
        dispatched_on_host_id: Option<Uuid>,
        lease_expires_at: Option<DateTime<Utc>>,
    ) -> JobSummary {
        JobSummary {
            job_id: Uuid::new_v4(),
            label: None,
            owner: None,
            revision: 0,
            annotations: BTreeMap::new(),
            state,
            image: JobImage {
                reference: JobImageReference::ImageSet {
                    set_id: Uuid::new_v4(),
                    generation: 0,
                },
                resolved_digest: None,
            },
            image_name: None,
            predecessor: None,
            queued_at: Utc::now(),
            started_at: None,
            terminated_at: None,
            dispatched_on_host_id,
            host_name: None,
            termination_reason: None,
            task_exit_status: None,
            lease_expires_at,
            lease_expiry_action: JobLeaseExpiryAction::Preempt,
        }
    }

    fn report(schedulable: Vec<Uuid>) -> HostRequirementsReport {
        HostRequirementsReport {
            authorized: schedulable.len() as u32,
            predicate_matched: schedulable.len() as u32,
            image_matched: None,
            schedulable,
            errored: 0,
            errors: vec![],
            compile_error: None,
            hosts: vec![],
        }
    }

    const EXTEND_WITHIN: TimeDelta = TimeDelta::minutes(15);

    #[test]
    fn reuses_a_ready_job_on_a_matching_host() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now + TimeDelta::hours(2)),
        );
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: false
            }
        );
    }

    #[test]
    fn ignores_a_ready_job_on_a_host_the_predicate_does_not_match() {
        let host_id = Uuid::new_v4();
        let other_host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now + TimeDelta::hours(2)),
        );

        let decision = choose(&[target], &report(vec![other_host_id]), now, EXTEND_WITHIN);

        assert_eq!(decision, Decision::Create);
    }

    #[test]
    fn ignores_a_job_that_is_not_ready() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Initializing,
            Some(host_id),
            Some(now + TimeDelta::hours(2)),
        );

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(decision, Decision::Create);
    }

    #[test]
    fn reuses_a_reclaimable_job() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now - TimeDelta::minutes(5)),
        );
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: true
            }
        );
    }

    #[test]
    fn a_job_with_no_lease_expiry_is_extended() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(JobState::Ready, Some(host_id), None);
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: true
            }
        );
    }

    #[test]
    fn extends_a_lease_inside_the_window() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now + TimeDelta::minutes(4)),
        );
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: true
            }
        );
    }

    #[test]
    fn a_lease_expiring_exactly_at_the_window_is_extended() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(JobState::Ready, Some(host_id), Some(now + EXTEND_WITHIN));
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: true
            }
        );
    }

    #[test]
    fn leaves_a_long_lease_alone() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now + TimeDelta::hours(2)),
        );
        let job_id = target.job_id;

        let decision = choose(&[target], &report(vec![host_id]), now, EXTEND_WITHIN);

        assert_eq!(
            decision,
            Decision::Reuse {
                job_id,
                extend: false
            }
        );
    }

    #[test]
    fn empty_schedulable_set_creates() {
        let host_id = Uuid::new_v4();
        let now = Utc::now();
        let target = job(
            JobState::Ready,
            Some(host_id),
            Some(now + TimeDelta::hours(2)),
        );

        let decision = choose(&[target], &report(vec![]), now, EXTEND_WITHIN);

        assert_eq!(decision, Decision::Create);
    }

    #[test]
    fn predicate_quotes_the_board_identifier() {
        assert_eq!(
            predicate_for("nrf52840dk"),
            "host.duts.exists(d, d.board == 'nrf52840dk')"
        );
    }
}
