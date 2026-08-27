use crate::datasource::{DataSourceError, QueryId};
use crate::editor::RunUnit;
use crate::ui::RequestId;

pub struct QueryRequest {
    pub id: RequestId,
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOutcome {
    Rows(crate::ui::grid::Page),
    NoResultSet { rows_affected: u64 },
}

pub enum QueryResponse {
    /// Emitted by the worker as soon as execute() returns Ok, BEFORE any row
    /// is consumed -- this is the only way the UI learns the QueryId while
    /// the query is still cancellable.
    Started {
        id: RequestId,
        query_id: QueryId,
    },
    Finished {
        id: RequestId,
        result: Result<QueryOutcome, DataSourceError>,
    },
    CancelFailed {
        id: RequestId,
        message: String,
    },
}

pub enum RunOutcome {
    Next(QueryRequest),
    Done(RunSummary),
}

pub struct RunSummary {
    pub ran: usize,
    pub total: usize,
    pub last_affected: Option<u64>,
    pub failed: Option<String>,
    pub cancelled: Option<CancelOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Server aborted the statement (DataSourceError::Cancelled) -- its
    /// effects are rolled back.
    Interrupted,
    /// The statement completed and committed before the cancel landed; the
    /// run stopped before the next statement.
    CompletedFirst,
}

/// Scopes a cancel to the `RequestId` that was active when it was requested,
/// so `CancelFailed` (which only carries a `QueryId`-derived error from the
/// transport, not the request that triggered it) can still be checked
/// against a possibly-superseded run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelRequest {
    pub id: RequestId,
    pub query_id: QueryId,
}

pub struct RunState {
    plan: Vec<RunUnit>,
    index: usize,
    active_id: Option<RequestId>,
    active_query_id: Option<QueryId>,
    cancel_requested: bool,
    next_request_id: u64,
    ran: usize,
    last_affected: Option<u64>,
}

