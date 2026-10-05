//! Shared fenced-code colors for projected previews and source-preserving editors.
//! Highlighting is derived paint: it never changes text, fonts, layout or source maps.
use crate::markdown::{Block, Content, Document, Id, inline::RichText};
use sanscale::PaintSpan;
use std::{collections::HashMap, sync::Arc};

/// Above these bounds code stays plain, selectable and copyable. Limits apply
/// before invoking the regex engine, including on an unclosed streaming fence.
pub const MAX_CODE_BYTES: usize = 512 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub struct Highlighter {
    blocks: HashMap<Id, CachedBlock>,
}
struct CachedBlock {
    language: String,
    text: Vec<Arc<RichText>>,
    spans: Vec<Vec<PaintSpan>>,
}
impl Highlighter {
    pub fn clear(&mut self) { self.blocks.clear(); }

    pub fn retain(&mut self, doc: &Document) {
        let live: std::collections::HashSet<_> = doc.blocks().iter().map(|b| b.id).collect();
        self.blocks.retain(|id, _| live.contains(id));
    }

    /// Line-local byte spans. Whole-fence parsing keeps multiline comments and
    /// strings correct; unchanged fences reuse their paint, including on reflow.
    pub fn block(&mut self, block: &Block) -> &[Vec<PaintSpan>] {
        let Content::Code { language, lines, .. } = &block.content else { return &[]; };
        let cached = self.blocks.get(&block.id).is_some_and(|c| {
            c.language == *language && c.text.len() == lines.len()
                && c.text.iter().zip(lines).all(|(a, b)| Arc::ptr_eq(a, &b.rich))
        });
        if !cached {
            let text: Vec<_> = lines.iter().map(|e| e.rich.clone()).collect();
            let spans = highlight(language, &text.iter().map(|r| r.text.as_str()).collect::<Vec<_>>());
            self.blocks.insert(block.id, CachedBlock { language: language.clone(), text, spans });
        }
        &self.blocks[&block.id].spans
    }

    /// Original-source byte spans. Fence markers/prose stay untouched; callers
    /// can color editable Markdown without introducing a second caret geometry.
    pub fn source_spans(&mut self, doc: &Document) -> Vec<PaintSpan> {
        self.retain(doc);
        let mut spans = Vec::new();
        for block in doc.blocks() {
            let Content::Code { lines, .. } = &block.content else { continue; };
            for (line, paints) in lines.iter().zip(self.block(block)) {
                let Some(offset) = doc.resolve(line.origin_at(0)) else { continue; };
                spans.extend(paints.iter().map(|p| PaintSpan {
                    range: offset + p.range.start..offset + p.range.end,
                    color: p.color,
                }));
            }
        }
        spans
    }
}

