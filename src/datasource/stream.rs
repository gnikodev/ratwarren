use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::OwnedSemaphorePermit;

use super::{DataSourceError, QueryId};
use crate::datasource::postgres::AbandonCtx;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    columns: Arc<[String]>,
    values: Vec<Option<String>>,
}

impl Row {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&str> {
        self.values.get(idx).and_then(|v| v.as_deref())
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    pub fn into_values(self) -> Vec<Option<String>> {
        self.values
    }
}

pub(crate) type BoxedResultStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<ResultMessage, DataSourceError>> + Send>,
>;

pub(crate) enum ResultMessage {
    Columns(Arc<[String]>),
    Row(Vec<Option<String>>),
    Complete { rows_affected: u64 },
    // Catch-all for #[non_exhaustive] SimpleQueryMessage variants we don't
    // handle: must NOT be turned into a fabricated Complete, or it would
    // trigger a spurious MultipleStatements error on a single-statement query.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamState {
    Streaming,
    AfterComplete,
    Ended,
    Aborted,
}

pub struct RowStream {
    pub(crate) inner: BoxedResultStream,
    pub(crate) query_id: QueryId,
    pub(crate) columns: Option<Arc<[String]>>,
    pub(crate) rows_affected: Option<u64>,
    pub(crate) active_id: Arc<AtomicU64>,
    pub(crate) permit: Option<OwnedSemaphorePermit>,
    pub(crate) abandon: Option<AbandonCtx>,
    pub(crate) state: StreamState,
}

impl RowStream {
    pub fn query_id(&self) -> QueryId {
        self.query_id
    }

    pub fn columns(&self) -> Option<&Arc<[String]>> {
        self.columns.as_ref()
    }

    pub fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    pub async fn next(&mut self) -> Option<Result<Row, DataSourceError>> {
        use futures_util::StreamExt;

        loop {
            if matches!(self.state, StreamState::Ended | StreamState::Aborted) {
                return None;
            }
            // Defensive, not fixing a live bug today (see drain_abandoned in
            // postgres.rs for the confirmed one): tokio-postgres's stream has
            // no tokio coop integration and can serve a large buffered batch
            // out of one poll(), which would stall the whole ratatui event
            // loop on the project's actual 2-core-VPS target hardware.
            tokio::task::coop::consume_budget().await;
            match self.inner.next().await {
                Some(Ok(ResultMessage::Ignored)) => continue,
                Some(Ok(ResultMessage::Columns(columns))) => match self.state {
                    StreamState::Streaming => {
                        self.columns = Some(columns);
                        continue;
                    }
                    _ => return Some(Err(self.multi_statement())),
                },
                Some(Ok(ResultMessage::Row(values))) => match self.state {
                    StreamState::Streaming => {
                        let columns = self
                            .columns
                            .clone()
                            .expect("RowDescription always precedes a DataRow");
                        return Some(Ok(Row { columns, values }));
                    }
                    _ => return Some(Err(self.multi_statement())),
                },
                Some(Ok(ResultMessage::Complete { rows_affected })) => match self.state {
                    StreamState::Streaming => {
                        self.rows_affected = Some(rows_affected);
                        self.state = StreamState::AfterComplete;
                        continue;
                    }
                    _ => return Some(Err(self.multi_statement())),
                },
                Some(Err(e)) => {
                    // Connection isn't idle yet (no ReadyForQuery observed):
                    // do NOT clear active_id here.
                    self.state = StreamState::Aborted;
                    return Some(Err(e));
                }
                None => {
                    // ReadyForQuery: the connection is genuinely idle now.
                    self.state = StreamState::Ended;
                    self.clear_active();
                    return None;
                }
            }
        }
    }

    fn multi_statement(&mut self) -> DataSourceError {
        self.state = StreamState::Aborted;
        DataSourceError::MultipleStatements
    }

    pub async fn take(&mut self, max: usize) -> Result<Vec<Row>, DataSourceError> {
        let mut rows = Vec::with_capacity(max.min(1024));
        for _ in 0..max {
            match self.next().await {
                Some(Ok(row)) => rows.push(row),
                Some(Err(e)) => return Err(e),
                None => break,
            }
        }
        Ok(rows)
    }

