use crate::model::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Attachment {
    Exact {
        range: TextRange,
    },
    ContextMatched {
        range: TextRange,
    },
    Ambiguous {
        candidates: Vec<TextRange>,
        total: usize,
    },
    Orphaned,
}
pub fn reattach(anchor: &CodeAnchor, blob: &GitOid, bytes: &[u8]) -> Attachment {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Attachment::Orphaned;
    };
    let Ok(start) = usize::try_from(anchor.range.start) else {
        return Attachment::Orphaned;
    };
    let Ok(end) = usize::try_from(anchor.range.end) else {
        return Attachment::Orphaned;
    };
    if blob == &anchor.blob
        && start <= end
        && text.get(start..end) == Some(anchor.selected_text.as_str())
    {
        return Attachment::Exact {
            range: anchor.range.clone(),
        };
    }
    let mut candidates = Vec::new();
    let mut total = 0;
    let starts: Box<dyn Iterator<Item = usize> + '_> = if anchor.selected_text.is_empty() {
        Box::new(
            text.char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len())),
        )
    } else {
        Box::new(text.match_indices(&anchor.selected_text).map(|(i, _)| i))
    };
    for start in starts {
        let end = start + anchor.selected_text.len();
        if text[..start].ends_with(&anchor.context_before)
            && text[end..].starts_with(&anchor.context_after)
        {
            total += 1;
            if candidates.len() < 256 {
                candidates.push(TextRange {
                    start: start as u64,
                    end: end as u64,
                });
            }
        }
    }
    match total {
        0 => Attachment::Orphaned,
        1 => Attachment::ContextMatched {
            range: candidates.remove(0),
        },
        _ => Attachment::Ambiguous { candidates, total },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn anchor() -> CodeAnchor {
        CodeAnchor {
            snapshot: SourceSnapshot {
                tree: GitOid("a".repeat(40)),
            },
            path: RepoPath("a.rs".into()),
            blob: GitOid("b".repeat(40)),
            side: DiffSide::After,
            range: TextRange { start: 4, end: 7 },
            context_before: "let ".into(),
            selected_text: "foo".into(),
            context_after: " =".into(),
        }
    }
    #[test]
    fn exact_context_ambiguous_and_orphaned_are_distinct() {
        let anchor = anchor();
        let other = GitOid("c".repeat(40));
        assert!(matches!(
            reattach(&anchor, &anchor.blob, b"let foo = 1;"),
            Attachment::Exact { .. }
        ));
        assert_eq!(
            reattach(&anchor, &other, b"// moved\nlet foo = 2;"),
            Attachment::ContextMatched {
                range: TextRange { start: 13, end: 16 }
            }
        );
        assert!(matches!(
            reattach(&anchor, &other, b"let foo = 1; let foo = 2;"),
            Attachment::Ambiguous { total: 2, .. }
        ));
        assert_eq!(
            reattach(&anchor, &other, b"let food = 1;"),
            Attachment::Orphaned
        );
        assert_eq!(reattach(&anchor, &other, b"\xff"), Attachment::Orphaned);
    }
    #[test]
    fn utf8_and_empty_ranges_use_byte_offsets_and_bounded_candidates() {
        let mut anchor = anchor();
        anchor.context_before.clear();
        anchor.context_after.clear();
        anchor.selected_text = "é".into();
        assert_eq!(
            reattach(&anchor, &GitOid("c".repeat(40)), "λé".as_bytes()),
            Attachment::ContextMatched {
                range: TextRange { start: 2, end: 4 }
            }
        );
        anchor.selected_text.clear();
        assert!(
            matches!(reattach(&anchor,&GitOid("c".repeat(40)),&vec![b'a';1000]),Attachment::Ambiguous{candidates,total:1001} if candidates.len()==256)
        );
    }
}
