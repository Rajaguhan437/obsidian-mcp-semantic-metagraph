//! Heading-aware chunking for chunk-level semantic retrieval.
//!
//! Upstream embedded one vector per note from `prepare_embed_text`, whose body
//! was truncated to `MAX_BODY_WORDS = 400`. Everything past word 400 was absent
//! from the semantic index (BM25 still saw it, but hybrid weights semantic at
//! `1 - alpha` = 0.75, so the dominant signal came from a truncated document).
//!
//! This module splits a note body into overlapping, heading-scoped chunks and
//! attaches a breadcrumb to each so the embedding keeps its structural context.
//!
//! Headings are detected here rather than reused from `parser::extract_headings`
//! because those offsets are relative to the *full file* including frontmatter,
//! while embedding runs on the frontmatter-stripped body. Detecting locally
//! keeps offsets and text in the same coordinate space.

/// Default chunk target in characters. ~1000 chars is roughly 250 tokens, well
/// inside every candidate model's context (arctic-embed2 is 8192), while small
/// enough that one chunk carries one idea rather than a whole section. Override
/// with `OBSIDIAN_CHUNK_CHARS`.
pub const DEFAULT_CHUNK_CHARS: usize = 1000;

/// Default overlap in characters (20% of target). Enough to keep a sentence
/// that straddles a boundary retrievable from both sides without inflating the
/// index much. Override with `OBSIDIAN_CHUNK_OVERLAP`.
pub const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// Absolute ceiling applied after packing, so a pathological section (a table
/// or a note with no whitespace) can never emit an unbounded chunk.
const HARD_MAX_FACTOR: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0-based position of this chunk within the note.
    pub index: usize,
    /// `"H1 > H2 > H3"` for the section this chunk belongs to; empty above the
    /// first heading.
    pub breadcrumb: String,
    /// Byte offset into the body this chunk starts at.
    pub offset: usize,
    pub text: String,
}

/// Chunking knobs, resolved from the environment once per run.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub target: usize,
    pub overlap: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target: DEFAULT_CHUNK_CHARS,
            overlap: DEFAULT_CHUNK_OVERLAP,
        }
    }
}

impl ChunkConfig {
    pub fn from_env() -> Self {
        let target = std::env::var("OBSIDIAN_CHUNK_CHARS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v >= 100)
            .unwrap_or(DEFAULT_CHUNK_CHARS);
        let overlap = std::env::var("OBSIDIAN_CHUNK_OVERLAP")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_CHUNK_OVERLAP);
        // Overlap must leave forward progress.
        let overlap = overlap.min(target.saturating_sub(1) / 2);
        Self { target, overlap }
    }

    fn hard_max(&self) -> usize {
        self.target.saturating_mul(HARD_MAX_FACTOR)
    }
}

struct Section {
    breadcrumb: String,
    start: usize,
    end: usize,
}

/// Split `body` into heading-delimited sections, tracking the breadcrumb stack.
/// Fenced code blocks are respected so `#` inside a fence is never a heading.
fn sections(body: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = ' ';
    let mut current_start = 0usize;
    let mut current_crumb = String::new();

    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();

        // Fence toggling: ``` or ~~~
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed.as_bytes()[0] as char;
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if marker == fence_marker {
                in_fence = false;
            }
            offset += line.len();
            continue;
        }

        if !in_fence && trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            let rest = &trimmed[hashes..];
            // A real ATX heading needs 1-6 hashes then whitespace.
            if (1..=6).contains(&hashes) && rest.starts_with(char::is_whitespace) {
                // Close the section that ended here.
                if offset > current_start {
                    out.push(Section {
                        breadcrumb: current_crumb.clone(),
                        start: current_start,
                        end: offset,
                    });
                }
                let level = hashes as u8;
                let text = rest.trim().to_string();
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, text));
                current_crumb = stack
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join(" > ");
                current_start = offset;
            }
        }
        offset += line.len();
    }

    if body.len() > current_start {
        out.push(Section {
            breadcrumb: current_crumb,
            start: current_start,
            end: body.len(),
        });
    }
    out
}

/// Find a good split point at or before `limit` within `s`: prefer a paragraph
/// break, then a sentence end, then a newline, then a space. Returns a byte
/// index that always lies on a char boundary.
fn split_point(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut cap = limit;
    while cap > 0 && !s.is_char_boundary(cap) {
        cap -= 1;
    }
    let window = &s[..cap];
    let floor = cap / 2;

    if let Some(p) = window.rfind("\n\n") {
        if p > floor {
            return p + 2;
        }
    }
    for pat in [". ", ".\n", "! ", "? ", "।"] {
        if let Some(p) = window.rfind(pat) {
            if p > floor {
                return p + pat.len();
            }
        }
    }
    if let Some(p) = window.rfind('\n') {
        if p > floor {
            return p + 1;
        }
    }
    if let Some(p) = window.rfind(' ') {
        if p > floor {
            return p + 1;
        }
    }
    cap
}

