use crate::{model::*, persistence::GitRepository};
use std::collections::{BTreeMap, BTreeSet};

type Entry = (String, GitOid);

fn storage(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(ApiErrorCode::StorageFailure, error.to_string())
}

fn flatten(
    repo: &GitRepository,
    tree: &str,
    prefix: &str,
    output: &mut BTreeMap<RepoPath, Entry>,
) -> ApiResult<()> {
    for entry in repo.tree_entries(tree).map_err(storage)? {
        let path = format!("{prefix}{}", entry.name);
        let checked = RepoPath::parse(path.clone())
            .map_err(|error| ApiError::new(ApiErrorCode::UnsupportedFile, error))?;
        match entry.kind.as_str() {
            "tree" => flatten(repo, &entry.oid, &format!("{path}/"), output)?,
            "blob" => {
                output.insert(
                    checked,
                    (entry.mode, GitOid::parse(entry.oid).map_err(storage)?),
                );
            }
            _ => {
                return Err(ApiError::new(
                    ApiErrorCode::UnsupportedFile,
                    format!("unsupported source entry: {path}"),
                ));
            }
        }
    }
    Ok(())
}

pub fn files(
    repo: &GitRepository,
    before: &SourceSnapshot,
    after: &SourceSnapshot,
) -> ApiResult<Vec<FileDiff>> {
    let mut old = BTreeMap::new();
    let mut new = BTreeMap::new();
    flatten(repo, &before.tree.0, "", &mut old)?;
    flatten(repo, &after.tree.0, "", &mut new)?;
    let paths: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let mut output = Vec::new();
    for path in paths {
        let left = old.get(&path);
        let right = new.get(&path);
        if left == right {
            continue;
        }
        let old_bytes = left
            .map(|entry| repo.read_blob(&entry.1.0))
            .transpose()
            .map_err(storage)?
            .unwrap_or_default();
        let new_bytes = right
            .map(|entry| repo.read_blob(&entry.1.0))
            .transpose()
            .map_err(storage)?
            .unwrap_or_default();
        let binary = old_bytes
            .iter()
            .take(8000)
            .chain(new_bytes.iter().take(8000))
            .any(|&byte| byte == 0);
        let kind = match (left, right) {
            (None, _) => FileChangeKind::Added,
            (_, None) => FileChangeKind::Deleted,
            (Some(a), Some(b)) if a.1 == b.1 => FileChangeKind::ModeChanged,
            _ => FileChangeKind::Modified,
        };
        let hunks = if binary || old_bytes == new_bytes {
            Vec::new()
        } else {
            let empty = if left.is_none() || right.is_none() {
                Some(repo.hash_blob(b"").map_err(storage)?)
            } else {
                None
            };
            let old_oid = left
                .map(|entry| entry.1.0.as_str())
                .or(empty.as_deref())
                .unwrap();
            let new_oid = right
                .map(|entry| entry.1.0.as_str())
                .or(empty.as_deref())
                .unwrap();
            let patch = repo
                .run(
                    &[
                        "-c",
                        "diff.suppressBlankEmpty=false",
                        "diff",
                        "--output-indicator-new=+",
                        "--output-indicator-old=-",
                        "--output-indicator-context= ",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-color",
                        "--no-renames",
                        "--text",
                        "--unified=3",
                        "--diff-algorithm=myers",
                        "--no-indent-heuristic",
                        old_oid,
                        new_oid,
                        "--",
                    ],
                    &[],
                    None,
                )
                .map_err(storage)?;
            parse_hunks(&patch, &old_bytes, &new_bytes)?
        };
        output.push(FileDiff {
            old_path: left.map(|_| path.clone()),
            new_path: right.map(|_| path),
            old_blob: left.map(|entry| entry.1.clone()),
            new_blob: right.map(|entry| entry.1.clone()),
            kind,
            binary,
            hunks,
        });
    }
    Ok(output)
}

fn line_ranges(bytes: &[u8]) -> Vec<TextRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (position, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            ranges.push(TextRange {
                start: start as u64,
                end: (position + 1) as u64,
            });
            start = position + 1;
        }
    }
    if start < bytes.len() {
        ranges.push(TextRange {
            start: start as u64,
            end: bytes.len() as u64,
        });
    }
    ranges
}

fn parse_span(text: &str) -> ApiResult<(u32, u32)> {
    let text = text
        .get(1..)
        .ok_or_else(|| storage("invalid diff hunk span"))?;
    let (start, count) = text.split_once(',').unwrap_or((text, "1"));
    Ok((
        start.parse().map_err(storage)?,
        count.parse().map_err(storage)?,
    ))
}

