use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarkerChoice {
    pub start: usize,
    pub end: usize,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub body: String,
    pub marker: ByteRange,
    pub target: ByteRange,
    pub file_level: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MapSegment {
    pub annotated: ByteRange,
    pub cleaned: ByteRange,
    pub removed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceMap {
    pub annotated_len: usize,
    pub cleaned_len: usize,
    pub segments: Vec<MapSegment>,
}

impl SourceMap {
    pub fn annotated_to_cleaned(&self, offset: usize) -> Option<usize> {
        if offset > self.annotated_len {
            return None;
        }
        for segment in &self.segments {
            if offset >= segment.annotated.start && offset <= segment.annotated.end {
                if segment.removed {
                    return if offset == segment.annotated.start || offset == segment.annotated.end {
                        Some(segment.cleaned.start)
                    } else {
                        None
                    };
                }
                return Some(segment.cleaned.start + offset - segment.annotated.start);
            }
        }
        (offset == 0).then_some(0)
    }

    pub fn cleaned_to_annotated(&self, offset: usize) -> Option<usize> {
        if offset > self.cleaned_len {
            return None;
        }
        self.segments
            .iter()
            .find_map(|segment| {
                (!segment.removed
                    && offset >= segment.cleaned.start
                    && offset < segment.cleaned.end)
                    .then(|| segment.annotated.start + offset - segment.cleaned.start)
            })
            .or_else(|| (offset == self.cleaned_len).then_some(self.annotated_len))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Extraction {
    pub cleaned: Vec<u8>,
    pub annotations: Vec<Annotation>,
    pub source_map: SourceMap,
}

struct Candidate {
    range: ByteRange,
    body: String,
}

struct Lexed {
    candidates: Vec<Candidate>,
    code: Vec<ByteRange>,
}

pub fn extract(
    base: &[u8],
    annotated: &[u8],
    intents: &[MarkerChoice],
) -> Result<Extraction, String> {
    std::str::from_utf8(base).map_err(|_| "base source is not UTF-8")?;
    std::str::from_utf8(annotated).map_err(|_| "annotated source is not UTF-8")?;
    let original = lex(base);
    let parsed = lex(annotated);
    let mut existing = BTreeMap::new();
    for candidate in original.candidates {
        *existing
            .entry(base[candidate.range.start..candidate.range.end].to_vec())
            .or_insert(0usize) += 1;
    }
    let mut choices = BTreeMap::new();
    for choice in intents {
        let key = (choice.start, choice.end);
        if !parsed
            .candidates
            .iter()
            .any(|candidate| (candidate.range.start, candidate.range.end) == key)
        {
            return Err("marker choice does not identify a current comment token".into());
        }
        if choices.insert(key, choice.enabled).is_some() {
            return Err("duplicate marker choice".into());
        }
    }
    let mut removals = Vec::new();
    let mut pending = Vec::new();
    for candidate in parsed.candidates {
        let old_count = existing
            .entry(annotated[candidate.range.start..candidate.range.end].to_vec())
            .or_insert(0);
        let was_existing = *old_count > 0;
        *old_count = old_count.saturating_sub(1);
        if !choices
            .get(&(candidate.range.start, candidate.range.end))
            .copied()
            .unwrap_or(!was_existing)
        {
            continue;
        }
        if candidate.body.is_empty() {
            return Err("review instruction cannot be empty".into());
        }
        let line_start = annotated[..candidate.range.start]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1);
        let standalone = annotated[line_start..candidate.range.start]
            .iter()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r'));
        let removal = if standalone {
            let end = if annotated.get(candidate.range.end) == Some(&b'\r')
                && annotated.get(candidate.range.end + 1) == Some(&b'\n')
            {
                candidate.range.end + 2
            } else if annotated.get(candidate.range.end) == Some(&b'\n') {
                candidate.range.end + 1
            } else {
                candidate.range.end
            };
            ByteRange {
                start: line_start,
                end,
            }
        } else {
            candidate.range.clone()
        };
        let target = if standalone {
            parsed
                .code
                .iter()
                .find(|token| token.start >= candidate.range.end)
                .map(|first| {
                    let line_end = annotated[first.start..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(annotated.len(), |p| first.start + p);
                    let end = parsed
                        .code
                        .iter()
                        .filter(|token| token.start >= first.start && token.start < line_end)
                        .map(|token| token.end.min(line_end))
                        .max()
                        .unwrap_or(first.end);
                    ByteRange {
                        start: first.start,
                        end,
                    }
                })
        } else {
            let mut preceding = parsed
                .code
                .iter()
                .filter(|token| token.start < candidate.range.start && token.end > line_start);
            preceding.next().map(|first| ByteRange {
                start: first.start.max(line_start),
                end: preceding.next_back().map_or(first.end, |token| token.end),
            })
        };
        pending.push((candidate, target));
        removals.push(removal);
    }
    let mut cleaned = Vec::new();
    let mut segments = Vec::new();
    let mut cursor = 0;
    for removal in removals {
        if removal.start > cursor {
            let start = cleaned.len();
            cleaned.extend_from_slice(&annotated[cursor..removal.start]);
            segments.push(MapSegment {
                annotated: ByteRange {
                    start: cursor,
                    end: removal.start,
                },
                cleaned: ByteRange {
                    start,
                    end: cleaned.len(),
                },
                removed: false,
            });
        }
        segments.push(MapSegment {
            annotated: removal.clone(),
            cleaned: ByteRange {
                start: cleaned.len(),
                end: cleaned.len(),
            },
            removed: true,
        });
        cursor = removal.end;
    }
    if cursor < annotated.len() {
        let start = cleaned.len();
        cleaned.extend_from_slice(&annotated[cursor..]);
        segments.push(MapSegment {
            annotated: ByteRange {
                start: cursor,
                end: annotated.len(),
            },
            cleaned: ByteRange {
                start,
                end: cleaned.len(),
            },
            removed: false,
        });
    }
    let source_map = SourceMap {
        annotated_len: annotated.len(),
        cleaned_len: cleaned.len(),
        segments,
    };
    let annotations = pending
        .into_iter()
        .map(|(candidate, target)| {
            let file_level = target.is_none();
            let target = target.map_or(
                ByteRange {
                    start: 0,
                    end: cleaned.len(),
                },
                |range| ByteRange {
                    start: source_map.annotated_to_cleaned(range.start).unwrap(),
                    end: source_map.annotated_to_cleaned(range.end).unwrap(),
                },
            );
            Annotation {
                body: candidate.body,
                marker: candidate.range,
                target,
                file_level,
            }
        })
        .collect();
    Ok(Extraction {
        cleaned,
        annotations,
        source_map,
    })
}

fn lex(source: &[u8]) -> Lexed {
    let mut candidates = Vec::new();
    let mut code = Vec::new();
    let mut pos = 0;
    while pos < source.len() {
        if source[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if source[pos..].starts_with(b"//") {
            let start = pos;
            while pos < source.len() && !matches!(source[pos], b'\r' | b'\n') {
                pos += 1;
            }
            let text = &source[start + 2..pos];
            if !text.starts_with(b"/") && !text.starts_with(b"!") {
                let trimmed = text
                    .iter()
                    .position(|b| !matches!(b, b' ' | b'\t'))
                    .unwrap_or(text.len());
                if text[trimmed..].starts_with(b"#!") {
                    candidates.push(Candidate {
                        range: ByteRange { start, end: pos },
                        body: String::from_utf8_lossy(&text[trimmed + 2..])
                            .trim()
                            .to_string(),
                    });
                }
            }
            continue;
        }
        if source[pos..].starts_with(b"/*") {
            pos += 2;
            let mut depth = 1;
            while pos < source.len() && depth > 0 {
                if source[pos..].starts_with(b"/*") {
                    depth += 1;
                    pos += 2;
                } else if source[pos..].starts_with(b"*/") {
                    depth -= 1;
                    pos += 2;
                } else {
                    pos += 1;
                }
            }
            continue;
        }
        let start = pos;
        if let Some(end) = raw_string_end(source, pos) {
            pos = end;
        } else if source[pos] == b'"' {
            pos += 1;
            while pos < source.len() {
                match source[pos] {
                    b'\\' => pos = (pos + 2).min(source.len()),
                    b'"' => {
                        pos += 1;
                        break;
                    }
                    _ => pos += 1,
                }
            }
        } else if source[pos] == b'\''
            && let Some(end) = char_end(source, pos)
        {
            pos = end;
        } else if source[pos].is_ascii_alphabetic() || source[pos] == b'_' || source[pos] >= 128 {
            pos += 1;
            while pos < source.len()
                && (source[pos].is_ascii_alphanumeric()
                    || source[pos] == b'_'
                    || source[pos] >= 128)
            {
                pos += 1;
            }
        } else {
            pos += 1;
        }
        code.push(ByteRange { start, end: pos });
    }
    Lexed { candidates, code }
}

fn raw_string_end(source: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    if matches!(source.get(pos), Some(b'b' | b'c')) {
        pos += 1;
    }
    if source.get(pos) != Some(&b'r') {
        return None;
    }
    pos += 1;
    let hashes_start = pos;
    while source.get(pos) == Some(&b'#') {
        pos += 1;
    }
    let hashes = pos - hashes_start;
    if source.get(pos) != Some(&b'"') {
        return None;
    }
    pos += 1;
    while pos < source.len() {
        if source[pos] == b'"'
            && source
                .get(pos + 1..pos + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|&b| b == b'#'))
        {
            return Some(pos + 1 + hashes);
        }
        pos += 1;
    }
    Some(source.len())
}

fn char_end(source: &[u8], start: usize) -> Option<usize> {
    let mut pos = start + 1;
    if source.get(pos) == Some(&b'\\') {
        pos += 1;
        match source.get(pos)? {
            b'u' if source.get(pos + 1) == Some(&b'{') => {
                pos += 2;
                while source.get(pos).is_some_and(|&b| b != b'}') {
                    pos += 1;
                }
                pos += 1;
            }
            b'x' => pos += 3,
            _ => pos += 1,
        }
    } else {
        let first = *source.get(pos)?;
        if matches!(first, b'\'' | b'\n' | b'\r') {
            return None;
        }
        pos += if first < 128 {
            1
        } else if first < 224 {
            2
        } else if first < 240 {
            3
        } else {
            4
        };
    }
    (source.get(pos) == Some(&b'\'')).then_some(pos + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Extraction {
        extract(b"", text.as_bytes(), &[]).unwrap()
    }

    #[test]
    fn trailing_instruction_keeps_exact_code_and_newline() {
        let output = run("let x = 1;  // #! Make constant.\r\n");
        assert_eq!(output.cleaned, b"let x = 1;  \r\n");
        assert_eq!(output.annotations.len(), 1);
        let annotation = &output.annotations[0];
        assert_eq!(annotation.body, "Make constant.");
        assert_eq!(
            &output.cleaned[annotation.target.start..annotation.target.end],
            b"let x = 1;"
        );
        assert!(!annotation.file_level);
    }

    #[test]
    fn standalone_instruction_skips_comments_and_blank_lines() {
        let output =
            run("  // #! Guard this.\r\n\r\n/* ordinary */\r\n// explanation\r\n  call();\r\n");
        assert_eq!(
            output.cleaned,
            b"\r\n/* ordinary */\r\n// explanation\r\n  call();\r\n"
        );
        let target = &output.annotations[0].target;
        assert_eq!(&output.cleaned[target.start..target.end], b"call();");
    }

    #[test]
    fn file_level_at_eof_and_marker_only_files() {
        for text in [
            "// #! Change this",
            " // #! Change this\n",
            "\t// #! Change this\r\n",
        ] {
            let output = run(text);
            assert!(output.cleaned.is_empty());
            assert!(output.annotations[0].file_level);
            assert_eq!(output.annotations[0].target, ByteRange { start: 0, end: 0 });
        }
        let output = run("call();\n// #! Also check imports");
        assert!(output.annotations[0].file_level);
        assert_eq!(output.annotations[0].target.end, output.cleaned.len());
    }

    #[test]
    fn strings_raw_strings_docs_and_nested_blocks_are_not_instructions() {
        let text = concat!(
            "let a = \"// #! literal \\\" quoted\";\n",
            "let b = r###\"// #! raw \\\" \"## still raw\"###;\n",
            "let c = br#\"// #! bytes\"#;\n",
            "let d = cr##\"// #! c string\"##;\n",
            "/* outer /* nested */ // #! block */\n",
            "/// #! docs\n",
            "//! #! inner docs\n",
            "#![allow(dead_code)]\n",
            "let character = '\"'; let escaped = '\\\'';\n",
            "fn borrow<'a>(x: &'a str) {}\n"
        );
        let output = run(text);
        assert!(output.annotations.is_empty());
        assert_eq!(output.cleaned, text.as_bytes());
    }

    #[test]
    fn lexer_resumes_after_literals_and_nested_comments() {
        let output = run("r#\"// #! ignored\"#; /* /* x */ y */ // #! Actual\n");
        assert_eq!(output.annotations.len(), 1);
        assert_eq!(output.annotations[0].body, "Actual");
        assert_eq!(output.cleaned, b"r#\"// #! ignored\"#; /* /* x */ y */ \n");
    }

    #[test]
    fn byte_and_unicode_chars_do_not_swallow_real_comments() {
        let output = run(
            "let a = b'/'; let b = '\u{1f980}'; let c = '\\u{10ffff}'; let d = '\\x2f'; // #! Check\n",
        );
        assert_eq!(output.annotations.len(), 1);
        assert_eq!(output.annotations[0].body, "Check");
    }

    #[test]
    fn existing_comments_stay_and_explicit_enabling_extracts_them() {
        let text = "// #! Existing\ncall();\n";
        let output = extract(text.as_bytes(), text.as_bytes(), &[]).unwrap();
        assert!(output.annotations.is_empty());
        assert_eq!(output.cleaned, text.as_bytes());
        let output = extract(
            text.as_bytes(),
            text.as_bytes(),
            &[MarkerChoice {
                start: 0,
                end: 14,
                enabled: true,
            }],
        )
        .unwrap();
        assert_eq!(output.cleaned, b"call();\n");
        assert_eq!(output.annotations.len(), 1);
    }

    #[test]
    fn existing_markers_survive_offset_changes_and_duplicate_new_marker_extracts() {
        let base = b"// #! Existing\ncall();\n";
        let annotated = b"\n// #! Existing\ncall();\n// #! Existing\n";
        let output = extract(base, annotated, &[]).unwrap();
        assert_eq!(output.annotations.len(), 1);
        assert_eq!(output.cleaned, b"\n// #! Existing\ncall();\n");
    }

    #[test]
    fn disabling_new_marker_preserves_it() {
        let output = extract(
            b"",
            b"// #! literal\n",
            &[MarkerChoice {
                start: 0,
                end: 13,
                enabled: false,
            }],
        )
        .unwrap();
        assert!(output.annotations.is_empty());
        assert_eq!(output.cleaned, b"// #! literal\n");
    }

    #[test]
    fn rejects_empty_enabled_instructions_invalid_utf8_and_stale_choices() {
        assert!(extract(b"", b"// #! \t\n", &[]).is_err());
        assert!(extract(b"", b"\xff", &[]).is_err());
        assert!(extract(b"\xff", b"", &[]).is_err());
        assert!(
            extract(
                b"",
                b"// plain",
                &[MarkerChoice {
                    start: 0,
                    end: 8,
                    enabled: true
                }]
            )
            .is_err()
        );
        let choice = MarkerChoice {
            start: 0,
            end: 7,
            enabled: true,
        };
        assert!(extract(b"", b"// #! x", &[choice.clone(), choice]).is_err());
        assert!(
            extract(
                b"",
                b"// #!",
                &[MarkerChoice {
                    start: 0,
                    end: 5,
                    enabled: false
                }]
            )
            .is_ok()
        );
    }

    #[test]
    fn utf8_offsets_and_byte_maps_preserve_retained_bytes() {
        let text =
            "// #! \u{1f980} improve\r\nlet caf\u{e9} = \"\u{1f980}\"; // #! clarify \u{e9}\r\n";
        let output = run(text);
        assert_eq!(output.annotations.len(), 2);
        for segment in &output.source_map.segments {
            if segment.removed {
                for position in segment.annotated.start + 1..segment.annotated.end {
                    assert_eq!(output.source_map.annotated_to_cleaned(position), None);
                }
            } else {
                assert_eq!(
                    &text.as_bytes()[segment.annotated.start..segment.annotated.end],
                    &output.cleaned[segment.cleaned.start..segment.cleaned.end]
                );
                for offset in 0..segment.annotated.end - segment.annotated.start {
                    assert_eq!(
                        output
                            .source_map
                            .annotated_to_cleaned(segment.annotated.start + offset),
                        Some(segment.cleaned.start + offset)
                    );
                }
            }
        }
        for offset in 0..output.cleaned.len() {
            let original = output.source_map.cleaned_to_annotated(offset).unwrap();
            assert_eq!(output.cleaned[offset], text.as_bytes()[original]);
        }
        assert_eq!(
            output.source_map.annotated_to_cleaned(text.len()),
            Some(output.cleaned.len())
        );
        assert_eq!(output.source_map.annotated_to_cleaned(text.len() + 1), None);
        assert_eq!(
            output
                .source_map
                .cleaned_to_annotated(output.cleaned.len() + 1),
            None
        );
    }

    #[test]
    fn adjacent_markers_share_cleaned_target() {
        let output = run("// #! First\n// #! Second\nfoo();");
        assert_eq!(output.cleaned, b"foo();");
        assert_eq!(output.annotations[0].target, output.annotations[1].target);
        assert_eq!(output.annotations[0].target, ByteRange { start: 0, end: 6 });
    }

    #[test]
    fn unclosed_literals_do_not_extract_their_contents() {
        for text in [
            "r##\"\n// #! Not comment",
            "\"\n// #! Not comment",
            "/* \n// #! Not comment",
        ] {
            let output = run(text);
            assert!(output.annotations.is_empty());
            assert_eq!(output.cleaned, text.as_bytes());
        }
    }

    #[test]
    fn empty_and_unchanged_source_maps() {
        let output = run("");
        assert_eq!(output.source_map.annotated_to_cleaned(0), Some(0));
        assert_eq!(output.source_map.cleaned_to_annotated(0), Some(0));
        let output = run("let x = 1;\n");
        for offset in 0..=output.cleaned.len() {
            assert_eq!(output.source_map.annotated_to_cleaned(offset), Some(offset));
            assert_eq!(output.source_map.cleaned_to_annotated(offset), Some(offset));
        }
    }
}
