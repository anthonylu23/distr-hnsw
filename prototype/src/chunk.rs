#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ordinal: usize,
    pub text: String,
    pub char_start: usize,
    pub char_end: usize,
}

/// Split text into overlapping character windows.
///
/// `chunk_chars` is the target window size; `overlap` characters are shared
/// between consecutive windows. Boundaries prefer whitespace near the end of
/// the window when available.
pub fn chunk_text(text: &str, chunk_chars: usize, overlap: usize) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chunk_chars == 0 {
        return Vec::new();
    }
    let overlap = overlap.min(chunk_chars.saturating_sub(1));
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut ordinal = 0usize;

    while start < chars.len() {
        let mut end = (start + chunk_chars).min(chars.len());
        if end < chars.len() {
            if let Some(rel) = chars[start..end].iter().rposition(|c| c.is_whitespace()) {
                // Prefer breaking on whitespace if we keep at least half a chunk.
                if rel + 1 >= chunk_chars / 2 {
                    end = start + rel + 1;
                }
            }
        }
        if end <= start {
            end = (start + chunk_chars).min(chars.len());
        }
        let slice: String = chars[start..end].iter().collect();
        let trimmed = slice.trim();
        if !trimmed.is_empty() {
            // Adjust start/end to trimmed content within the window.
            let leading = slice.len() - slice.trim_start().len();
            let trailing = slice.len() - slice.trim_end().len();
            let char_start = start + slice[..leading].chars().count();
            let char_end = end - slice[slice.len() - trailing..].chars().count();
            out.push(Chunk {
                ordinal,
                text: trimmed.to_string(),
                char_start,
                char_end,
            });
            ordinal += 1;
        }
        if end >= chars.len() {
            break;
        }
        let next = end.saturating_sub(overlap);
        start = if next <= start { end } else { next };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_for_short_text() {
        let chunks = chunk_text("hello world", 100, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].ordinal, 0);
    }

    #[test]
    fn overlap_produces_multiple_chunks() {
        let text = "word ".repeat(100);
        let chunks = chunk_text(&text, 50, 10);
        assert!(chunks.len() > 1);
        for window in chunks.windows(2) {
            assert!(window[1].char_start < window[0].char_end);
            assert_eq!(window[1].ordinal, window[0].ordinal + 1);
        }
    }

    #[test]
    fn empty_input() {
        assert!(chunk_text("", 100, 10).is_empty());
    }
}