fn parse_hunks(patch: &[u8], old: &[u8], new: &[u8]) -> ApiResult<Vec<DiffHunk>> {
    let old_ranges = line_ranges(old);
    let new_ranges = line_ranges(new);
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut old_line = 0;
    let mut new_line = 0;
    for row in patch.split(|&byte| byte == b'\n') {
        if row.starts_with(b"@@ ") {
            let header = std::str::from_utf8(row).map_err(storage)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            if fields.len() < 4 {
                return Err(storage("invalid diff hunk header"));
            }
            let (old_start, old_count) = parse_span(fields[1])?;
            let (new_start, new_count) = parse_span(fields[2])?;
            old_line = old_start;
            new_line = new_start;
            hunks.push(DiffHunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        let kind = match row.first() {
            Some(b' ') => DiffLineKind::Context,
            Some(b'+') => DiffLineKind::Added,
            Some(b'-') => DiffLineKind::Removed,
            _ => continue,
        };
        let has_old = kind != DiffLineKind::Added;
        let has_new = kind != DiffLineKind::Removed;
        let old_range = if has_old {
            Some(
                old_ranges
                    .get(old_line.saturating_sub(1) as usize)
                    .ok_or_else(|| storage("diff old line out of range"))?
                    .clone(),
            )
        } else {
            None
        };
        let new_range = if has_new {
            Some(
                new_ranges
                    .get(new_line.saturating_sub(1) as usize)
                    .ok_or_else(|| storage("diff new line out of range"))?
                    .clone(),
            )
        } else {
            None
        };
        let bytes = if let Some(range) = &new_range {
            &new[range.start as usize..range.end as usize]
        } else {
            let range = old_range.as_ref().unwrap();
            &old[range.start as usize..range.end as usize]
        };
        hunk.lines.push(DiffLine {
            kind,
            old_range,
            new_range,
            text: String::from_utf8_lossy(bytes).into_owned(),
            old_line: has_old.then_some(old_line),
            new_line: has_new.then_some(new_line),
        });
        if has_old {
            old_line += 1;
        }
        if has_new {
            new_line += 1;
        }
    }
    for hunk in &hunks {
        if hunk
            .lines
            .iter()
            .filter(|line| line.old_line.is_some())
            .count()
            != hunk.old_count as usize
            || hunk
                .lines
                .iter()
                .filter(|line| line.new_line.is_some())
                .count()
                != hunk.new_count as usize
        {
            return Err(storage("diff hunk counts do not match its lines"));
        }
    }
    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, GitRepository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(dir.path()).unwrap();
        (dir, repo)
    }

    fn snapshot(repo: &GitRepository, entries: &[(&str, &[u8], &str)]) -> SourceSnapshot {
        let updates = entries
            .iter()
            .map(|(path, bytes, mode)| {
                (
                    path.to_string(),
                    Some((mode.to_string(), repo.hash_blob(bytes).unwrap())),
                )
            })
            .collect();
        SourceSnapshot {
            tree: GitOid::parse(
                repo.tree_with_updates(&repo.empty_tree().unwrap(), &updates)
                    .unwrap(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn nested_added_deleted_and_mode_only_files() {
        let (_dir, repo) = fixture();
        let before = snapshot(
            &repo,
            &[
                ("gone.rs", b"old\n", "100644"),
                ("run.sh", b"echo hi\n", "100644"),
            ],
        );
        let after = snapshot(
            &repo,
            &[
                ("nested/new.rs", b"new\n", "100644"),
                ("run.sh", b"echo hi\n", "100755"),
            ],
        );
        let changes = files(&repo, &before, &after).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].kind, FileChangeKind::Deleted);
        assert!(changes[0].new_path.is_none());
        assert_eq!(changes[0].hunks[0].new_count, 0);
        assert_eq!(changes[1].kind, FileChangeKind::Added);
        assert!(changes[1].old_path.is_none());
        assert_eq!(changes[1].hunks[0].old_count, 0);
        assert_eq!(changes[2].kind, FileChangeKind::ModeChanged);
        assert!(changes[2].hunks.is_empty());
    }

    #[test]
    fn exact_crlf_and_multibyte_offsets_with_missing_eof_newline() {
        let (_dir, repo) = fixture();
        let old = "a\r\n\u{e9}\r\nlast".as_bytes();
        let new = "a\r\n\u{1f980}\r\nlast\n".as_bytes();
        let before = snapshot(&repo, &[("x", old, "100644")]);
        let after = snapshot(&repo, &[("x", new, "100644")]);
        let changes = files(&repo, &before, &after).unwrap();
        assert_eq!(changes[0].kind, FileChangeKind::Modified);
        let hunk = &changes[0].hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (1, 3, 1, 3)
        );
        for line in &hunk.lines {
            if let Some(range) = &line.old_range {
                assert_eq!(
                    line.text.as_bytes(),
                    &old[range.start as usize..range.end as usize]
                );
            }
            if let Some(range) = &line.new_range {
                assert_eq!(
                    line.text.as_bytes(),
                    &new[range.start as usize..range.end as usize]
                );
            }
        }
        assert!(
            hunk.lines
                .iter()
                .any(|line| line.text == "last" && line.kind == DiffLineKind::Removed)
        );
        assert!(
            hunk.lines
                .iter()
                .any(|line| line.text == "last\n" && line.kind == DiffLineKind::Added)
        );
    }

    #[test]
    fn binary_and_empty_files() {
        let (_dir, repo) = fixture();
        let before = snapshot(&repo, &[]);
        let after = snapshot(
            &repo,
            &[("binary", b"abc\0def", "100644"), ("empty", b"", "100644")],
        );
        let changes = files(&repo, &before, &after).unwrap();
        assert!(changes[0].binary);
        assert!(changes[0].hunks.is_empty());
        assert!(!changes[1].binary);
        assert!(changes[1].hunks.is_empty());
        assert!(files(&repo, &after, &after).unwrap().is_empty());
    }

    #[test]
    fn lossy_text_retains_exact_byte_offsets() {
        let (_dir, repo) = fixture();
        let before = snapshot(&repo, &[]);
        let after = snapshot(&repo, &[("bytes", b"\xff\xfe\n", "100644")]);
        let changes = files(&repo, &before, &after).unwrap();
        let line = &changes[0].hunks[0].lines[0];
        assert_eq!(line.new_range, Some(TextRange { start: 0, end: 3 }));
        assert_eq!(line.text, "\u{fffd}\u{fffd}\n");
        assert!(!changes[0].binary);
    }

    #[test]
    fn separated_hunks_have_correct_absolute_line_numbers() {
        let (_dir, repo) = fixture();
        let old: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 2\n", "replacement 2\n")
            .replace("line 28\n", "replacement 28\n");
        let before = snapshot(&repo, &[("x", old.as_bytes(), "100644")]);
        let after = snapshot(&repo, &[("x", new.as_bytes(), "100644")]);
        let changes = files(&repo, &before, &after).unwrap();
        assert_eq!(changes[0].hunks.len(), 2);
        let added: Vec<_> = changes[0]
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == DiffLineKind::Added)
            .map(|l| l.new_line.unwrap())
            .collect();
        assert_eq!(added, vec![2, 28]);
    }

    #[test]
    fn content_that_looks_like_patch_headers_is_data() {
        let (_dir, repo) = fixture();
        let before = snapshot(&repo, &[]);
        let bytes = b"@@ -1 +1 @@\n--- a/x\n+++ b/x\n\\ No newline at end of file";
        let after = snapshot(&repo, &[("x", bytes, "100644")]);
        let changes = files(&repo, &before, &after).unwrap();
        assert_eq!(changes[0].hunks[0].lines.len(), 4);
        assert_eq!(
            changes[0].hunks[0].lines[3].text,
            "\\ No newline at end of file"
        );
    }
    #[test]
    fn user_diff_display_config_cannot_change_protocol_parsing() {
        let (_dir, repo) = fixture();
        for (key, value) in [
            ("diff.suppressBlankEmpty", "true"),
            ("diff.outputIndicatorNew", ">"),
            ("diff.outputIndicatorOld", "<"),
            ("diff.outputIndicatorContext", ":"),
        ] {
            repo.run(&["config", key, value], &[], None).unwrap();
        }
        let before = snapshot(&repo, &[("x", b"a\n\nb\n", "100644")]);
        let after = snapshot(&repo, &[("x", b"a\n\nc\n", "100644")]);
        let changes = files(&repo, &before, &after).unwrap();
        assert_eq!(changes[0].hunks[0].lines.len(), 4);
        assert_eq!(changes[0].hunks[0].lines[1].text, "\n");
    }
}
