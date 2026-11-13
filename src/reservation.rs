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

/// A reservation that can never be placed, and why.
///
/// Separate from the anyhow chain around it so the caller can tell this
/// apart from a switchboard that was unreachable: this one is the caller's
/// to fix — a typo in the board name, a permission they do not have, a
/// fleet with nothing in it — and is reported as a `Selection` refusal
/// rather than a device failure.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Unplaceable(pub String);

/// Why a job carrying `predicate` could never be placed, or `None` if some
/// host could run it right now.
///
/// A job whose `schedulable` set is empty is still *accepted* by `POST
/// /jobs` — the fleet may change — and then sits queued forever, which from
/// outside is indistinguishable from waiting for a busy host. The report
/// that comes back from `POST /hosts/match` and from the enqueue itself
/// distinguishes every way that happens, and this is where that
/// distinction becomes a sentence: a predicate that did not compile, no
/// authorized host to evaluate it against, a predicate that compiled and
/// matched nothing, an image set that admits nothing, and the two that
/// matched but never the same host.
///
/// Hosts that errored evaluating the predicate are carried through
/// verbatim: a forgotten `has()` guard otherwise looks exactly like an
/// empty fleet.
pub fn unplaceable(report: &HostRequirementsReport, predicate: &str) -> Option<Unplaceable> {
    if let Some(error) = &report.compile_error {
        return Some(Unplaceable(format!(
            "the host predicate does not compile: {error}\n  predicate: {predicate}"
        )));
    }
    if !report.schedulable.is_empty() {
        return None;
    }

    if report.authorized == 0 {
        return Some(Unplaceable(
            "you are not authorized to start a job on any host in this fleet, \
             so nothing can serve this board; ask for access to a host that \
             carries it"
                .to_string(),
        ));
    }

    if report.predicate_matched == 0 {
        let mut message = format!(
            "no host matches this board: the predicate compiled and matched \
             none of the {} host(s) you may start a job on\n  predicate: {predicate}",
            report.authorized
        );
        message.push_str(&errors_note(report));
        message.push_str(
            "\n  check the board option against the Treadmill board identifier \
             (`tml host list` shows what the fleet carries)",
        );
        return Some(Unplaceable(message));
    }

    if report.image_matched == Some(0) {
        return Some(Unplaceable(format!(
            "the predicate matches {} host(s), but the image set admits none \
             of them; it needs a member for their platform profile",
            report.predicate_matched
        )));
    }

    Some(Unplaceable(format!(
        "the predicate matches {} host(s) and the image set admits others, \
         but never the same host, so nothing can run this job\n  predicate: {predicate}",
        report.predicate_matched
    )))
}

/// The hosts that errored evaluating the predicate, as a line to append to
/// a refusal, or nothing if none did.
fn errors_note(report: &HostRequirementsReport) -> String {
    if report.errored == 0 {
        return String::new();
    }
    let mut note = format!(
        "\n  {} host(s) errored evaluating it, which counts as not matching",
        report.errored
    );
    for error in &report.errors {
        note.push_str(&format!("\n    {}: {}", error.name, error.message));
    }
    note
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
mod unplaceable_tests {
    use uuid::Uuid;

    use treadmill_rs::api::switchboard::hosts::{HostPredicateError, HostRequirementsReport};

    use super::*;

    /// A report of a fleet that can run the job: one schedulable host.
    fn placeable() -> HostRequirementsReport {
        HostRequirementsReport {
            authorized: 3,
            predicate_matched: 1,
            image_matched: Some(3),
            schedulable: vec![Uuid::new_v4()],
            errored: 0,
            errors: vec![],
            compile_error: None,
            hosts: vec![],
        }
    }

    /// The same fleet with nothing schedulable, for the callers below to
    /// spoil in one specific way each.
    fn unschedulable() -> HostRequirementsReport {
        HostRequirementsReport {
            schedulable: vec![],
            ..placeable()
        }
    }

    fn message(report: &HostRequirementsReport) -> String {
        unplaceable(report, "host.duts.exists(d, d.board == 'nrf52840dk')")
            .expect("this report should be refused")
            .to_string()
    }

    #[test]
    fn a_fleet_that_can_run_the_job_is_not_refused() {
        assert!(
            unplaceable(&placeable(), "host.name == 'a'").is_none(),
            "a schedulable host means the job can be placed"
        );
    }

    #[test]
    fn a_predicate_that_does_not_compile_says_so() {
        let report = HostRequirementsReport {
            compile_error: Some("undeclared reference to 'dut'".into()),
            authorized: 0,
            predicate_matched: 0,
            ..unschedulable()
        };

        let message = message(&report);

        assert!(message.contains("compile"), "message was: {message}");
        assert!(
            message.contains("undeclared reference to 'dut'"),
            "the evaluator's own diagnostic should survive: {message}"
        );
        assert!(
            message.contains("nrf52840dk"),
            "the predicate itself should be shown: {message}"
        );
    }

    #[test]
    fn no_authorized_host_is_a_permissions_problem() {
        let report = HostRequirementsReport {
            authorized: 0,
            predicate_matched: 0,
            image_matched: Some(0),
            ..unschedulable()
        };

        let message = message(&report);

        assert!(
            message.contains("authorized") || message.contains("permission"),
            "zero authorized hosts is a permissions problem, not a query one: {message}"
        );
    }

    #[test]
    fn a_predicate_that_compiled_and_matched_nothing_says_that_instead() {
        let report = HostRequirementsReport {
            predicate_matched: 0,
            ..unschedulable()
        };

        let message = message(&report);

        assert!(
            message.contains("3"),
            "the size of the fleet it was evaluated over should be named: {message}"
        );
        assert!(
            !message.contains("does not compile"),
            "a predicate that compiled must not be blamed for not compiling: {message}"
        );
        assert!(
            message.contains("nrf52840dk"),
            "the predicate itself should be shown: {message}"
        );
    }

    #[test]
    fn hosts_that_errored_evaluating_the_predicate_are_surfaced() {
        let report = HostRequirementsReport {
            predicate_matched: 0,
            errored: 2,
            errors: vec![HostPredicateError {
                host_id: Uuid::new_v4(),
                name: "rpi5-02".into(),
                message: "no such key: duts".into(),
            }],
            ..unschedulable()
        };

        let message = message(&report);

        assert!(
            message.contains("no such key: duts") && message.contains("rpi5-02"),
            "an evaluation error otherwise looks exactly like an empty fleet: {message}"
        );
    }

    #[test]
    fn an_image_set_that_admits_no_host_is_named_as_such() {
        let report = HostRequirementsReport {
            image_matched: Some(0),
            ..unschedulable()
        };

        let message = message(&report);

        assert!(
            message.contains("image"),
            "a predicate that matched but an image set that did not is an \
             image-set problem: {message}"
        );
    }

    #[test]
    fn a_predicate_and_an_image_set_that_do_not_overlap_say_so() {
        // Both matched something, but never the same host.
        let message = message(&unschedulable());

        assert!(
            message.contains("image"),
            "the overlap is what failed, and the image set is half of it: {message}"
        );
    }
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