/// Chunk a note body into overlapping, heading-scoped pieces.
///
/// Guarantees: every byte of `body` is covered by at least one chunk, and no
/// chunk exceeds `config.hard_max()`.
pub fn chunk_note(body: &str, config: ChunkConfig) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for section in sections(body) {
        let raw = &body[section.start..section.end];
        if raw.trim().is_empty() {
            continue;
        }
        let mut cursor = 0usize;
        while cursor < raw.len() {
            let remaining = &raw[cursor..];
            let mut take = split_point(remaining, config.target);
            if take == 0 {
                take = remaining.len();
            }
            if take > config.hard_max() {
                let mut t = config.hard_max();
                while t > 0 && !remaining.is_char_boundary(t) {
                    t -= 1;
                }
                take = t.max(1);
            }
            let piece = &remaining[..take];
            if !piece.trim().is_empty() {
                chunks.push(Chunk {
                    index: chunks.len(),
                    breadcrumb: section.breadcrumb.clone(),
                    offset: section.start + cursor,
                    text: piece.trim().to_string(),
                });
            }
            if cursor + take >= raw.len() {
                break;
            }
            // Step forward, rewinding by the overlap but always progressing.
            let mut next = cursor + take;
            if config.overlap > 0 {
                let mut back = next.saturating_sub(config.overlap).max(cursor + 1);
                while back < next && !raw.is_char_boundary(back) {
                    back += 1;
                }
                next = back.max(cursor + 1);
            }
            cursor = next;
        }
    }
    // Reindex so `index` is dense and note-global.
    for (i, c) in chunks.iter_mut().enumerate() {
        c.index = i;
    }
    chunks
}

/// Text handed to the embedding model for one chunk: title, breadcrumb, body.
/// Mirrors upstream's `"{title}\n{headings}\n{body}"` shape so the two are
/// comparable, but scoped to this chunk's own section rather than the whole note.
pub fn prepare_chunk_embed_text(title: &str, breadcrumb: &str, chunk: &str) -> String {
    if breadcrumb.is_empty() {
        format!("{title}\n{chunk}")
    } else {
        format!("{title}\n{breadcrumb}\n{chunk}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(target: usize, overlap: usize) -> ChunkConfig {
        ChunkConfig { target, overlap }
    }

    #[test]
    fn short_note_is_one_chunk() {
        let c = chunk_note("Just a short body.", cfg(1000, 200));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Just a short body.");
        assert_eq!(c[0].breadcrumb, "");
    }

    #[test]
    fn long_note_is_not_truncated() {
        // The upstream failure mode: a body far past 400 words.
        let body = (0..3000)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_note(&body, cfg(1000, 200));
        assert!(chunks.len() > 10, "expected many chunks, got {}", chunks.len());
        // The tail must be present somewhere.
        let joined = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("word2999"), "tail of the note was lost");
    }

    #[test]
    fn breadcrumbs_track_heading_hierarchy() {
        let body = "# Top\nintro text here\n## Middle\ndeeper text here\n### Leaf\nleaf text here\n";
        let chunks = chunk_note(body, cfg(1000, 200));
        let crumbs: Vec<&str> = chunks.iter().map(|c| c.breadcrumb.as_str()).collect();
        assert!(crumbs.iter().any(|c| *c == "Top"));
        assert!(crumbs.iter().any(|c| *c == "Top > Middle"));
        assert!(crumbs.iter().any(|c| *c == "Top > Middle > Leaf"));
    }

    #[test]
    fn sibling_heading_pops_stack() {
        let body = "## A\natext\n## B\nbtext\n";
        let chunks = chunk_note(body, cfg(1000, 200));
        assert!(chunks.iter().any(|c| c.breadcrumb == "A"));
        assert!(chunks.iter().any(|c| c.breadcrumb == "B"));
        assert!(!chunks.iter().any(|c| c.breadcrumb == "A > B"));
    }

    #[test]
    fn hash_inside_code_fence_is_not_a_heading() {
        let body = "# Real\ntext\n```\n# not a heading\n```\nmore text\n";
        let chunks = chunk_note(body, cfg(1000, 200));
        assert!(chunks.iter().all(|c| c.breadcrumb == "Real"));
    }

    #[test]
    fn no_chunk_exceeds_hard_max() {
        // No whitespace at all - the pathological case for a naive splitter.
        let body = "x".repeat(20_000);
        let config = cfg(1000, 200);
        let chunks = chunk_note(&body, config);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(
                c.text.len() <= config.hard_max(),
                "chunk of {} exceeded hard max {}",
                c.text.len(),
                config.hard_max()
            );
        }
    }

    #[test]
    fn chunking_always_progresses() {
        // Overlap >= target must not loop forever.
        let body = "a b c d e f g h i j k l m n o p q r s t u v w x y z ".repeat(50);
        let chunks = chunk_note(&body, ChunkConfig { target: 200, overlap: 500 });
        assert!(!chunks.is_empty());
        assert!(chunks.len() < 10_000);
    }

    #[test]
    fn embed_text_includes_breadcrumb() {
        let t = prepare_chunk_embed_text("Note", "A > B", "chunk body");
        assert_eq!(t, "Note\nA > B\nchunk body");
        let t2 = prepare_chunk_embed_text("Note", "", "chunk body");
        assert_eq!(t2, "Note\nchunk body");
    }

    #[test]
    fn utf8_multibyte_is_never_split_mid_char() {
        let body = "🧘 discipline ".repeat(400);
        let chunks = chunk_note(&body, cfg(300, 60));
        assert!(!chunks.is_empty());
        // Reaching here without panicking proves boundaries held.
        for c in &chunks {
            assert!(!c.text.is_empty());
        }
    }
}