impl RunState {
    pub fn new() -> Self {
        Self {
            plan: Vec::new(),
            index: 0,
            active_id: None,
            active_query_id: None,
            cancel_requested: false,
            next_request_id: 0,
            ran: 0,
            last_affected: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active_id.is_some()
    }

    pub fn current(&self) -> Option<&RunUnit> {
        self.plan.get(self.index)
    }

    pub fn progress(&self) -> (usize, usize) {
        let total = self.plan.len();
        (self.index.saturating_add(1).min(total), total)
    }

    /// None when plan is empty ("nothing to run" is not a run).
    pub fn start(&mut self, plan: Vec<RunUnit>) -> Option<QueryRequest> {
        if plan.is_empty() {
            return None;
        }
        self.plan = plan;
        self.index = 0;
        self.ran = 0;
        self.last_affected = None;
        self.cancel_requested = false;
        self.active_query_id = None;
        let id = self.next_id();
        self.active_id = Some(id);
        Some(QueryRequest {
            id,
            sql: self.plan[0].sql.clone(),
        })
    }

    pub fn owns(&self, id: RequestId) -> bool {
        self.active_id == Some(id)
    }

    /// Returns Some(CancelRequest) when a cancel was already requested
    /// (before the QueryId was known) and must be fired now.
    pub fn on_started(&mut self, id: RequestId, qid: QueryId) -> Option<CancelRequest> {
        if !self.owns(id) {
            return None;
        }
        self.active_query_id = Some(qid);
        if self.cancel_requested {
            Some(CancelRequest { id, query_id: qid })
        } else {
            None
        }
    }

    /// Marks a cancel wanted; returns Some(CancelRequest) if it can be sent
    /// immediately (the QueryId is already known), None if it must wait for
    /// on_started.
    pub fn request_cancel(&mut self) -> Option<CancelRequest> {
        if !self.is_active() {
            return None;
        }
        self.cancel_requested = true;
        let id = self
            .active_id
            .expect("is_active() confirmed active_id is Some");
        let query_id = self.active_query_id?;
        Some(CancelRequest { id, query_id })
    }

    pub fn on_finished(
        &mut self,
        id: RequestId,
        result: &Result<QueryOutcome, DataSourceError>,
    ) -> Option<RunOutcome> {
        if !self.owns(id) {
            return None;
        }
        self.ran += 1;
        if let Ok(QueryOutcome::NoResultSet { rows_affected }) = result {
            self.last_affected = Some(*rows_affected);
        }
        let total = self.plan.len();
        match result {
            Err(e) => {
                // A genuine server-side Cancelled is an interruption
                // regardless of whether `self.cancel_requested` was actually
                // set -- a cancel from elsewhere (e.g. a future
                // admin-initiated pg_cancel_backend) is still a real
                // interruption, not a clean failure.
                let cancelled =
                    matches!(e, DataSourceError::Cancelled).then_some(CancelOutcome::Interrupted);
                let failed = if cancelled.is_some() {
                    None
                } else {
                    Some(crate::ui::error_chain(e))
                };
                self.active_id = None;
                Some(RunOutcome::Done(RunSummary {
                    ran: self.ran,
                    total,
                    last_affected: self.last_affected,
                    failed,
                    cancelled,
                }))
            }
            Ok(_) => {
                // request_cancel is intent to stop the whole RUN, not just the
                // statement in flight. Postgres's cancel is out-of-band and
                // unacknowledged, so a fast statement can return Ok() before
                // the cancel lands -- advancing here would run statements the
                // user explicitly asked to stop, including destructive ones.
                if self.cancel_requested {
                    self.active_id = None;
                    return Some(RunOutcome::Done(RunSummary {
                        ran: self.ran,
                        total,
                        last_affected: self.last_affected,
                        failed: None,
                        cancelled: Some(CancelOutcome::CompletedFirst),
                    }));
                }
                self.index += 1;
                if self.index >= total {
                    self.active_id = None;
                    Some(RunOutcome::Done(RunSummary {
                        ran: self.ran,
                        total,
                        last_affected: self.last_affected,
                        failed: None,
                        cancelled: None,
                    }))
                } else {
                    let id = self.next_id();
                    self.active_id = Some(id);
                    self.active_query_id = None;
                    self.cancel_requested = false;
                    Some(RunOutcome::Next(QueryRequest {
                        id,
                        sql: self.plan[self.index].sql.clone(),
                    }))
                }
            }
        }
    }

    fn next_id(&mut self) -> RequestId {
        let id = RequestId(self.next_request_id);
        self.next_request_id += 1;
        id
    }
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::ByteSpan;

    fn unit(sql: &str) -> RunUnit {
        RunUnit {
            sql: sql.to_string(),
            span: ByteSpan { start: 0, end: 0 },
            start: crate::editor::Position::default(),
        }
    }

    // Trace-through per the review requirement: a 3-statement run where
    // statement 2 fails must never issue statement 3, and the summary must
    // report "2 of 3, failed".
    #[test]
    fn a_failing_middle_statement_stops_the_run_and_reports_progress() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("BAD SQL"), unit("SELECT 3")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");

        let outcome1 = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("first statement belongs to the active run");
        let req2 = match outcome1 {
            RunOutcome::Next(req) => req,
            RunOutcome::Done(_) => panic!("must not finish after statement 1 of 3"),
        };
        assert_eq!(req2.sql, "BAD SQL");

        let outcome2 = state
            .on_finished(
                req2.id,
                &Err(DataSourceError::Busy {
                    name: "test".to_string(),
                }),
            )
            .expect("second statement belongs to the active run");
        let summary = match outcome2 {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!("a failed statement must stop the run, not continue"),
        };
        assert_eq!(summary.ran, 2);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.cancelled, None);
        assert!(summary.failed.is_some());
        assert!(!state.is_active());

