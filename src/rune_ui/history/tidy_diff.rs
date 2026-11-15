//! The 文字整理 pass as a GitHub-style unified diff for a History entry:
//! context once, `-` removed lines, `+` added lines, and the changed
//! characters marked inside a replaced pair.

use rune::{El, Span, col, rich, row, text};

use super::super::controls::Paint;
use crate::desktop::i18n::{tr, tr_fill};

/// The 文字整理 pass as a unified diff, GitHub style: context lines once,
/// `-` lines for what the model removed, `+` lines for what it added, with
/// the changed characters marked inside a replaced line pair.
pub(super) fn tidy_diff(before: &str, after: &str, p: Paint) -> El {
    if before == after {
        return text(tr("Tidying changed nothing"))
            .key("tidy-diff")
            .font(p.k.ty.body)
            .color(p.faint())
            .wrap();
    }
    let mut list = col().key("tidy-diff").gap(2.0);
    for (index, row) in tidy_rows(before, after).into_iter().enumerate() {
        list = list.child(match row {
            TidyRow::Gap(lines) => text(tr_fill("{} unchanged lines", &lines.to_string()))
                .key(format!("tidy-gap{index}"))
                .font(p.k.ty.body)
                .color(p.faint())
                .py(2.0),
            TidyRow::Line { kind, text, other } => {
                tidy_line(index, kind, &text, other.as_deref(), p)
            }
        });
    }
    list
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TidyKind {
    Context,
    Delete,
    Insert,
}

enum TidyRow {
    Line {
        kind: TidyKind,
        text: String,
        /// The other side of a replaced line pair, for character marks.
        other: Option<String>,
    },
    /// A run of equal lines left out between context lines.
    Gap(usize),
}

/// One line of the unified diff: a fixed marker gutter and the text. Only
/// the marker and the characters this side changed carry a colour; the rest
/// stays in the normal text colour.
fn tidy_line(index: usize, kind: TidyKind, line: &str, other: Option<&str>, p: Paint) -> El {
    let (marker, change_color, body_color) = match kind {
        TidyKind::Context => (" ", p.muted(), p.muted()),
        TidyKind::Delete => ("-", p.danger(), p.text()),
        TidyKind::Insert => ("+", p.success(), p.text()),
    };
    let body = match other {
        Some(other) => {
            let (content, spans) = marked(line, other, change_color);
            rich(content, spans).font(p.k.ty.mono).color(body_color)
        }
        // A line added or removed whole has no unchanged text left to keep
        // plain: colour it, since every character is the change.
        None => text(line.to_owned())
            .font(p.k.ty.mono)
            .color(change_color)
            .wrap(),
    };
    row()
        .key(format!("tidy-row{index}"))
        .w_full()
        .gap(6.0)
        .items_start()
        .px(6.0)
        .py(2.0)
        .child(text(marker).font(p.k.ty.mono).color(change_color).w(10.0))
        .child(body.grow(1.0).shrink())
}

/// `line` with the characters it changed against `other` marked: Equal and
/// Delete runs reconstruct this side, and its Delete characters are the
/// changed ones (Insert characters belong to the other side only).
fn marked(line: &str, other: &str, color: rune::Color) -> (String, Vec<Span>) {
    let diff = similar::TextDiff::from_chars(line, other);
    let mut content = String::new();
    let mut spans = Vec::new();
    for change in diff.iter_all_changes() {
        let value = change.value();
        let tag = change.tag();
        if tag == similar::ChangeTag::Insert {
            continue;
        }
        let offset = content.len();
        let lead = value.len() - value.trim_start().len();
        let trail = value.len() - value.trim_end().len();
        content.push_str(value);
        let start = offset + lead;
        let end = offset + value.len() - trail;
        if tag == similar::ChangeTag::Delete && start < end {
            spans.push(Span::new(start, end).color(color).underline(color));
        }
    }
    (content, spans)
}

/// The unified diff's rows: equal lines (collapsed to two around a gap),
/// then each replaced hunk's `-` lines and `+` lines, as GitHub shows them.
fn tidy_rows(before: &str, after: &str) -> Vec<TidyRow> {
    let old = tidy_lines(before);
    let new = tidy_lines(after);
    let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
    let diff = similar::TextDiff::from_slices(&old_refs, &new_refs);
    let mut rows = Vec::new();
    for op in diff.ops() {
        match *op {
            similar::DiffOp::Equal { old_index, len, .. } => {
                let lines = &old[old_index..old_index + len];
                if len > 6 {
                    rows.extend(lines[..2].iter().map(|line| context(line)));
                    rows.push(TidyRow::Gap(len - 4));
                    rows.extend(lines[len - 2..].iter().map(|line| context(line)));
                } else {
                    rows.extend(lines.iter().map(|line| context(line)));
                }
            }
            similar::DiffOp::Delete {
                old_index, old_len, ..
            } => rows.extend(
                old[old_index..old_index + old_len]
                    .iter()
                    .map(|line| added_or_deleted(line, TidyKind::Delete, None)),
            ),
            similar::DiffOp::Insert {
                new_index, new_len, ..
            } => rows.extend(
                new[new_index..new_index + new_len]
                    .iter()
                    .map(|line| added_or_deleted(line, TidyKind::Insert, None)),
            ),
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let paired = old_len.min(new_len);
                for index in 0..old_len {
                    rows.push(added_or_deleted(
                        old[old_index + index].as_str(),
                        TidyKind::Delete,
                        (index < paired).then(|| new[new_index + index].clone()),
                    ));
                }
                for index in 0..new_len {
                    rows.push(added_or_deleted(
                        new[new_index + index].as_str(),
                        TidyKind::Insert,
                        (index < paired).then(|| old[old_index + index].clone()),
                    ));
                }
            }
        }
    }
    rows
}

