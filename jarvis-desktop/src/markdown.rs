//! Markdown → TranscriptEntry list with inline-emphasis support.
//!
//! Block-level: headers (#/##/###), bullets (- / *), fenced code blocks,
//! whole-paragraph bold (**block**), plain paragraphs.
//!
//! Inline: paragraphs that contain mid-sentence **bold** / `code` /
//! [link](url) runs get **split into sub-blocks** at the emphasis
//! boundaries, each sub-block carrying its style flags. The Slint
//! renderer stacks consecutive same-kind sub-blocks tightly so they
//! read as one paragraph rather than three separate bubbles. Slint
//! 1.x doesn't support mid-line styled spans in a single Text, so
//! splitting at boundaries is the cleanest path to actually rendering
//! "the **fast** path is sane" with the right word bolded.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Block {
    pub body: String,
    pub header_rank: i32,
    pub is_code: bool,
    pub is_bullet: bool,
    pub is_bold: bool,
    /// True if this block is a mid-paragraph continuation of the prior
    /// block. The UI tightens the spacing and drops the bubble border
    /// so it visually reads as one paragraph.
    pub continues_prev: bool,
}

pub fn parse(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut iter = text.lines().peekable();
    let mut paragraph: Vec<String> = Vec::new();

    let flush_paragraph = |paragraph: &mut Vec<String>, out: &mut Vec<Block>| {
        if paragraph.is_empty() {
            return;
        }
        let joined = paragraph.join("\n");
        let trimmed = joined.trim().to_string();
        if trimmed.is_empty() {
            paragraph.clear();
            return;
        }
        // Whole-paragraph bold: **... ... ...**
        let whole_bold = trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4;
        if whole_bold {
            let inner = &trimmed[2..trimmed.len() - 2];
            for sub in split_inline_runs(inner) {
                let mut b = sub;
                b.is_bold = true;
                if !out.is_empty() {
                    // Mark continuation only after the first run of
                    // this paragraph.
                    b.continues_prev = false;
                }
                out.push(b);
            }
        } else {
            for (i, sub) in split_inline_runs(&trimmed).into_iter().enumerate() {
                let mut b = sub;
                b.continues_prev = i > 0;
                out.push(b);
            }
        }
        paragraph.clear();
    };

    while let Some(line) = iter.next() {
        // Fenced code block. Consume until next ``` line.
        if line.trim_start().starts_with("```") {
            flush_paragraph(&mut paragraph, &mut out);
            let mut code: Vec<String> = Vec::new();
            for inner in iter.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
                code.push(inner.to_string());
            }
            out.push(Block {
                body: code.join("\n"),
                is_code: true,
                ..Default::default()
            });
            continue;
        }

        let trimmed = line.trim_start();

        // Headers
        if let Some(rest) = trimmed.strip_prefix("### ") {
            flush_paragraph(&mut paragraph, &mut out);
            out.push(Block {
                body: strip_inline_emphasis(rest),
                header_rank: 3,
                ..Default::default()
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush_paragraph(&mut paragraph, &mut out);
            out.push(Block {
                body: strip_inline_emphasis(rest),
                header_rank: 2,
                ..Default::default()
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush_paragraph(&mut paragraph, &mut out);
            out.push(Block {
                body: strip_inline_emphasis(rest),
                header_rank: 1,
                ..Default::default()
            });
            continue;
        }

        // Bullets
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            flush_paragraph(&mut paragraph, &mut out);
            out.push(Block {
                body: strip_inline_emphasis(rest),
                is_bullet: true,
                ..Default::default()
            });
            continue;
        }

        // Blank line ends a paragraph.
        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut out);
            continue;
        }

        // Otherwise: accumulate.
        paragraph.push(line.to_string());
    }
    flush_paragraph(&mut paragraph, &mut out);

    if out.is_empty() {
        out.push(Block {
            body: text.trim().to_string(),
            ..Default::default()
        });
    }
    out
}

