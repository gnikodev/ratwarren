pub const PAGE_SIZE: usize = 50;
pub const FETCH_LIMIT: usize = PAGE_SIZE + 1;

pub fn has_next_page(fetched_len: usize) -> bool {
    fetched_len > PAGE_SIZE
}

pub fn split_page<T>(mut fetched: Vec<T>) -> (Vec<T>, bool) {
    debug_assert!(fetched.len() <= FETCH_LIMIT);
    let has_next = has_next_page(fetched.len());
    fetched.truncate(PAGE_SIZE);
    (fetched, has_next)
}

pub fn next_offset(offset: u64) -> u64 {
    offset + PAGE_SIZE as u64
}

pub fn prev_offset(offset: u64) -> u64 {
    offset.saturating_sub(PAGE_SIZE as u64)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub has_next: bool,
}

impl Page {
    pub fn from_fetched(columns: Vec<String>, fetched: Vec<crate::datasource::Row>) -> Self {
        let (rows, has_next) = split_page(fetched);
        Self {
            columns,
            rows: rows.into_iter().map(|r| r.into_values()).collect(),
            has_next,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridContent {
    Rows(Page),
    NoResultSet { rows_affected: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_next_page_is_false_at_exactly_page_size() {
        assert!(!has_next_page(0));
        assert!(!has_next_page(PAGE_SIZE));
    }

    #[test]
    fn has_next_page_is_true_one_past_page_size() {
        assert!(has_next_page(PAGE_SIZE + 1));
    }

    #[test]
    fn split_page_truncates_and_reports_has_next() {
        let fetched: Vec<u32> = (0..(FETCH_LIMIT as u32)).collect();
        let (rows, has_next) = split_page(fetched);
        assert_eq!(rows.len(), PAGE_SIZE);
        assert!(has_next);
    }

    #[test]
    fn split_page_with_exactly_page_size_has_no_next() {
        let fetched: Vec<u32> = (0..(PAGE_SIZE as u32)).collect();
        let (rows, has_next) = split_page(fetched);
        assert_eq!(rows.len(), PAGE_SIZE);
        assert!(!has_next);
    }

    #[test]
    fn split_page_with_zero_rows() {
        let fetched: Vec<u32> = Vec::new();
        let (rows, has_next) = split_page(fetched);
        assert!(rows.is_empty());
        assert!(!has_next);
    }

    #[test]
    fn next_offset_advances_by_page_size() {
        assert_eq!(next_offset(0), PAGE_SIZE as u64);
        assert_eq!(next_offset(100), 100 + PAGE_SIZE as u64);
    }

    #[test]
    fn prev_offset_saturates_at_zero() {
        assert_eq!(prev_offset(0), 0);
        assert_eq!(prev_offset(30), 0);
        assert_eq!(prev_offset(PAGE_SIZE as u64), 0);
        assert_eq!(prev_offset(2 * PAGE_SIZE as u64), PAGE_SIZE as u64);
    }
}