fn context(line: &str) -> TidyRow {
    TidyRow::Line {
        kind: TidyKind::Context,
        text: line.to_owned(),
        other: None,
    }
}

fn added_or_deleted(line: &str, kind: TidyKind, other: Option<String>) -> TidyRow {
    TidyRow::Line {
        kind,
        text: line.to_owned(),
        other,
    }
}

/// Split a dictation into diff lines: its own line breaks, then sentences at
/// terminal punctuation, so a paragraph becomes a few aligned rows.
fn tidy_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut start = 0;
        for (index, (offset, character)) in chars.iter().enumerate() {
            let terminal = matches!(character, '。' | '！' | '？' | '；' | '!' | '?' | ';')
                || (*character == '.'
                    && chars
                        .get(index + 1)
                        .is_none_or(|(_, next)| next.is_whitespace()));
            if !terminal {
                continue;
            }
            let end = offset + character.len_utf8();
            let piece = line[start..end].trim();
            if !piece.is_empty() {
                lines.push(piece.to_owned());
            }
            start = end;
        }
        let rest = line[start..].trim();
        if !rest.is_empty() {
            lines.push(rest.to_owned());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tidy_diff_splits_sentences_and_pairs_replacements() {
        assert_eq!(
            tidy_lines("第一句。第二句！\n第三句因为比较长"),
            vec!["第一句。", "第二句！", "第三句因为比较长"]
        );
        let before = "The deploy failed because the the cache key changed.";
        let after = "The deploy failed because the cache key had changed.";
        let rows = tidy_rows(before, after);
        assert_eq!(rows.len(), 2, "one removed line and one added line");
        match &rows[0] {
            TidyRow::Line {
                kind: TidyKind::Delete,
                text,
                other,
            } => {
                assert_eq!(text, before);
                assert_eq!(other.as_deref(), Some(after));
            }
            _ => panic!("the first row is the removed line"),
        }
        match &rows[1] {
            TidyRow::Line {
                kind: TidyKind::Insert,
                text,
                other,
            } => {
                assert_eq!(text, after);
                assert_eq!(other.as_deref(), Some(before));
            }
            _ => panic!("the second row is the added line"),
        }
    }

    #[test]
    fn a_replaced_line_keeps_its_own_text() {
        let before = "The deploy failed because the the cache key changed.";
        let after = "The deploy failed because the cache key had changed.";
        let color = rune::Color::WHITE;
        assert_eq!(marked(before, after, color).0, before);
        assert_eq!(marked(after, before, color).0, after);
        assert!(!marked(before, after, color).1.is_empty());
        assert!(!marked(after, before, color).1.is_empty());
    }
}
