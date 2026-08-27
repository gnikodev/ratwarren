pub mod buffer;
pub mod split;

pub use buffer::{Motion, Position, TextBuffer};
pub use split::{ByteSpan, Split, SplitError, SplitErrorKind, Statement, split};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTarget {
    Cursor,
    Selection,
    Buffer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUnit {
    pub sql: String,
    pub span: ByteSpan,
    pub start: Position,
}

/// Splits `buffer`'s text and resolves the statement(s) implied by `target`.
///
/// Error propagation policy: `Buffer` always refuses to run if the buffer
/// has any tokenizer error anywhere (never run a prefix of a tail that
/// doesn't tokenize). `Cursor` and `Selection` refuse when the position (or
/// selection range) is inside the unrunnable trailing region -- no statement
/// is ever emitted from inside that region. "Nothing to run" (e.g. no
/// selection) is `Ok(vec![])`, not an error.
pub fn plan_run(buffer: &TextBuffer, target: RunTarget) -> Result<Vec<RunUnit>, SplitError> {
    let text = buffer.text();
    let split = split::split(&text);

    if let Some(err) = split.error() {
        let should_propagate = match target {
            RunTarget::Buffer => true,
            RunTarget::Cursor => {
                let offset = buffer.offset_of(buffer.cursor());
                // Every statement split() emits alongside an error is
                // ';'-terminated and ends at or before err.span.start, so a
                // statement ending EXACTLY at offset is complete and is what
                // statement_at's rule 1 resolves to (the byte right after a
                // ';' belongs to the statement that just ended). Anything
                // else at/past the poison start is inside the broken tail.
                offset >= err.span.start
                    && !split
                        .statement_at(offset)
                        .is_some_and(|s| s.span.end == offset)
            }
            RunTarget::Selection => match buffer.selection() {
                Some((start, end)) => {
                    let start_off = buffer.offset_of(start);
                    let end_off = buffer.offset_of(end);
                    start_off < err.span.end && err.span.start < end_off
                }
                None => false,
            },
        };
        if should_propagate {
            return Err(err.clone());
        }
    }

    let statements: Vec<&Statement> = match target {
        RunTarget::Buffer => split.statements().iter().collect(),
        RunTarget::Cursor => {
            let offset = buffer.offset_of(buffer.cursor());
            split.statement_at(offset).into_iter().collect()
        }
        RunTarget::Selection => match buffer.selection() {
            Some((start, end)) => {
                let start_off = buffer.offset_of(start);
                let end_off = buffer.offset_of(end);
                split.statements_in(start_off, end_off).iter().collect()
            }
            None => Vec::new(),
        },
    };

    Ok(statements
        .into_iter()
        .map(|s| RunUnit {
            sql: s.sql_span.slice(&text).to_string(),
            span: s.sql_span,
            start: buffer.position_of(s.sql_span.start),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_target_with_clean_buffer_returns_all_statements() {
        let buf = TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 3;");
        let units = plan_run(&buf, RunTarget::Buffer).expect("no tokenizer error");
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].sql, "SELECT 1");
        assert_eq!(units[1].sql, "SELECT 2");
        assert_eq!(units[2].sql, "SELECT 3");
    }

    #[test]
    fn buffer_target_refuses_on_any_tokenizer_error_even_near_the_end() {
        // Design: `Buffer` always refuses if the buffer has ANY tokenizer
        // error anywhere, even if most of the buffer tokenizes fine and the
        // error is near the very end.
        let buf = TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 'unterminated");
        let result = plan_run(&buf, RunTarget::Buffer);
        assert!(result.is_err());
    }

    #[test]
    fn cursor_target_before_error_elsewhere_still_succeeds() {
        let mut buf = TextBuffer::from_text("SELECT 1; SELECT 'unterminated");
        buf.move_to(Position { line: 0, col: 2 }, Motion::Move); // inside "SELECT 1"
        let units = plan_run(&buf, RunTarget::Cursor).expect("cursor is before the error region");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].sql, "SELECT 1");
    }

    #[test]
    fn cursor_target_past_the_error_region_start_fails() {
        let text = "SELECT 1; SELECT 'unterminated";
        let mut buf = TextBuffer::from_text(text);
        let error_start = split::split(text).error().unwrap().span.start;
        // One byte PAST the boundary -- inside the broken tail itself, not on
        // the terminated statement that precedes it.
        buf.move_to(buf.position_of(error_start + 1), Motion::Move);
        let result = plan_run(&buf, RunTarget::Cursor);
        assert!(result.is_err());
    }

    #[test]
    fn cursor_target_exactly_at_error_region_start_still_runs_the_preceding_statement() {
        // Boundary case: err.span.start is exactly the end of the last
        // terminated statement's span (the byte right after its ';'). A
        // cursor sitting there resolves, via statement_at's rule 1, to that
        // completed statement -- it must NOT be refused just because it's
        // numerically at err.span.start.
        let text = "SELECT 1; SELECT 'unterminated";
        let mut buf = TextBuffer::from_text(text);
        let error_start = split::split(text).error().unwrap().span.start;
        buf.move_to(buf.position_of(error_start), Motion::Move);
        let units = plan_run(&buf, RunTarget::Cursor)
            .expect("cursor at the boundary resolves to the preceding terminated statement");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].sql, "SELECT 1");
    }

    #[test]
    fn selection_target_not_overlapping_error_succeeds() {
        let mut buf = TextBuffer::from_text("SELECT 1; SELECT 'unterminated");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 8 }, Motion::Extend); // "SELECT 1"
        let units = plan_run(&buf, RunTarget::Selection).expect("selection doesn't overlap error");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].sql, "SELECT 1");
    }

    #[test]
    fn selection_target_overlapping_error_fails() {
        let text = "SELECT 1; SELECT 'unterminated";
        let mut buf = TextBuffer::from_text(text);
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_buffer_end(Motion::Extend);
        let result = plan_run(&buf, RunTarget::Selection);
        assert!(result.is_err());
    }

    #[test]
    fn cursor_target_on_unclosed_paren_tail_refuses_but_prior_statement_still_runs() {
        // Bug 1 repro: an unclosed `(` suppresses every `;` after it, merging
        // "SELECT count(", "SELECT 2;" and "DELETE FROM audit_log;" into one
        // unrunnable blob. Cursor positioned inside that blob must refuse;
        // cursor positioned on the earlier, properly terminated "SELECT 1"
        // must still succeed.
        let text = "SELECT 1;\nSELECT count(\nSELECT 2;\nDELETE FROM audit_log;";
        let mut buf = TextBuffer::from_text(text);
        buf.move_to(Position { line: 2, col: 3 }, Motion::Move); // inside "SELECT 2"
        let err = plan_run(&buf, RunTarget::Cursor).expect_err("cursor is inside the merged tail");
        assert_eq!(err.kind, SplitErrorKind::UnclosedParen);

        buf.move_to(Position { line: 0, col: 3 }, Motion::Move); // inside "SELECT 1"
        let units = plan_run(&buf, RunTarget::Cursor).expect("cursor is on a terminated statement");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].sql, "SELECT 1");
    }

    #[test]
    fn cursor_target_never_runs_a_prefix_truncated_by_an_unterminated_comment() {
        // Bug 2 repro: "UPDATE t SET x = 1" LOOKS like a complete, runnable
        // statement, but it's actually the start of an unterminated block
        // comment -- the whole tail is poisoned, and this prefix must never
        // be resolved as a runnable statement regardless of cursor position.
        let text = "UPDATE t SET x = 1 /* WHERE id = 5";
        let mut buf = TextBuffer::from_text(text);
        for col in [0, 5, 15, text.len()] {
            buf.move_to(Position { line: 0, col }, Motion::Move);
            let result = plan_run(&buf, RunTarget::Cursor);
            assert!(result.is_err(), "cursor at col {col} should refuse to run");
        }
    }

    #[test]
    fn cursor_target_resolving_to_nothing_is_ok_empty_not_error() {
        // Cursor sitting in leading whitespace before an empty leading `;`
        // resolves to no statement at all -- "nothing to run" is `Ok(vec![])`.
        let mut buf = TextBuffer::from_text("  ; SELECT 1;");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        let units = plan_run(&buf, RunTarget::Cursor).expect("no error, just nothing to run");
        assert!(units.is_empty());
    }

    #[test]
    fn selection_target_resolving_to_nothing_is_ok_empty_not_error() {
        let mut buf = TextBuffer::from_text("SELECT 1;");
        buf.move_to(Position { line: 0, col: 3 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 3 }, Motion::Extend); // zero-width
        assert_eq!(buf.selection(), None);
        let units = plan_run(&buf, RunTarget::Selection).expect("no error");
        assert!(units.is_empty());
    }

    #[test]
    fn run_unit_sql_is_trimmed_but_keeps_comments_and_drops_semicolon() {
        let buf = TextBuffer::from_text("  /* hint */ SELECT 1  ;  SELECT 2;");
        let units = plan_run(&buf, RunTarget::Buffer).unwrap();
        assert_eq!(units[0].sql, "/* hint */ SELECT 1");
        assert!(!units[0].sql.contains(';'));
        assert_eq!(units[1].sql, "SELECT 2");
    }

    #[test]
    fn run_unit_start_maps_back_to_the_correct_line_and_column() {
        let buf = TextBuffer::from_text("SELECT 1;\n  SELECT 2;");
        let units = plan_run(&buf, RunTarget::Buffer).unwrap();
        assert_eq!(units[0].start, Position { line: 0, col: 0 });
        assert_eq!(units[1].start, Position { line: 1, col: 2 });
    }

    #[test]
    fn select_all_after_a_stale_goal_col_then_shift_down_keeps_the_full_selection() {
        // Bug repro: a prior vertical move (Down) sets goal_col. select_all()
        // must reset it -- otherwise a following Shift+Down (an Extend
        // vertical motion) uses the stale goal_col instead of the correct
        // column for the new cursor line, silently shrinking the selection
        // instead of being a no-op on the already-last line.
        let text = "SELECT 1;\nSELECT 2;\nDELETE FROM audit_log;";
        let mut buf = TextBuffer::from_text(text);
        buf.move_down(Motion::Move); // sets goal_col
        buf.select_all();
        buf.move_down(Motion::Extend); // no-op: cursor is already on the last line

        let units = plan_run(&buf, RunTarget::Selection).expect("no tokenizer error");
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].sql, "SELECT 1");
        assert_eq!(units[1].sql, "SELECT 2");
        assert_eq!(units[2].sql, "DELETE FROM audit_log");
    }
}
