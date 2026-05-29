//! Streaming sentence splitter + TTS-oriented markdown stripper.
//!
//! Pure logic, no Slint or tokio dependencies. Used by the SSE consumer
//! to turn streaming chunks into spoken sentences as soon as a complete
//! sentence boundary is observed, instead of waiting for the full
//! response. Mirror of `voice::strip_markdown_for_tts` and
//! `SentenceSplitter` on the Leptos dashboard.

/// A completed sentence the splitter emitted. `ends_paragraph` is true
/// when the trailing whitespace after the boundary contained at least
/// one blank line OR ended a list item — TTS uses this to insert a
/// longer breath between paragraphs vs. between sentences within a
/// paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    pub text: String,
    pub ends_paragraph: bool,
}

/// Streaming sentence splitter. Feed it streamed chunks via `push`;
/// returns complete sentences as soon as their boundary lands. Decimal
/// numbers (1.5, 3.14) don't trigger a split. Paragraph boundaries
/// (`\n\n`) get flagged on the trailing sentence so TTS can hold a
/// longer pause.
pub struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self {
            buf: String::with_capacity(512),
        }
    }

    /// Append `text` to the internal buffer; return any sentences that
    /// completed as a result, each annotated with whether it ended a
    /// paragraph.
    pub fn push(&mut self, text: &str) -> Vec<Sentence> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        loop {
            let Some(idx) = find_sentence_boundary(&self.buf) else {
                break;
            };
            let (head, rest) = self.buf.split_at(idx + 1);
            // Paragraph detection: count newlines in the whitespace
            // run that follows the boundary. ≥2 newlines = blank line
            // = paragraph break. Saves us from having to look at the
            // source markup; whatever produced the streaming chunks
            // already inserted the blank lines if a paragraph
            // boundary was intended.
            let leading_ws: String = rest
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            let ends_paragraph = leading_ws.matches('\n').count() >= 2;
            let s = head.trim().to_string();
            let tail: String = rest[leading_ws.len()..].to_string();
            if !s.is_empty() {
                out.push(Sentence {
                    text: s,
                    ends_paragraph,
                });
            }
            self.buf = tail;
        }
        out
    }

    /// Flush whatever's left in the buffer as a final (potentially
    /// boundary-less) chunk. Called when the SSE stream signals end.
    pub fn finish(&mut self) -> Option<Sentence> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        if s.is_empty() {
            None
        } else {
            // Final flush is conceptually the end of the message —
            // treat as paragraph-end so trailing breath is generous.
            Some(Sentence {
                text: s,
                ends_paragraph: true,
            })
        }
    }

    /// Discard any in-progress sentence. Called on barge-in stop so the
    /// next SSE chunk doesn't immediately get re-spoken.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

fn find_sentence_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?') {
            let next = bytes.get(i + 1).copied().unwrap_or(b' ');
            if next.is_ascii_whitespace() || i + 1 == bytes.len() {
                // Skip decimals like 1.5
                if i > 0 && bytes[i - 1].is_ascii_digit() {
                    if let Some(&n) = bytes.get(i + 1) {
                        if n.is_ascii_digit() {
                            continue;
                        }
                    }
                }
                return Some(i);
            }
        }
    }
    None
}

/// Strip a minimal set of markdown markers so TTS doesn't read them
/// aloud. Drops fenced code blocks entirely; strips leading bullets,
/// headers, and inline `**bold**` / `*italic*` / `` `code` `` markers.
pub fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        // Strip leading # headers and - / * bullets.
        let trimmed = line.trim_start();
        let stripped = if let Some(rest) = trimmed.strip_prefix("# ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("## ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("### ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("- ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("* ") {
            rest
        } else {
            trimmed
        };
        // Remove inline **bold** / *italic* / `code`.
        let mut cleaned = String::with_capacity(stripped.len());
        for c in stripped.chars() {
            if c == '*' || c == '_' || c == '`' {
                continue;
            }
            cleaned.push(c);
        }
        out.push_str(&cleaned);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Sentence {
        Sentence {
            text: text.to_string(),
            ends_paragraph: false,
        }
    }
    fn p(text: &str) -> Sentence {
        Sentence {
            text: text.to_string(),
            ends_paragraph: true,
        }
    }

    #[test]
    fn splitter_emits_on_period_space() {
        let mut sp = SentenceSplitter::new();
        let r = sp.push("Hello world. ");
        assert_eq!(r, vec![s("Hello world.")]);
    }

    #[test]
    fn splitter_decimals_dont_trigger() {
        let mut sp = SentenceSplitter::new();
        let r = sp.push("Pi is 3.14 ");
        assert!(r.is_empty(), "expected no split, got {:?}", r);
        let r = sp.push("approximately. Next sentence.");
        assert_eq!(
            r,
            vec![s("Pi is 3.14 approximately."), s("Next sentence.")]
        );
    }

    #[test]
    fn splitter_finish_returns_tail() {
        let mut sp = SentenceSplitter::new();
        sp.push("Incomplete sentence");
        assert_eq!(sp.finish(), Some(p("Incomplete sentence")));
        assert_eq!(sp.finish(), None);
    }

    #[test]
    fn splitter_reset_clears_buffer() {
        let mut sp = SentenceSplitter::new();
        sp.push("In-progress");
        sp.reset();
        assert_eq!(sp.finish(), None);
    }

    #[test]
    fn splitter_marks_paragraph_break() {
        let mut sp = SentenceSplitter::new();
        // "...one.\n\nTwo..." — first sentence ends a paragraph.
        let r = sp.push("First sentence.\n\nSecond sentence.");
        assert_eq!(r, vec![p("First sentence."), s("Second sentence.")]);
    }

    #[test]
    fn splitter_single_newline_is_not_paragraph_break() {
        let mut sp = SentenceSplitter::new();
        // Soft wrap: single newline = same paragraph.
        let r = sp.push("Line one.\nLine two.");
        assert_eq!(r, vec![s("Line one."), s("Line two.")]);
    }

    #[test]
    fn strip_markdown_removes_code_fences() {
        let input = "Hello\n```rust\nlet x = 1;\n```\nWorld";
        assert_eq!(strip_markdown(input), "Hello\nWorld\n");
    }

    #[test]
    fn strip_markdown_removes_inline_markers() {
        let input = "This is **bold** and *italic* and `code`";
        assert_eq!(
            strip_markdown(input),
            "This is bold and italic and code\n"
        );
    }

    #[test]
    fn strip_markdown_strips_headers_and_bullets() {
        let input = "# Title\n## Sub\n- bullet one\n* bullet two";
        assert_eq!(
            strip_markdown(input),
            "Title\nSub\nbullet one\nbullet two\n"
        );
    }
}