    /// Ensures the stream is fully drained (or was already aborted by an
    /// error) so that dropping it takes the synchronous permit-release path
    /// (`Drop`'s `Ended` fast path) instead of deferring to a background
    /// drain task. Call this before dropping a stream you're done with, on
    /// EVERY code path (success AND error) — an aborted stream still needs
    /// its remaining wire bytes discarded before the connection is reusable,
    /// and `next()` alone can't do that once `state` is `Aborted`: it
    /// short-circuits straight to `None` without ever touching `self.inner`
    /// again.
    pub async fn finish(&mut self) {
        use futures_util::StreamExt;

        if matches!(
            self.state,
            StreamState::Streaming | StreamState::AfterComplete
        ) {
            while self.next().await.is_some() {}
        }
        if matches!(self.state, StreamState::Aborted) {
            // Not fused: a closed connection keeps yielding Err forever, so
            // this break is load-bearing (without it, this spins at 100% CPU
            // and never returns, stalling the whole tokio runtime — see the
            // identical hazard/fix in postgres.rs's drain_abandoned).
            while let Some(item) = self.inner.next().await {
                if item.is_err() {
                    break;
                }
                // Defensive, see the identical comment in RowStream::next().
                tokio::task::coop::consume_budget().await;
            }
            self.state = StreamState::Ended;
            self.clear_active();
        }
    }