        // A stale response for the request id that WOULD have been statement
        // 3 must never have been issued in the first place -- there is no
        // third id to even construct here, which is the point.
    }

    #[test]
    fn start_on_an_empty_plan_returns_none_and_never_activates() {
        let mut state = RunState::new();
        assert!(state.start(Vec::new()).is_none());
        assert!(!state.is_active());
    }

    #[test]
    fn a_three_statement_run_where_all_succeed_advances_one_at_a_time_and_summarizes() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("SELECT 2"), unit("SELECT 3")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");
        assert_eq!(state.progress(), (1, 3));
        assert_eq!(state.current().map(|u| u.sql.as_str()), Some("SELECT 1"));

        let outcome1 = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let req2 = match outcome1 {
            RunOutcome::Next(req) => req,
            RunOutcome::Done(_) => panic!("must not finish after statement 1 of 3"),
        };
        assert_eq!(req2.sql, "SELECT 2");
        assert_eq!(state.progress(), (2, 3));
        assert_eq!(state.current().map(|u| u.sql.as_str()), Some("SELECT 2"));

        let outcome2 = state
            .on_finished(req2.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 2 belongs to the active run");
        let req3 = match outcome2 {
            RunOutcome::Next(req) => req,
            RunOutcome::Done(_) => panic!("must not finish after statement 2 of 3"),
        };
        assert_eq!(req3.sql, "SELECT 3");
        assert_eq!(state.progress(), (3, 3));

        let outcome3 = state
            .on_finished(req3.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 7 }))
            .expect("statement 3 belongs to the active run");
        let summary = match outcome3 {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!("the run must finish after its last statement"),
        };
        assert_eq!(summary.ran, 3);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.failed, None);
        assert_eq!(summary.cancelled, None);
        assert_eq!(summary.last_affected, Some(7));
        assert!(!state.is_active());
    }

    #[test]
    fn cancel_requested_before_the_query_id_is_known_defers_and_then_fires_on_started() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT pg_sleep(10)")];
        state.start(plan).expect("non-empty plan starts a run");

        // Cancel races ahead of `on_started`: the worker hasn't reported the
        // QueryId yet, so there's nothing to send a cancel for right now.
        assert_eq!(
            state.request_cancel(),
            None,
            "a cancel requested before the QueryId is known must defer, not fire early"
        );

        let qid = crate::datasource::QueryId::for_test(42);
        let fired = state.on_started(RequestId(0), qid);
        assert_eq!(
            fired,
            Some(CancelRequest {
                id: RequestId(0),
                query_id: qid
            }),
            "on_started must fire the deferred cancel as soon as the QueryId is known"
        );
    }

    #[test]
    fn cancel_requested_after_the_query_id_is_known_fires_immediately() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT pg_sleep(10)")];
        state.start(plan).expect("non-empty plan starts a run");

        let qid = crate::datasource::QueryId::for_test(7);
        assert_eq!(
            state.on_started(RequestId(0), qid),
            None,
            "on_started must not fire a cancel that was never requested"
        );

        assert_eq!(
            state.request_cancel(),
            Some(CancelRequest {
                id: RequestId(0),
                query_id: qid
            }),
            "a cancel requested after the QueryId is known must fire immediately"
        );
    }

    #[test]
    fn on_started_and_on_finished_with_a_stale_request_id_are_complete_noops() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("SELECT 2")];
        state.start(plan).expect("non-empty plan starts a run");
        let stale_id = RequestId(999);
        assert!(!state.owns(stale_id));

        let qid = crate::datasource::QueryId::for_test(1);
        assert_eq!(
            state.on_started(stale_id, qid),
            None,
            "on_started for an id that isn't the active one must be a no-op"
        );
        assert!(
            state
                .on_finished(
                    stale_id,
                    &Ok(QueryOutcome::NoResultSet { rows_affected: 0 })
                )
                .is_none(),
            "on_finished for an id that isn't the active one must be a no-op"
        );

        // Neither call should have perturbed the real, still-active run.
        assert!(state.is_active());
        assert_eq!(state.progress(), (1, 2));
        assert_eq!(state.current().map(|u| u.sql.as_str()), Some("SELECT 1"));
    }

    #[test]
    fn a_cancelled_statement_reports_cancelled_true_and_failed_none_distinctly() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT pg_sleep(10)"), unit("SELECT 2")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");

        let outcome = state
            .on_finished(req1.id, &Err(DataSourceError::Cancelled))
            .expect("the cancelled statement belongs to the active run");
        let summary = match outcome {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!("a cancelled statement must stop the run"),
        };
        assert_eq!(
            summary.cancelled,
            Some(CancelOutcome::Interrupted),
            "a genuine Cancelled error must report CancelOutcome::Interrupted"
        );
        assert_eq!(
            summary.failed, None,
            "a cancelled run must not ALSO report failed: Some(_) -- they're mutually exclusive"
        );
        assert_eq!(summary.ran, 1);
        assert_eq!(summary.total, 2);
        assert!(!state.is_active());
    }

    #[test]
    fn progress_reflects_the_current_step_and_freezes_at_its_last_value_once_done() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("SELECT 2")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");
        assert_eq!(state.progress(), (1, 2));

        let outcome = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let req2 = match outcome {
            RunOutcome::Next(req) => req,
            RunOutcome::Done(_) => panic!("must not finish after statement 1 of 2"),
        };
        assert_eq!(state.progress(), (2, 2));

        let outcome2 = state
            .on_finished(req2.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 2 belongs to the active run");
        assert!(matches!(outcome2, RunOutcome::Done(_)));
        assert!(!state.is_active());
        assert_eq!(state.progress(), (2, 2));
    }

    #[test]
    fn progress_on_a_fresh_never_started_run_state_is_zero_of_zero() {
        let state = RunState::new();
        assert_eq!(state.progress(), (0, 0));
    }

    #[test]
    fn request_ids_are_monotonic_within_and_across_separate_runs() {
        let mut state = RunState::new();
        let plan1 = vec![unit("SELECT 1"), unit("SELECT 2")];
        let req1 = state.start(plan1).expect("first run starts");
        let outcome = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let req2 = match outcome {
            RunOutcome::Next(req) => req,
            RunOutcome::Done(_) => panic!("must not finish after statement 1 of 2"),
        };
        assert!(
            req2.id.0 > req1.id.0,
            "ids must increase within a single run"
        );
        let outcome2 = state
            .on_finished(req2.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 2 belongs to the active run");
        assert!(matches!(outcome2, RunOutcome::Done(_)));

        // A second, independent run must mint ids that never collide with
        // (or repeat) the first run's -- DataGridState::finish_query's
        // staleness check assumes ids from the same counter are comparable
        // across runs, not just within one.
        let plan2 = vec![unit("SELECT 3")];
        let req3 = state.start(plan2).expect("second run starts");
        assert!(
            req3.id.0 > req2.id.0,
            "the second run's ids must continue strictly increasing past the first run's, got \
             req1={:?} req2={:?} req3={:?}",
            req1.id,
            req2.id,
            req3.id
        );
    }

    #[test]
    fn a_cancel_requested_before_a_statement_finishes_ok_stops_the_run_instead_of_advancing() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("SELECT 2"), unit("SELECT 3")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");

        let qid = crate::datasource::QueryId::for_test(1);
        state.on_started(req1.id, qid);
        assert_eq!(
            state.request_cancel(),
            Some(CancelRequest {
                id: req1.id,
                query_id: qid
            }),
            "a cancel requested after the QueryId is known must fire immediately"
        );

        let outcome = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let summary = match outcome {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!(
                "a cancel requested before a fast Ok() lands must stop the run, not advance to \
                 the next statement -- otherwise a destructive statement the user explicitly \
                 asked to cancel would still run"
            ),
        };
        assert_eq!(summary.cancelled, Some(CancelOutcome::CompletedFirst));
        assert_eq!(summary.failed, None);
        assert_eq!(summary.ran, 1);
        assert_eq!(summary.total, 3);
        assert!(!state.is_active());
    }

    #[test]
    fn a_cancel_requested_before_the_query_id_is_known_still_stops_a_statement_that_finishes_ok() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1"), unit("SELECT 2"), unit("SELECT 3")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");

        assert_eq!(
            state.request_cancel(),
            None,
            "a cancel requested before the QueryId is known must defer, not fire early"
        );

        let outcome = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let summary = match outcome {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!(
                "a cancel requested before the QueryId is known must still stop the run once the \
                 in-flight statement finishes Ok(), not advance to the next statement"
            ),
        };
        assert_eq!(summary.cancelled, Some(CancelOutcome::CompletedFirst));
        assert_eq!(summary.failed, None);
        assert_eq!(summary.ran, 1);
        assert_eq!(summary.total, 3);
        assert!(!state.is_active());
    }

    #[test]
    fn a_cancel_on_the_last_statement_that_finishes_ok_reports_cancelled_not_a_clean_completion() {
        let mut state = RunState::new();
        let plan = vec![unit("SELECT 1")];
        let req1 = state.start(plan).expect("non-empty plan starts a run");

        let qid = crate::datasource::QueryId::for_test(1);
        state.on_started(req1.id, qid);
        assert_eq!(
            state.request_cancel(),
            Some(CancelRequest {
                id: req1.id,
                query_id: qid
            })
        );

        let outcome = state
            .on_finished(req1.id, &Ok(QueryOutcome::NoResultSet { rows_affected: 0 }))
            .expect("statement 1 belongs to the active run");
        let summary = match outcome {
            RunOutcome::Done(summary) => summary,
            RunOutcome::Next(_) => panic!("a 1-statement plan must not advance further"),
        };
        assert_eq!(
            summary.cancelled,
            Some(CancelOutcome::CompletedFirst),
            "the cancel check must happen before the index >= total clean-completion branch"
        );
        assert_eq!(summary.failed, None);
        assert_eq!(summary.ran, 1);
        assert_eq!(summary.total, 1);
        assert!(!state.is_active());
    }
}