/// Split a single-paragraph string into a sequence of styled sub-blocks,
/// preserving bold (`**...**`), inline code (`` `...` ``), and link
/// text. Adjacent plain text gets coalesced. Returned blocks are
/// kind-agnostic — caller annotates as user/jarvis/etc.
fn split_inline_runs(s: &str) -> Vec<Block> {
    let mut runs: Vec<Block> = Vec::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();
    let flush_plain = |buf: &mut String, runs: &mut Vec<Block>| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        runs.push(Block {
            body: text,
            ..Default::default()
        });
    };
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                // **bold** boundary.
                chars.next(); // consume the second *
                flush_plain(&mut buf, &mut runs);
                let mut bold = String::new();
                let mut closed = false;
                while let Some(b) = chars.next() {
                    if b == '*' && chars.peek() == Some(&'*') {
                        chars.next();
                        closed = true;
                        break;
                    }
                    bold.push(b);
                }
                if !bold.is_empty() {
                    runs.push(Block {
                        body: bold,
                        is_bold: true,
                        ..Default::default()
                    });
                }
                if !closed {
                    // Unmatched ** — treat as literal.
                }
            }
            '`' => {
                flush_plain(&mut buf, &mut runs);
                let mut code = String::new();
                for b in chars.by_ref() {
                    if b == '`' {
                        break;
                    }
                    code.push(b);
                }
                if !code.is_empty() {
                    runs.push(Block {
                        body: code,
                        is_code: true,
                        ..Default::default()
                    });
                }
            }
            '[' => {
                // [text](url) — keep text only.
                flush_plain(&mut buf, &mut runs);
                let mut link_text = String::new();
                for b in chars.by_ref() {
                    if b == ']' {
                        break;
                    }
                    link_text.push(b);
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    for b in chars.by_ref() {
                        if b == ')' {
                            break;
                        }
                    }
                }
                if !link_text.is_empty() {
                    runs.push(Block {
                        body: link_text,
                        // Underline lives in a future Slint widget pass;
                        // for now links read as plain text.
                        ..Default::default()
                    });
                }
            }
            _ => buf.push(c),
        }
    }
    flush_plain(&mut buf, &mut runs);
    if runs.is_empty() {
        runs.push(Block {
            body: s.to_string(),
            ..Default::default()
        });
    }
    runs
}

/// Legacy: drop **, *, ` from a string. Kept for headers and bullets
/// where we currently render the whole line as one styled chunk; the
/// per-run splitter above is the modern path.
fn strip_inline_emphasis(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            // Eat an optional second '*' for **bold** pairs.
            if chars.peek() == Some(&'*') {
                chars.next();
            }
            continue;
        }
        if c == '`' {
            continue;
        }
        // [link text](url) → link text
        if c == '[' {
            let mut link_text = String::new();
            for inner in chars.by_ref() {
                if inner == ']' {
                    break;
                }
                link_text.push(inner);
            }
            // Skip the (url) portion if it follows immediately.
            if chars.peek() == Some(&'(') {
                chars.next();
                for inner in chars.by_ref() {
                    if inner == ')' {
                        break;
                    }
                }
            }
            out.push_str(&link_text);
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_h1() {
        let blocks = parse("# Title\n\nbody text");
        assert_eq!(blocks[0].header_rank, 1);
        assert_eq!(blocks[0].body, "Title");
        assert_eq!(blocks[1].body, "body text");
    }

    #[test]
    fn bullets_unordered() {
        let blocks = parse("- one\n- two\n- three");
        assert_eq!(blocks.len(), 3);
        assert!(blocks.iter().all(|b| b.is_bullet));
    }

    #[test]
    fn fenced_code_block() {
        let blocks = parse("```\nfn main() {}\n```");
        assert!(blocks[0].is_code);
        assert_eq!(blocks[0].body, "fn main() {}");
    }

    #[test]
    fn inline_emphasis_stripped() {
        let blocks = parse("hello **world** and `code` too");
        assert_eq!(blocks[0].body, "hello world and code too");
    }

    #[test]
    fn link_text_only() {
        let blocks = parse("see [the docs](https://example.com) please");
        assert_eq!(blocks[0].body, "see the docs please");
    }
}