    fn clear_active(&self) {
        let _ = self.active_id.compare_exchange(
            self.query_id.0,
            0,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

impl Drop for RowStream {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let inner = std::mem::replace(
            &mut self.inner,
            Box::pin(futures_util::stream::empty::<
                Result<ResultMessage, DataSourceError>,
            >()),
        );
        if matches!(self.state, StreamState::Ended) {
            self.clear_active();
            return;
        }
        let allow_cancel = matches!(self.state, StreamState::Streaming);
        match self.abandon.take() {
            Some(ctx) => {
                if ctx
                    .try_spawn_drain(
                        inner,
                        self.query_id,
                        Arc::clone(&self.active_id),
                        permit,
                        allow_cancel,
                    )
                    .is_err()
                {
                    self.clear_active();
                }
            }
            None => self.clear_active(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use tokio::sync::Semaphore;

    fn test_row_stream(
        active_id: Arc<AtomicU64>,
        query_id: u64,
        messages: Vec<Result<ResultMessage, DataSourceError>>,
        state: StreamState,
    ) -> RowStream {
        let permit = std::sync::Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("fresh semaphore has a free permit");
        RowStream {
            inner: Box::pin(stream::iter(messages)),
            query_id: QueryId(query_id),
            columns: None,
            rows_affected: None,
            active_id,
            permit: Some(permit),
            abandon: None,
            state,
        }
    }

    // Adversarial repro for the "left query still marked active forever" case
    // called out in the design: a caller that drops the stream mid-iteration
    // (never observes Complete/Err/None) must still see active_id cleared.
    #[tokio::test]
    async fn drop_mid_iteration_without_draining_clears_active_id() {
        let active_id = Arc::new(AtomicU64::new(7));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            7,
            vec![
                Ok(ResultMessage::Columns(Arc::from(vec!["a".to_string()]))),
                Ok(ResultMessage::Row(vec![Some("1".to_string())])),
                Ok(ResultMessage::Row(vec![Some("2".to_string())])),
                Ok(ResultMessage::Row(vec![Some("3".to_string())])),
            ],
            StreamState::Streaming,
        );

        let first = rs.next().await;
        assert!(matches!(first, Some(Ok(_))));
        assert_eq!(active_id.load(Ordering::Acquire), 7);

        drop(rs);

        assert_eq!(
            active_id.load(Ordering::Acquire),
            0,
            "dropping a RowStream that never reached Complete/Err/None must still clear active_id"
        );
    }

    #[tokio::test]
    async fn reaching_complete_clears_active_id_and_ends_the_stream() {
        let active_id = Arc::new(AtomicU64::new(3));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            3,
            vec![
                Ok(ResultMessage::Columns(Arc::from(vec!["a".to_string()]))),
                Ok(ResultMessage::Complete { rows_affected: 5 }),
            ],
            StreamState::Streaming,
        );

        assert!(rs.next().await.is_none());
        assert_eq!(rs.rows_affected(), Some(5));
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn stream_error_clears_active_id_and_is_returned_to_caller() {
        let active_id = Arc::new(AtomicU64::new(9));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            9,
            vec![Err(DataSourceError::Cancelled)],
            StreamState::Streaming,
        );

        let result = rs.next().await;
        assert!(matches!(result, Some(Err(DataSourceError::Cancelled))));
        // A stream error means the connection isn't confirmed idle yet, so
        // active_id is intentionally NOT cleared by next() itself here —
        // Drop (synchronous fallback path, abandon: None) clears it.
        drop(rs);
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    // clear_active must be a no-op if active_id has already moved on to a
    // newer query (compare_exchange guards this) — otherwise a slow-to-drop
    // stale stream could clobber a query that started after it.
    #[tokio::test]
    async fn clear_active_does_not_clobber_a_newer_query_id() {
        let active_id = Arc::new(AtomicU64::new(42));
        let rs = test_row_stream(Arc::clone(&active_id), 1, vec![], StreamState::Streaming);

        drop(rs);

        assert_eq!(
            active_id.load(Ordering::Acquire),
            42,
            "a stale stream's Drop must not clear an active_id that has since moved to a newer query"
        );
    }

    // A genuine two-statement batch: the first statement's full result
    // (Columns, Row, Complete) followed by a second statement's
    // RowDescription arriving on the same simple_query_raw stream. Draining
    // the first row and then hitting Complete must not silently end the
    // stream (that would drop the second statement's results without any
    // signal) — the next poll must surface MultipleStatements.
    #[tokio::test]
    async fn columns_row_complete_then_columns_reports_multiple_statements() {
        let active_id = Arc::new(AtomicU64::new(1));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            1,
            vec![
                Ok(ResultMessage::Columns(Arc::from(vec!["a".to_string()]))),
                Ok(ResultMessage::Row(vec![Some("1".to_string())])),
                Ok(ResultMessage::Complete { rows_affected: 1 }),
                Ok(ResultMessage::Columns(Arc::from(vec!["b".to_string()]))),
            ],
            StreamState::Streaming,
        );

        let first = rs.next().await;
        assert!(
            matches!(first, Some(Ok(_))),
            "expected the single row from the first statement, got {first:?}"
        );

        let second = rs.next().await;
        assert!(
            matches!(second, Some(Err(DataSourceError::MultipleStatements))),
            "a RowDescription arriving after the first statement's Complete must be reported as \
             MultipleStatements, not silently ended, got {second:?}"
        );
    }

    // Two zero-row/DDL-style statements back to back (e.g. two CREATE
    // TABLEs): no Columns/Row messages at all, just two Completes. This must
    // be distinguished from the single-statement zero-row case below. Since
    // the first Complete only flips internal state (AfterComplete) and
    // loops rather than returning to the caller, a single next() call
    // drains straight through to the second Complete and must surface
    // MultipleStatements from that very first call — not None.
    #[tokio::test]
    async fn two_completes_in_a_row_reports_multiple_statements() {
        let active_id = Arc::new(AtomicU64::new(2));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            2,
            vec![
                Ok(ResultMessage::Complete { rows_affected: 0 }),
                Ok(ResultMessage::Complete { rows_affected: 0 }),
            ],
            StreamState::Streaming,
        );

        let first = rs.next().await;
        assert!(
            matches!(first, Some(Err(DataSourceError::MultipleStatements))),
            "a second Complete arriving right after the first must be reported as \
             MultipleStatements, got {first:?}"
        );
        assert_eq!(rs.state, StreamState::Aborted);
    }

    // Pinned regression test: a single statement that affects zero rows
    // (e.g. `UPDATE ... WHERE false`) produces exactly one Complete and then
    // the underlying stream ends (ReadyForQuery). This must reach
    // StreamState::Ended cleanly and must NOT be misclassified as
    // MultipleStatements just because there were no Row messages to
    // distinguish it from the two-Completes case above.
    #[tokio::test]
    async fn single_complete_then_stream_end_reaches_ended_not_multiple_statements() {
        let active_id = Arc::new(AtomicU64::new(3));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            3,
            vec![Ok(ResultMessage::Complete { rows_affected: 0 })],
            StreamState::Streaming,
        );

        let first = rs.next().await;
        assert!(
            first.is_none(),
            "single zero-row statement should end with None, got {first:?}"
        );
        assert_eq!(rs.state, StreamState::Ended);

        let second = rs.next().await;
        assert!(
            second.is_none(),
            "polling an already-Ended stream must keep returning None, got {second:?}"
        );
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    // Once MultipleStatements has been surfaced, the stream is Aborted:
    // further polls must not loop, panic, or re-return the error — they
    // must just return None.
    #[tokio::test]
    async fn after_multiple_statements_error_state_is_aborted_and_next_returns_none() {
        let active_id = Arc::new(AtomicU64::new(4));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            4,
            vec![
                Ok(ResultMessage::Complete { rows_affected: 0 }),
                Ok(ResultMessage::Complete { rows_affected: 0 }),
            ],
            StreamState::Streaming,
        );

        let err = rs.next().await;
        assert!(matches!(
            err,
            Some(Err(DataSourceError::MultipleStatements))
        ));
        assert_eq!(rs.state, StreamState::Aborted);

        let after = rs.next().await;
        assert!(
            after.is_none(),
            "polling an Aborted stream must return None, not loop or re-return the error, got {after:?}"
        );
    }

    // finish() must drain a Streaming stream all the way to Ended (and clear
    // active_id) even when the caller stopped short of Complete/None itself
    // -- this is the success-path half of the drain-before-drop invariant
    // that fetch_page relies on.
    #[tokio::test]
    async fn finish_drains_remaining_messages_from_streaming_and_reaches_ended() {
        let active_id = Arc::new(AtomicU64::new(11));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            11,
            vec![
                Ok(ResultMessage::Columns(Arc::from(vec!["a".to_string()]))),
                Ok(ResultMessage::Row(vec![Some("1".to_string())])),
                Ok(ResultMessage::Complete { rows_affected: 1 }),
            ],
            StreamState::Streaming,
        );

        rs.finish().await;

        assert_eq!(rs.state, StreamState::Ended);
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    // AfterComplete with nothing left on the wire (the common single-
    // statement case): finish() must reach Ended via the plain `next()`
    // drain loop, without ever falling into the Aborted branch.
    #[tokio::test]
    async fn finish_from_after_complete_with_no_more_messages_reaches_ended() {
        let active_id = Arc::new(AtomicU64::new(12));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            12,
            vec![],
            StreamState::AfterComplete,
        );

        rs.finish().await;

        assert_eq!(rs.state, StreamState::Ended);
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    // The specific case the doc comment on finish() calls out: once state is
    // Aborted, next() short-circuits straight to None without ever touching
    // self.inner again, so finish() must fall back to draining self.inner
    // directly to discard the remaining wire bytes and clear active_id. This
    // is the error-path half of the drain-before-drop invariant (e.g. a
    // query that failed mid-stream, like `SELECT 1/0`).
    #[tokio::test]
    async fn finish_from_aborted_drains_remaining_wire_bytes_and_reaches_ended() {
        let active_id = Arc::new(AtomicU64::new(13));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            13,
            vec![
                Ok(ResultMessage::Row(vec![Some("leftover".to_string())])),
                Ok(ResultMessage::Complete { rows_affected: 0 }),
            ],
            StreamState::Aborted,
        );

        rs.finish().await;

        assert_eq!(rs.state, StreamState::Ended);
        assert_eq!(
            active_id.load(Ordering::Acquire),
            0,
            "finish() must clear active_id even when starting from Aborted, since next() alone \
             short-circuits to None without ever touching self.inner again once state is Aborted"
        );
    }

