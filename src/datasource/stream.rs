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
        DataSourceError::MultipleStatements { method: "execute" }
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