/// Unknown/omitted languages and over-budget input have no overrides. The
/// optional syntax feature uses only Rust dependencies (no Oniguruma/C build).
pub fn highlight(language: &str, lines: &[&str]) -> Vec<Vec<PaintSpan>> {
    let plain = || vec![Vec::new(); lines.len()];
    if lines.iter().any(|s| s.len() > MAX_LINE_BYTES)
        || lines.iter().map(|s| s.len().saturating_add(1)).sum::<usize>() > MAX_CODE_BYTES
    { return plain(); }
    #[cfg(not(feature = "syntax"))]
    { let _ = language; plain() }
    #[cfg(feature = "syntax")]
    {
        use std::sync::OnceLock;
        use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};
        static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
        static THEMES: OnceLock<ThemeSet> = OnceLock::new();
        let name = language.split_whitespace().next().unwrap_or("")
            .trim_matches(['{', '}', '.']).split(',').next().unwrap_or("").to_ascii_lowercase();
        let name = match name.as_str() {
            "rs" => "rust", "py" | "py3" | "python3" => "python",
            "js" | "jsx" => "javascript", "sh" | "shell" | "console" => "bash",
            "yml" => "yaml", "c++" => "cpp", "c#" => "cs", other => other,
        };
        if name.is_empty() || matches!(name, "text" | "txt" | "plain" | "plaintext") { return plain(); }
        let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
        let Some(syntax) = syntaxes.find_syntax_by_token(name) else { return plain(); };
        let themes = THEMES.get_or_init(ThemeSet::load_defaults);
        let mut highlighter = HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
        let mut result = Vec::with_capacity(lines.len());
        for line in lines {
            // The bundled grammars expect LF, even though Elements omit it.
            let input = format!("{line}\n");
            let Ok(runs) = highlighter.highlight_line(&input, syntaxes) else { return plain(); };
            let mut spans: Vec<PaintSpan> = Vec::new();
            let mut offset = 0;
            for (style, token) in runs {
                let end = (offset + token.len()).min(line.len());
                if offset < end {
                    let c = style.foreground;
                    let color = sanscale::Color([linear(c.r), linear(c.g), linear(c.b), c.a as f32 / 255.]);
                    if let Some(last) = spans.last_mut().filter(|s| s.color == color && s.range.end == offset) {
                        last.range.end = end;
                    } else { spans.push(PaintSpan { range: offset..end, color }); }
                }
                offset += token.len();
            }
            result.push(spans);
        }
        result
    }
}
#[cfg(feature = "syntax")]
fn linear(v: u8) -> f32 {
    let v = v as f32 / 255.;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_unknown_and_large_code_are_plain() {
        for language in ["", "text", "not-a-real-language"] {
            assert!(highlight(language, &["fn main() {}"]).iter().all(Vec::is_empty));
        }
        let large = "x".repeat(MAX_LINE_BYTES + 1);
        assert!(highlight("rust", &[&large])[0].is_empty());
        assert!(highlight("rust", &vec!["xxxxxxxx"; MAX_CODE_BYTES / 8]).iter().all(Vec::is_empty));
    }
    #[test]
    #[cfg(feature = "syntax")]
    fn languages_aliases_multiline_and_unicode() {
        for (language, source) in [("RUST,ignore", "fn main() { let s = \"🦀\"; }"),
            ("py", "def hello(): return 'café'"), ("js", "const x = '🐶';"),
            ("json", "{\"x\": 42}"), ("sh", "echo \"$HOME\"")] {
            let spans = highlight(language, &[source]);
            assert!(spans[0].len() > 1, "{language}");
            for span in &spans[0] { assert!(source.get(span.range.clone()).is_some()); }
        }
        let spans = highlight("rust", &["/* café", "still comment */", "fn main() {}"]);
        assert_eq!(spans[0][0].color, spans[1][0].color);
        assert_ne!(spans[1][0].color, spans[2][0].color);
    }
    #[test]
    #[cfg(feature = "syntax")]
    fn streaming_and_edits_match_cold_source_paint_without_touching_markdown() {
        let source = "# Prose\n\n```rust\n/* café\nstill comment */\nfn main() {}\n```\n";
        let mut doc = Document::default();
        let mut highlighter = Highlighter::default();
        for c in source.chars() {
            doc.append(&c.to_string()).unwrap();
            let cold = Document::new(&doc.source().to_string());
            assert_eq!(highlighter.source_spans(&doc), Highlighter::default().source_spans(&cold));
        }
        assert_eq!(doc.source().to_string(), source);
        let before = highlighter.source_spans(&doc);
        assert!(before.iter().all(|p| p.range.start >= source.find("/*").unwrap()));
        assert!(before.iter().all(|p| source.get(p.range.clone()).is_some()));
        doc.edit(source.find("rust").unwrap()..source.find("rust").unwrap()+4, "text").unwrap();
        assert!(highlighter.source_spans(&doc).is_empty());
        doc.edit(0..doc.source().len_bytes(), "prose").unwrap();
        assert!(highlighter.source_spans(&doc).is_empty());
        assert!(highlighter.blocks.is_empty());
    }
}