    // Calling finish() on an already-Ended stream (e.g. a caller that fully
    // drained via next() itself before calling finish() defensively) must be
    // a no-op: neither branch should fire, and active_id must not be
    // clobbered by a stale compare_exchange.
    #[tokio::test]
    async fn finish_on_an_already_ended_stream_is_a_no_op() {
        let active_id = Arc::new(AtomicU64::new(14));
        let mut rs = test_row_stream(Arc::clone(&active_id), 14, vec![], StreamState::Ended);

        rs.finish().await;

        assert_eq!(rs.state, StreamState::Ended);
        assert_eq!(
            active_id.load(Ordering::Acquire),
            14,
            "finish() on an already-Ended stream must not touch active_id"
        );
    }

    // Regression test for the infinite-loop bug: once state is Aborted, a
    // dead connection's underlying stream is not fused and yields Err on
    // every single poll forever (never Pending, never None). finish()'s
    // Aborted-branch drain loop must break on the first Err instead of
    // looping on `.is_some()` (which is true for Some(Err) too), or this
    // spins at 100% CPU and never returns. The whole test is wrapped in a
    // timeout so a regression fails the suite instead of hanging it.
    #[tokio::test]
    async fn finish_from_aborted_breaks_on_a_stream_that_yields_err_forever() {
        let active_id = Arc::new(AtomicU64::new(20));
        let permit = std::sync::Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("fresh semaphore has a free permit");
        let mut rs = RowStream {
            inner: Box::pin(stream::repeat_with(|| Err(DataSourceError::Cancelled))),
            query_id: QueryId(20),
            columns: None,
            rows_affected: None,
            active_id: Arc::clone(&active_id),
            permit: Some(permit),
            abandon: None,
            state: StreamState::Aborted,
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), rs.finish())
            .await
            .expect(
                "finish() must return promptly even when the inner stream yields Err on every \
                 poll forever, not spin at 100% CPU indefinitely",
            );

        assert_eq!(rs.state, StreamState::Ended);
        assert_eq!(active_id.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn take_stops_early_on_none_and_collects_available_rows() {
        let active_id = Arc::new(AtomicU64::new(1));
        let mut rs = test_row_stream(
            Arc::clone(&active_id),
            1,
            vec![
                Ok(ResultMessage::Columns(Arc::from(vec!["a".to_string()]))),
                Ok(ResultMessage::Row(vec![Some("1".to_string())])),
                Ok(ResultMessage::Complete { rows_affected: 1 }),
            ],
            StreamState::Streaming,
        );

        let rows = rs.take(50).await.expect("no error in this fixture");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0), Some("1"));
    }
}
