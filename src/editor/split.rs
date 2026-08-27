use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, Whitespace};

/// Bare whitespace (space/tab/newline) is trimmed from `sql_span`; comments
/// are not (dropping a leading/interior/trailing comment could silently
/// change what gets sent, e.g. optimizer hint comments).
fn is_bare_whitespace(token: &Token) -> bool {
    matches!(
        token,
        Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
    )
}

/// Byte range into the text that was split. `end` is EXCLUSIVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }

    pub fn slice(self, text: &str) -> &str {
        debug_assert!(text.is_char_boundary(self.start));
        debug_assert!(text.is_char_boundary(self.end));
        &text[self.start..self.end]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Full segment: from just past the previous terminator (or buffer
    /// start) to just past this statement's terminating `;` (or EOF).
    /// Contiguous with neighboring statements' spans --
    /// `span[i].end == span[i+1].start`. Used for cursor-position lookup.
    pub span: ByteSpan,
    /// The executable range: first non-whitespace token's start .. last
    /// significant token's end. Excludes the terminating `;` and
    /// surrounding whitespace; KEEPS leading and interior comments
    /// (dropping a leading comment would silently change what gets sent,
    /// e.g. optimizer hint comments). Never empty. Used for extraction and
    /// selection-overlap.
    pub sql_span: ByteSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitErrorKind {
    /// sqlparser's tokenizer stopped before consuming the whole buffer
    /// (unterminated string / dollar-quote / block comment).
    Tokenize,
    /// Every token was consumed, but a `(` was never closed -- so every `;`
    /// after it was suppressed and the tail is a merged blob, not a
    /// statement.
    UnclosedParen,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SplitError {
    pub kind: SplitErrorKind,
    pub message: String,
    /// The UNRUNNABLE region: [start of the trailing segment that could not
    /// be completed, text.len()) -- everything after the last ';' that
    /// actually terminated a statement. Deliberately NOT the tokenizer's
    /// stopping byte: a tokenizer error truncates the tail mid-statement,
    /// and the visible prefix can look complete on its own (e.g.
    /// "UPDATE t SET x = 1 /* WHERE id = 5" would otherwise resolve to a
    /// runnable "UPDATE t SET x = 1"). The whole tail is poisoned, not just
    /// the part from the error byte onward.
    pub span: ByteSpan,
}

#[derive(Debug, Clone, Default)]
pub struct Split {
    statements: Vec<Statement>,
    error: Option<SplitError>,
}

/// Byte offset where each line begins, computed by scanning for `'\n'`
/// bytes only. Used to convert sqlparser's 1-based line/character-column
/// `Location`s into byte offsets.
struct LineIndex {
    starts: Vec<usize>,
    /// Memo of the last resolved (line_idx, column, byte offset). sqlparser's
    /// token spans tile the input in increasing order (token[i].span.end ==
    /// token[i+1].span.start), so consecutive lookups almost always land on
    /// this line at or past this column -- advancing from the memo makes the
    /// char walk amortized O(1) per token instead of O(column). A lookup
    /// BEFORE the memo falls back to the line-start scan, so correctness
    /// never depends on the tiling property -- only the speed does.
    memo_line: usize,
    memo_col: usize,
    memo_offset: usize,
}

impl LineIndex {
    fn build(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self {
            starts,
            memo_line: 0,
            memo_col: 1,
            memo_offset: 0,
        }
    }

    fn line_end(&self, text: &str, line_idx: usize) -> usize {
        self.starts
            .get(line_idx + 1)
            .map(|&s| s - 1)
            .unwrap_or(text.len())
    }

    /// Converts a 1-based `(line, column)` location into a byte offset.
    /// `line == 0` / `column == 0` (sqlparser's "unknown location" sentinel)
    /// and out-of-range values are clamped to the nearest valid boundary
    /// rather than panicking.
    ///
    /// `&mut self`: this advances the lookup memo (see the `memo_*` fields)
    /// as a side effect.
    fn byte_offset_at(&mut self, text: &str, line: u64, column: u64) -> usize {
        let line = line.max(1) as usize;
        let column = column.max(1) as usize;
        let line_idx = (line - 1).min(self.starts.len() - 1);
        let line_end = self.line_end(text, line_idx);

        let (mut offset, mut remaining) = if line_idx == self.memo_line && column >= self.memo_col {
            (self.memo_offset, column - self.memo_col)
        } else {
            (self.starts[line_idx], column - 1)
        };

        if remaining > 0 {
            for ch in text[offset..line_end].chars() {
                if remaining == 0 {
                    break;
                }
                offset += ch.len_utf8();
                remaining -= 1;
            }
        }

        self.memo_line = line_idx;
        self.memo_col = column;
        self.memo_offset = offset;
        offset
    }
}

pub fn split(text: &str) -> Split {
    let mut line_index = LineIndex::build(text);
    let mut buf: Vec<TokenWithSpan> = Vec::new();
    let tokenize_result =
        Tokenizer::new(&PostgreSqlDialect {}, text).tokenize_with_location_into_buf(&mut buf);

    let mut statements = Vec::new();
    let mut depth: u32 = 0;
    let mut seg_start = 0usize;
    let mut seg_has_significant = false;
    let mut seg_first: Option<usize> = None;
    let mut seg_last_end: Option<usize> = None;
    let mut last_tok_end = 0usize;

    for tok in &buf {
        let tok_start = line_index.byte_offset_at(text, tok.span.start.line, tok.span.start.column);
        let tok_end = line_index.byte_offset_at(text, tok.span.end.line, tok.span.end.column);
        last_tok_end = tok_end;

        match &tok.token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }

        if matches!(tok.token, Token::SemiColon) && depth == 0 {
            if seg_has_significant {
                statements.push(Statement {
                    span: ByteSpan {
                        start: seg_start,
                        end: tok_end,
                    },
                    sql_span: ByteSpan {
                        start: seg_first.expect("seg_has_significant implies seg_first is set"),
                        end: seg_last_end.expect("seg_has_significant implies seg_last_end is set"),
                    },
                });
            }
            seg_start = tok_end;
            seg_has_significant = false;
            seg_first = None;
            seg_last_end = None;
            continue;
        }

        if !matches!(tok.token, Token::Whitespace(_)) {
            seg_has_significant = true;
        }
        if !is_bare_whitespace(&tok.token) {
            if seg_first.is_none() {
                seg_first = Some(tok_start);
            }
            seg_last_end = Some(tok_end);
        }
    }

    // The trailing segment is a real statement ONLY if the split reached a
    // trustworthy end state. A tokenizer error truncates it mid-statement; an
    // unclosed '(' means every ';' after that paren was suppressed, so the
    // "segment" is actually several statements merged into one blob --
    // simple_query would run all of them.
    let cause = match tokenize_result {
        Err(e) => Some((SplitErrorKind::Tokenize, e.to_string())),
        Ok(()) if depth > 0 => Some((
            SplitErrorKind::UnclosedParen,
            "unclosed `(`: the trailing statement is incomplete".to_string(),
        )),
        Ok(()) => None,
    };

    let error = match cause {
        None => {
            if seg_has_significant {
                statements.push(Statement {
                    span: ByteSpan {
                        start: seg_start,
                        end: last_tok_end,
                    },
                    sql_span: ByteSpan {
                        start: seg_first.expect("seg_has_significant implies seg_first is set"),
                        end: seg_last_end.expect("seg_has_significant implies seg_last_end is set"),
                    },
                });
            }
            None
        }
        Some((kind, message)) => Some(SplitError {
            kind,
            message,
            span: ByteSpan {
                start: seg_start,
                end: text.len(),
            },
        }),
    };

    Split { statements, error }
}

impl Split {
    pub fn statements(&self) -> &[Statement] {
        &self.statements
    }

    pub fn error(&self) -> Option<&SplitError> {
        self.error.as_ref()
    }

    /// Resolution order matters:
    /// 1. A statement whose `span.end == offset` wins first (cursor sitting
    ///    immediately after the `;` resolves to the statement that just
    ///    ended, not the next one).
    /// 2. Else a statement whose `span` contains `offset`.
    /// 3. Else the nearest preceding statement (cursor in trailing
    ///    whitespace/comments after the last statement).
    /// 4. Else `None`.
    pub fn statement_at(&self, offset: usize) -> Option<&Statement> {
        if let Some(s) = self.statements.iter().find(|s| s.span.end == offset) {
            return Some(s);
        }
        if let Some(s) = self.statements.iter().find(|s| s.span.contains(offset)) {
            return Some(s);
        }
        self.statements.iter().rev().find(|s| s.span.end <= offset)
    }

    /// Every statement whose `sql_span` overlaps `[start, end)`, normalizing
    /// `start`/`end` if reversed. A zero-width range matches nothing.
    pub fn statements_in(&self, start: usize, end: usize) -> &[Statement] {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if start == end {
            return &[];
        }
        let overlaps = |s: &Statement| s.sql_span.start < end && start < s.sql_span.end;
        let Some(first) = self.statements.iter().position(overlaps) else {
            return &[];
        };
        let last = self.statements.iter().rposition(overlaps).unwrap_or(first);
        &self.statements[first..=last]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canary: this whole module depends on `Span::end` being EXCLUSIVE and
    /// on columns being counted in CHARACTERS, not bytes. Neither is
    /// actually guaranteed by sqlparser's own (incorrect) doc comment on
    /// `Span::end`, which claims "inclusive". If a future sqlparser upgrade
    /// changes this, this test must fail loudly before anything downstream
    /// silently starts producing off-by-one spans.
    #[test]
    fn sqlparser_span_end_is_exclusive_and_char_counted() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::tokenizer::{Location, Tokenizer};

        let mut buf = Vec::new();
        Tokenizer::new(&PostgreSqlDialect {}, "SELECT")
            .tokenize_with_location_into_buf(&mut buf)
            .unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].span.start, Location::new(1, 1));
        assert_eq!(buf[0].span.end, Location::new(1, 7));

        let mut buf = Vec::new();
        Tokenizer::new(&PostgreSqlDialect {}, "SELECT '🦀', 1;")
            .tokenize_with_location_into_buf(&mut buf)
            .unwrap();
        let string_tok = &buf[2];
        assert_eq!(string_tok.span.start, Location::new(1, 8));
        assert_eq!(string_tok.span.end, Location::new(1, 11));
    }

    #[test]
    fn simple_statements_split_on_semicolon() {
        let s = split("SELECT 1; SELECT 2;");
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert_eq!(
            s.statements()[0].sql_span.slice("SELECT 1; SELECT 2;"),
            "SELECT 1"
        );
        assert_eq!(
            s.statements()[1].sql_span.slice("SELECT 1; SELECT 2;"),
            "SELECT 2"
        );
    }

    #[test]
    fn semicolon_inside_parens_does_not_split() {
        let text = "SELECT (1; 2);";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT (1; 2)");
    }

    #[test]
    fn statement_at_prefers_boundary_match_over_containment() {
        let text = "SELECT 1; SELECT 2;";
        let s = split(text);
        // Contiguous spans mean the byte right after statement 0's ';' is
        // SIMULTANEOUSLY statement 0's span.end AND (since spans are
        // contiguous, span[i].end == span[i+1].start) contained by
        // statement 1's span. Rule 1 (exact span.end match) must win here,
        // not rule 2 (containment) -- this is the off-by-one this project's
        // review process keeps catching.
        let first_end = s.statements()[0].span.end;
        assert_eq!(s.statements()[1].span.start, first_end);
        assert!(s.statements()[1].span.contains(first_end));

        let resolved = s.statement_at(first_end).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);

        // Inside statement 1's span (past the boundary) must resolve via
        // rule 2 to statement 1.
        let inside_second = s.statements()[1].span.start + 1;
        let resolved = s.statement_at(inside_second).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[1].sql_span);
    }

    #[test]
    fn statement_at_after_last_statement_falls_back_to_rule_three() {
        let text = "SELECT 1; -- trailing\n";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        let resolved = s.statement_at(text.len()).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    #[test]
    fn statement_at_before_any_statement_is_none() {
        let text = "  ; SELECT 1;";
        let s = split(text);
        assert_eq!(s.statement_at(0), None);
    }

    #[test]
    fn tokenizer_error_reports_good_prefix() {
        let text = "SELECT 1; SELECT 'unterminated";
        let s = split(text);
        // The trailing segment is truncated mid-statement by the tokenizer
        // error, so it can never be emitted as a runnable Statement -- only
        // the completed "SELECT 1" is. The error region covers everything
        // after "SELECT 1"'s terminating ';'.
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT 1");
        let err = s.error().expect("unterminated string should error");
        assert_eq!(err.kind, SplitErrorKind::Tokenize);
        assert_eq!(err.span.end, text.len());
        assert_eq!(err.span.start, s.statements()[0].span.end);
    }

    #[test]
    fn dollar_quoted_string_containing_semicolon_is_not_split() {
        let text = "SELECT $$a; b$$;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
    }

    #[test]
    fn comment_only_segment_is_not_emitted() {
        let text = "SELECT 1; -- trailing comment only\n";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
    }

    #[test]
    fn leading_comment_is_kept_in_sql_span() {
        let text = "/* hint */ SELECT 1;";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "/* hint */ SELECT 1"
        );
    }

    // ---- dollar-quoting -------------------------------------------------

    #[test]
    fn tagged_dollar_quote_with_semicolon_and_fake_dollar_signs_is_not_split() {
        let text = "SELECT $$a; b$$; SELECT $tag$c; d $$ e$tag$;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT $$a; b$$");
        assert_eq!(
            s.statements()[1].sql_span.slice(text),
            "SELECT $tag$c; d $$ e$tag$"
        );
    }

    #[test]
    fn tagged_dollar_quote_is_not_closed_by_a_different_tag_inside() {
        // The inner `$inner$` occurrences don't close the `$outer$...$outer$`
        // quote -- only a matching `$outer$` does. If this were mishandled,
        // the tokenizer would either error or the `;` after `$inner$ b`
        // would wrongly split the buffer into two statements.
        let text = "SELECT $outer$ a; $inner$ b $outer$;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT $outer$ a; $inner$ b $outer$"
        );
    }

    #[test]
    fn dollar_quote_spans_multiple_lines_with_embedded_semicolons() {
        let text = "SELECT $$line1;\nline2;\nline3$$;\nSELECT 2;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT $$line1;\nline2;\nline3$$"
        );
        assert_eq!(s.statements()[1].sql_span.slice(text), "SELECT 2");
    }

    #[test]
    fn dollar_quote_closes_at_first_matching_tag_not_nested() {
        // Postgres dollar-quoting doesn't nest: a `$tag$` occurrence inside
        // the body closes the string at the FIRST match, even if it "looks"
        // like the start of a nested quote of the same tag. The remainder
        // (`def$tag$`) is then tokenized as ordinary content (a single
        // identifier word, since `$` is a valid identifier character here),
        // and the buffer is still one statement (only one real `;` at depth
        // 0, at the very end).
        let text = "SELECT $tag$abc $tag$ def$tag$;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT $tag$abc $tag$ def$tag$"
        );
    }

    // ---- string / identifier literals containing `;` --------------------

    #[test]
    fn single_quoted_string_with_semicolon_is_not_split() {
        let text = "SELECT 'a;b';";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT 'a;b'");
    }

    #[test]
    fn escaped_string_literal_with_semicolon_and_escaped_quote_is_not_split() {
        let text = r"SELECT E'a\';b';";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), r"SELECT E'a\';b'");
    }

    #[test]
    fn doubled_single_quote_escape_with_semicolon_is_not_split() {
        let text = "SELECT 'it''s; here';";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT 'it''s; here'"
        );
    }

    #[test]
    fn double_quoted_identifier_with_semicolon_is_not_split() {
        let text = "SELECT \"we;ird\";";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT \"we;ird\"");
    }

    // ---- comments ---------------------------------------------------------

    #[test]
    fn line_comment_containing_semicolon_does_not_split_and_ends_at_newline() {
        let text = "SELECT 1; --comment; more text\nSELECT 2;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        // The second statement's sql_span absorbs the trailing same-line
        // comment as leading trivia (see the boundary-rule tests below), but
        // its own executable content still starts cleanly with `SELECT 2` on
        // the next line -- the comment doesn't leak `; more text` into it.
        assert!(s.statements()[1].sql_span.slice(text).ends_with("SELECT 2"));
        assert!(s.statements()[1].sql_span.slice(text).contains("SELECT 2"));
    }

    #[test]
    fn block_comment_with_semicolon_and_newline_does_not_split() {
        let text = "SELECT /* a;\nb */ 1;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT /* a;\nb */ 1"
        );
    }

    #[test]
    fn nested_block_comments_do_not_misfire() {
        // Postgres block comments nest; a naive "find the next `*/`"
        // implementation would close the comment too early (at the inner
        // `*/`) and then choke on stray ` still outer */` tokens. sqlparser's
        // tokenizer handles this itself; this test pins that our split logic
        // doesn't break given that token.
        let text = "SELECT /* outer /* inner */ still outer */ 1;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT /* outer /* inner */ still outer */ 1"
        );
    }

    #[test]
    fn leading_comment_before_statement_is_included_in_sql_span_not_trimmed() {
        let text = "-- lead\nSELECT 1;";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "-- lead\nSELECT 1");
    }

    #[test]
    fn comment_between_two_statements_becomes_leading_trivia_of_the_following_statement() {
        // Per the design: a trailing same-line/interior comment between one
        // statement's `;` and the next statement's first token is NOT
        // dropped, and is NOT attached to the statement that precedes it --
        // it's kept as leading trivia of the statement that FOLLOWS it.
        let text = "SELECT 1; -- mid\nSELECT 2;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT 1");
        assert_eq!(
            s.statements()[1].sql_span.slice(text),
            "-- mid\nSELECT 2",
            "trailing comment after `;` must become the next statement's leading trivia"
        );
    }

    // ---- multi-line statements --------------------------------------------

    #[test]
    fn single_statement_spans_many_lines_with_varied_indentation() {
        let text = "SELECT\n    a,\n        b,\n  c\nFROM\n    t;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "SELECT\n    a,\n        b,\n  c\nFROM\n    t"
        );
    }

    #[test]
    fn multiple_multiline_statements_have_correctly_bounded_spans() {
        let text = "SELECT\n  1,\n  2;\nSELECT\n  3,\n  4;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT\n  1,\n  2");
        assert_eq!(s.statements()[1].sql_span.slice(text), "SELECT\n  3,\n  4");
    }

    // ---- statement_at boundary cases ---------------------------------------

    #[test]
    fn statement_at_on_semicolon_char_itself_resolves_to_terminated_statement() {
        let text = "SELECT 1;";
        let s = split(text);
        let semi_offset = text.find(';').unwrap();
        let resolved = s.statement_at(semi_offset).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    #[test]
    fn statement_at_one_byte_after_semicolon_prefers_preceding_statement_at_every_boundary() {
        // The single most-emphasized rule in this module: the byte
        // immediately after a `;` resolves to the statement that just
        // ENDED, not the one that follows -- checked at both internal
        // boundaries of a 3-statement buffer, not just one.
        let text = "SELECT 1; SELECT 2; SELECT 3;";
        let s = split(text);
        assert_eq!(s.statements().len(), 3);

        let boundary_1_2 = s.statements()[0].span.end;
        assert_eq!(
            s.statement_at(boundary_1_2).unwrap().sql_span,
            s.statements()[0].sql_span
        );

        let boundary_2_3 = s.statements()[1].span.end;
        assert_eq!(
            s.statement_at(boundary_2_3).unwrap().sql_span,
            s.statements()[1].sql_span
        );
    }

    #[test]
    fn statement_at_in_whitespace_gap_past_the_boundary_resolves_to_following_statement() {
        let text = "SELECT 1;   SELECT 2;";
        let s = split(text);
        // One byte further into the gap than the exact post-semicolon
        // boundary: no longer an exact `span.end` match, so this falls
        // through to containment (rule 2), which is owned by statement 1
        // (spans are contiguous, so the gap belongs to the FOLLOWING
        // statement's span once we're past the exact boundary byte).
        let offset = s.statements()[0].span.end + 1;
        let resolved = s.statement_at(offset).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[1].sql_span);
    }

    #[test]
    fn statement_at_inside_leading_comment_resolves_to_following_statement() {
        let text = "-- lead\nSELECT 1;";
        let s = split(text);
        let offset = text.find("lead").unwrap();
        let resolved = s.statement_at(offset).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    #[test]
    fn statement_at_inside_trailing_same_line_comment_resolves_to_following_statement() {
        // The debatable case, documented: a comment on the same line right
        // after a `;` still resolves to the NEXT statement, not the one it
        // trails.
        let text = "SELECT 1; -- mid\nSELECT 2;";
        let s = split(text);
        let offset = text.find("mid").unwrap();
        let resolved = s.statement_at(offset).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[1].sql_span);
    }

    #[test]
    fn statement_at_in_trailing_whitespace_after_last_statement_resolves_via_rule_three() {
        let text = "SELECT 1;\n\n   ";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        let resolved = s.statement_at(text.len()).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    #[test]
    fn statement_at_inside_multiline_statement_body_resolves_regardless_of_line() {
        let text = "SELECT\n  1,\n  2\nFROM\n  t;";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        for needle in ["1,", "2", "FROM", "t"] {
            let offset = text.find(needle).unwrap();
            let resolved = s.statement_at(offset).unwrap();
            assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
        }
    }

    #[test]
    fn statement_at_inside_dollar_quoted_whitespace_look_alike_stays_in_its_statement() {
        // The dollar-quoted body contains a blank line that could be
        // mistaken for an inter-statement gap by anything not respecting
        // quoting; it must still resolve to the one statement.
        let text = "SELECT $$line1\n\nline2$$;\nSELECT 2;";
        let s = split(text);
        assert_eq!(s.statements().len(), 2);
        let offset = text.find("\n\n").unwrap() + 1;
        let resolved = s.statement_at(offset).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    #[test]
    fn statement_at_on_empty_buffer_is_none() {
        let s = split("");
        assert_eq!(s.statement_at(0), None);
    }

    #[test]
    fn statement_at_on_comments_and_whitespace_only_buffer_is_none() {
        let text = "-- just a comment\n/* another */   \n";
        let s = split(text);
        assert!(s.statements().is_empty());
        for offset in [0, text.len() / 2, text.len()] {
            assert_eq!(s.statement_at(offset), None);
        }
    }

    #[test]
    fn statement_at_before_first_statement_with_no_preceding_terminator_resolves_to_it() {
        // NOT a `None` case: unlike `statement_at_before_any_statement_is_none`
        // (which relies on a leading empty `;` moving the first statement's
        // span start away from offset 0), plain leading whitespace with
        // nothing before it leaves the first statement's `span.start` at 0
        // (segments start at the previous terminator OR buffer start).
        // Position 0 is therefore contained in statement 0's span via rule 2,
        // symmetric with how a trailing/interior gap belongs to the
        // following statement everywhere else in this module.
        let text = "   \nSELECT 1;";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].span.start, 0);
        let resolved = s.statement_at(0).unwrap();
        assert_eq!(resolved.sql_span, s.statements()[0].sql_span);
    }

    // ---- statements_in (selection) -----------------------------------------

    #[test]
    fn statements_in_selection_fully_inside_one_statement_returns_whole_statement() {
        let text = "SELECT 1; SELECT 2; SELECT 3;";
        let s = split(text);
        let stmt1 = &s.statements()[1];
        let start = stmt1.sql_span.start + 1;
        let end = stmt1.sql_span.end - 1;
        let result = s.statements_in(start, end);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sql_span, stmt1.sql_span);
    }

    #[test]
    fn statements_in_selection_spanning_two_adjacent_statements() {
        let text = "SELECT 1; SELECT 2; SELECT 3;";
        let s = split(text);
        let start = s.statements()[0].sql_span.start;
        let end = s.statements()[1].sql_span.end;
        let result = s.statements_in(start, end);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].sql_span, s.statements()[0].sql_span);
        assert_eq!(result[1].sql_span, s.statements()[1].sql_span);
    }

    #[test]
    fn statements_in_partial_overlap_at_both_ends_still_returns_whole_statements() {
        let text = "SELECT 1; SELECT 2; SELECT 3;";
        let s = split(text);
        // Selection starts mid-way through statement 0 and ends mid-way
        // through statement 2, fully containing statement 1. Per the
        // "selection expands to whole statements" semantics, all three whole
        // statements must be returned, not sub-ranges.
        let start = s.statements()[0].sql_span.start + 3;
        let end = s.statements()[2].sql_span.start + 3;
        let result = s.statements_in(start, end);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].sql_span, s.statements()[0].sql_span);
        assert_eq!(result[1].sql_span, s.statements()[1].sql_span);
        assert_eq!(result[2].sql_span, s.statements()[2].sql_span);
    }

    #[test]
    fn statements_in_selection_in_blank_space_between_statements_is_empty() {
        let text = "SELECT 1;      SELECT 2;";
        let s = split(text);
        let start = s.statements()[0].sql_span.end + 1;
        let end = s.statements()[1].sql_span.start - 1;
        assert!(start < end, "test setup must produce a non-empty gap");
        let result = s.statements_in(start, end);
        assert!(result.is_empty());
    }

    #[test]
    fn statements_in_zero_width_selection_is_empty() {
        let text = "SELECT 1; SELECT 2;";
        let s = split(text);
        let offset = s.statements()[0].sql_span.start + 2;
        assert!(s.statements_in(offset, offset).is_empty());
    }

    #[test]
    fn statements_in_reversed_range_is_normalized() {
        let text = "SELECT 1; SELECT 2; SELECT 3;";
        let s = split(text);
        let start = s.statements()[0].sql_span.start;
        let end = s.statements()[1].sql_span.end;
        let forward = s.statements_in(start, end);
        let reversed = s.statements_in(end, start);
        assert_eq!(forward, reversed);
        assert_eq!(forward.len(), 2);
    }

    // ---- tokenizer error handling -------------------------------------------

    #[test]
    fn unterminated_dollar_quote_reports_good_prefix_and_error_span_from_last_good_token() {
        let text = "SELECT 1; SELECT $$abc";
        let s = split(text);
        // The trailing segment is truncated mid-statement, same shape as
        // `tokenizer_error_reports_good_prefix` for an unterminated string --
        // only the completed "SELECT 1" is emitted.
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT 1");
        let err = s.error().expect("unterminated dollar-quote should error");
        assert_eq!(err.kind, SplitErrorKind::Tokenize);
        // The error span starts right after the preceding ';', covering the
        // whole poisoned tail -- not derived from `TokenizerError::location`,
        // which is inconsistent (start-of-token for an unterminated string,
        // but EOF for an unterminated dollar-quote; verified empirically
        // against sqlparser 0.62.0).
        assert_eq!(err.span.start, s.statements()[0].span.end);
        assert_eq!(err.span.end, text.len());
    }

    // ---- LineIndex (direct, private-item access via same-file test mod) ---

    #[test]
    fn line_index_line_one_col_one_is_offset_zero() {
        let text = "SELECT 1;\nSELECT 2;";
        let mut idx = LineIndex::build(text);
        assert_eq!(idx.byte_offset_at(text, 1, 1), 0);
    }

    #[test]
    fn line_index_resolves_a_later_line_correctly() {
        let text = "ab\ncd\nef";
        let mut idx = LineIndex::build(text);
        // Line 3 ("ef"), column 2 -> the byte offset of 'f'.
        assert_eq!(idx.byte_offset_at(text, 3, 2), text.find('f').unwrap());
    }

    #[test]
    fn line_index_accounts_for_multibyte_characters_on_earlier_lines() {
        let text = "🦀\nab";
        let mut idx = LineIndex::build(text);
        // Line 2 ("ab"), column 2 -> byte offset of 'b'; must be counted
        // from the crab's BYTE length (4), not its character count (1).
        assert_eq!(idx.byte_offset_at(text, 2, 2), text.find('b').unwrap());
    }

    #[test]
    fn line_index_zero_zero_sentinel_clamps_to_offset_zero() {
        let text = "SELECT 1;";
        let mut idx = LineIndex::build(text);
        assert_eq!(idx.byte_offset_at(text, 0, 0), 0);
    }

    #[test]
    fn every_statement_emitted_alongside_an_error_is_semicolon_terminated() {
        let text = "SELECT 1; SELECT 'unterminated";
        let s = split(text);
        let err = s.error().unwrap();
        for stmt in s.statements() {
            assert!(
                stmt.span.end <= err.span.start,
                "statement span {:?} overlaps error region starting at {}",
                stmt.span,
                err.span.start
            );
            let prefix = text[..stmt.span.end].trim_end();
            assert!(
                prefix.ends_with(';'),
                "statement span {:?} does not end at a ';' (prefix: {:?})",
                stmt.span,
                prefix
            );
        }
    }

    // ---- SplitErrorKind ---------------------------------------------------

    #[test]
    fn unclosed_paren_at_eof_reports_unclosed_paren_kind_and_only_terminated_statements() {
        let text = "SELECT 1;\nSELECT count(\nSELECT 2;\nDELETE FROM audit_log;";
        let s = split(text);
        assert_eq!(s.statements().len(), 1);
        assert_eq!(s.statements()[0].sql_span.slice(text), "SELECT 1");
        let err = s.error().expect("unclosed paren should error");
        assert_eq!(err.kind, SplitErrorKind::UnclosedParen);
        assert_eq!(err.span.start, s.statements()[0].span.end);
        assert_eq!(err.span.end, text.len());
    }

    #[test]
    fn semicolon_inside_rule_body_parens_does_not_split_and_depth_returns_to_zero() {
        let text = "CREATE RULE r AS ON INSERT TO foo DO (DELETE FROM bar; INSERT INTO bar VALUES (1));\nSELECT 9;";
        let s = split(text);
        assert!(s.error().is_none());
        // Two statements, not one: this proves paren depth actually returns
        // to zero at the end of the CREATE RULE statement, rather than
        // merely never causing an error by EOF -- a depth leak here would
        // silently swallow `SELECT 9` into the same "statement" instead of
        // splitting it out.
        assert_eq!(s.statements().len(), 2);
        assert_eq!(
            s.statements()[0].sql_span.slice(text),
            "CREATE RULE r AS ON INSERT TO foo DO (DELETE FROM bar; INSERT INTO bar VALUES (1))"
        );
        assert_eq!(s.statements()[1].sql_span.slice(text), "SELECT 9");
    }

    // ---- \r / \r\n line endings --------------------------------------------

    #[test]
    fn sqlparser_advances_line_only_on_newline_not_bare_carriage_return() {
        // Canary, same class as the Span::end one above: LineIndex indexes line
        // starts by scanning for '\n' ONLY. If sqlparser ever treated a bare '\r'
        // as a line terminator, every offset after it would be silently off by
        // one -- wrong statement boundaries, not a cosmetic bug.
        use sqlparser::tokenizer::Location;

        let mut buf = Vec::new();
        Tokenizer::new(&PostgreSqlDialect {}, "a\rb")
            .tokenize_with_location_into_buf(&mut buf)
            .unwrap();
        assert_eq!(buf.last().unwrap().span.start, Location::new(1, 3));

        let mut buf = Vec::new();
        Tokenizer::new(&PostgreSqlDialect {}, "a\r\nb")
            .tokenize_with_location_into_buf(&mut buf)
            .unwrap();
        assert_eq!(buf.last().unwrap().span.start, Location::new(2, 1));
    }

    #[test]
    fn split_handles_crlf_line_endings() {
        let text = "SELECT 1;\r\nSELECT 2;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert!(
            s.statements()[0]
                .sql_span
                .slice(text)
                .trim()
                .ends_with("SELECT 1")
        );
        assert!(
            s.statements()[1]
                .sql_span
                .slice(text)
                .trim()
                .ends_with("SELECT 2")
        );
    }

    #[test]
    fn split_handles_bare_cr_line_endings() {
        let text = "SELECT 1;\rSELECT 2;";
        let s = split(text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 2);
        assert!(
            s.statements()[0]
                .sql_span
                .slice(text)
                .trim()
                .ends_with("SELECT 1")
        );
        assert!(
            s.statements()[1]
                .sql_span
                .slice(text)
                .trim()
                .ends_with("SELECT 2")
        );
    }

    // ---- perf regression -----------------------------------------------------

    #[test]
    fn split_is_linear_on_a_long_single_line_buffer() {
        // Regression guard: LineIndex used to re-scan from the line start on
        // every lookup, making this O(n^2) -- 316KB measured at ~15s on the UI
        // thread. The bound is deliberately ~7x looser than the observed
        // regression so a loaded machine doesn't make it flaky.
        let mut text = String::from("INSERT INTO t VALUES ");
        for i in 0..20_000 {
            text.push_str(&format!("({i},'xxxxxxxx'),"));
        }
        text.push_str("(0,'x');");
        let start = std::time::Instant::now();
        let s = split(&text);
        assert!(s.error().is_none());
        assert_eq!(s.statements().len(), 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "split took {:?}, expected well under 2s",
            start.elapsed()
        );
    }
}
