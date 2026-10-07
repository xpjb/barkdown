//! Window-independent sanscale adapter. Layout, projected/source coordinates,
//! table dependency tracking, and paint ownership are separate from the demo UI.
use super::markdown::{
    self as md, Content, Document, Element, Id, Origin, RawLine, TextKind,
    inline::{self, RichText},
};
use sanscale::{
    Align, BlockKey, Color, Draw, FontChainHandle, FontSpan, PaintHandle, PaintSpan, ParagraphKey,
    ParagraphSource, Rect, ShapedHandle, Style, TextService, Vec2,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Faces {
    pub prose: [FontChainHandle; 4],
    pub mono: [FontChainHandle; 4],
}
impl Faces {
    fn face(self, flags: u8) -> FontChainHandle {
        let index = (flags & (inline::STRONG | inline::EMPHASIS)) as usize;
        if flags & inline::CODE != 0 {
            self.mono[index]
        } else {
            self.prose[index]
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub alternate: bool,
    pub italic: bool,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            alternate: false,
            italic: true,
        }
    }
}
impl Theme {
    pub fn foreground(self) -> Color {
        // Tau 1's muted onSurfaceVariant (#B7C2CE), in linear GPU color space.
        Color([0.473_531_5, 0.539_479_5, 0.617_206_6, 1.])
    }
    fn role(self, flags: u8) -> Color {
        match flags {
            f if f & inline::LINK != 0 => {
                if self.alternate {
                    Color([0.96, 0.53, 0.34, 1.])
                } else {
                    Color([0.30, 0.66, 0.96, 1.])
                }
            }
            f if f & inline::CODE != 0 => {
                if self.alternate {
                    Color([0.79, 0.63, 0.96, 1.])
                } else {
                    Color([0.48, 0.83, 0.69, 1.])
                }
            }
            f if f & inline::IMAGE != 0 => Color([0.68, 0.62, 0.48, 1.]),
            _ => self.foreground(),
        }
    }
    pub fn accent(self) -> Color {
        self.role(inline::LINK)
    }
    fn panel(self) -> Color {
        Color([0.025, 0.036, 0.055, 1.])
    }
    fn grid(self) -> Color {
        Color([0.11, 0.16, 0.22, 1.])
    }
}
#[derive(Clone, Copy, Default, Debug)]
pub struct Work {
    pub paint_snapshots: usize,
    pub resolved_elements: usize,
    pub layout_requests: usize,
    pub measured_rows: usize,
    pub indexed_rows: usize,
}

/// Dynamic prefix sums: a changed cell updates one row height, not the Y
/// position of every subsequent row. Appending rows is O(log rows); inserting
/// or deleting rows in the middle rebuilds this *metadata*, not their layouts.
#[derive(Default)]
struct Heights {
    values: Vec<f32>,
    tree: Vec<f32>,
}
impl Heights {
    fn from(values: Vec<f32>) -> Self {
        let mut h = Self::default();
        for x in values {
            h.push(x);
        }
        h
    }
    fn prefix(&self, mut count: usize) -> f32 {
        let mut sum = 0.;
        while count > 0 {
            sum += self.tree[count - 1];
            count &= count - 1;
        }
        sum
    }
    fn total(&self) -> f32 {
        self.prefix(self.values.len())
    }
    fn push(&mut self, value: f32) {
        let n = self.values.len() + 1;
        let low = n & n.wrapping_neg();
        let sum = self.prefix(n - 1) - self.prefix(n - low) + value;
        self.values.push(value);
        self.tree.push(sum);
    }
    fn set(&mut self, index: usize, value: f32) {
        let delta = value - self.values[index];
        self.values[index] = value;
        let mut i = index + 1;
        while i <= self.tree.len() {
            self.tree[i - 1] += delta;
            i += i & i.wrapping_neg();
        }
    }
    fn row_at(&self, y: f32) -> usize {
        let mut index = 0;
        let mut sum = 0.;
        let mut step = self.tree.len().next_power_of_two();
        while step > 0 {
            let next = index + step;
            if next <= self.tree.len() && sum + self.tree[next - 1] <= y {
                index = next;
                sum += self.tree[next - 1];
            }
            step >>= 1;
        }
        index.min(self.values.len())
    }
    fn splice(&mut self, range: Range<usize>, count: usize) -> usize {
        if range.start == self.values.len() {
            for _ in 0..count {
                self.push(0.);
            }
            return count;
        }
        if range.len() == count {
            for i in range {
                self.set(i, 0.);
            }
            return count;
        }
        let mut values = std::mem::take(&mut self.values);
        values.splice(range, std::iter::repeat_n(0., count));
        let work = values.len();
        *self = Self::from(values);
        work
    }
}
struct Source<'a> {
    rich: &'a RichText,
    fonts: &'a [FontSpan],
    parts: Vec<Range<usize>>,
}
impl<'a> Source<'a> {
    fn new(rich: &'a RichText, fonts: &'a [FontSpan]) -> Self {
        let mut start = 0;
        let parts = rich
            .text
            .split('\n')
            .map(|s| {
                let r = start..start + s.len();
                start += s.len() + 1;
                r
            })
            .collect();
        Self { rich, fonts, parts }
    }
}
impl ParagraphSource for Source<'_> {
    fn paragraph_text(&self, i: usize, _: ParagraphKey) -> Option<Cow<'_, str>> {
        self.parts
            .get(i)
            .map(|r| Cow::Borrowed(&self.rich.text[r.clone()]))
    }
    fn paragraph_fonts(&self, i: usize, _: ParagraphKey) -> Cow<'_, [FontSpan]> {
        let r = &self.parts[i];
        Cow::Owned(
            self.fonts[self.fonts.partition_point(|s| s.range.end <= r.start)..]
                .iter().take_while(|s| s.range.start < r.end)
                .filter_map(|s| {
                    let a = s.range.start.max(r.start);
                    let b = s.range.end.min(r.end);
                    (a < b).then(|| FontSpan {
                        range: a - r.start..b - r.start,
                        chain: s.chain,
                    })
                })
                .collect(),
        )
    }
}
// Paragraph identities describe actual text/font inputs, not the enclosing
// Markdown element. Editing one line must not reshape an entire prose group.
struct CachedParagraph {
    key: ParagraphKey,
    origin: Origin,
    range: Range<usize>,
    fonts: Vec<FontSpan>,
    top_em: f32,
}
struct PreparedSource<'a> { rich: &'a RichText, paragraphs: &'a [CachedParagraph] }
impl ParagraphSource for PreparedSource<'_> {
    fn paragraph_text(&self, i: usize, _: ParagraphKey) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(&self.rich.text[self.paragraphs.get(i)?.range.clone()]))
    }
    fn paragraph_fonts(&self, i: usize, _: ParagraphKey) -> Cow<'_, [FontSpan]> {
        Cow::Borrowed(&self.paragraphs[i].fonts)
    }
}

/// Compact is the chat presentation; PreserveSource retains authored empty
/// paragraphs and groups compatible prose in ordinary Sanscale blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSpacing { Compact, PreserveSource }

struct ProseInput {
    lines: Vec<(Id, u32)>,
    rich: Arc<RichText>,
    origins: Vec<RawLine>,
}

struct TextCache {
    rich: Arc<RichText>,
    origins: Vec<RawLine>,
    fonts: Vec<FontSpan>,
    paint_spans: Vec<PaintSpan>,
    syntax_spans: Vec<PaintSpan>,
    math: Option<crate::math::Layout>,
    paint: Option<PaintHandle>,
    handle: ShapedHandle,
    style: Style,
    paragraphs: Vec<CachedParagraph>,
    size: f32,
    height: f32,
    flags: u8,
    color: Color,
    faces: Faces,
    theme: Theme,
}
impl TextCache {
    fn shape(&mut self, text: &mut TextService, key: u64, work: &mut Work) {
        let source = PreparedSource { rich: &self.rich, paragraphs: &self.paragraphs };
        let keys: Vec<_> = self.paragraphs.iter().map(|p| p.key).collect();
        self.handle = text.shape(BlockKey(key), &self.style, &keys, &source)
            .expect("Markdown adapter emits valid grapheme-aligned font spans");
        let layout = text.measure(self.handle);
        for p in &mut self.paragraphs {
            p.top_em = layout.caret_rect(layout.caret_at(p.range.start)).y_em;
        }
        self.height = self.math.as_ref().map_or_else(|| text.measure(self.handle).height_em(), |m| m.height()) * self.size;
        work.layout_requests += 1;
    }
}
#[derive(Clone, Copy)]
struct Part {
    id: Id,
    x: f32,
    y: f32,
}
struct TableCache {
    align: Vec<md::Alignment>,
    width: f32,
    rows: Vec<Id>,
    heights: Heights,
}
enum LayoutContent {
    Parts {
        items: Vec<Part>,
        quote: bool,
        code: bool,
        marker: Option<String>,
    },
    Table(TableCache),
    Rule,
}
struct BlockCache {
    y: f32,
    height: f32,
    content: LayoutContent,
}
#[derive(Clone, Copy, Debug)]
pub struct Decoration {
    pub rect: Rect,
    pub color: Color,
}
#[derive(Default)]
pub struct Scene {
    pub draws: Vec<Draw>,
    pub under: Vec<Decoration>,
    pub over: Vec<Decoration>,
    placed: Vec<(Id, Vec2)>,
}

/// A view reserves BlockKey/ParagraphKey namespace `(namespace << 32) | id`.
/// Give separate documents/views distinct nonzero namespaces. Explicit `release`
/// frees its paint snapshots before discarding the view or replacing its document.
pub struct Preview {
    namespace: u32,
    document: Option<u64>,
    texts: HashMap<Id, TextCache>,
    syntax: crate::syntax::Highlighter,
    blocks: HashMap<Id, BlockCache>,
    order: Vec<Id>,
    revision: Option<u64>,
    config: Option<(Faces, Theme, f32, f32)>,
    next_generation: u32,
    body_style: Option<Style>,
    spacing: BlockSpacing,
    source_mode: bool,
    prose: HashMap<Id, ProseInput>,
    pub height: f32,
    pub width: f32,
    pub last_work: Work,
}
fn ordinary(block: &md::Block) -> bool {
    matches!(&block.content, Content::Text { kind: TextKind::Paragraph, element } if element.rich.math.is_none())
}

fn prose_input(doc: &Document, range: Range<usize>) -> ProseInput {
    let mut rich = RichText::default(); let mut origins = Vec::new(); let mut raw_offset = 0;
    let mut i = range.start;
    let mut b = doc.blocks().partition_point(|b| b.lines.end <= i);
    while i < range.end {
        if i > range.start { rich.append(&RichText::literal("\n"), raw_offset); raw_offset += 1; }
        if let Some(block) = doc.blocks().get(b).filter(|b| b.lines.start == i) {
            let Content::Text { element, .. } = &block.content else { unreachable!() };
            rich.append(&element.rich, raw_offset);
            origins.extend(element.origins.iter().map(|o| RawLine { offset: o.offset+raw_offset, origin: o.origin }));
            raw_offset += element.raw_len(); i = block.lines.end; b += 1;
        } else {
            let raw = doc.line_text(i).unwrap();
            origins.push(RawLine { offset: raw_offset, origin: Origin { line: doc.line_key(i).unwrap().0, column: 0 } });
            rich.append(&RichText::literal(raw), raw_offset);
            raw_offset += raw.len(); i += 1;
        }
    }
    ProseInput { lines: range.map(|i| doc.line_key(i).unwrap()).collect(), rich: Arc::new(rich), origins }
}

fn source_input(doc: &Document) -> ProseInput {
    let raw = doc.source().to_string();
    let mut rich = RichText::literal(&raw);
    rich.runs.clear();
    let mut offset = 0; let mut end = 0; let mut origins = Vec::new();
    for (i, ranges) in source_code_ranges(doc, &raw).into_iter().enumerate() {
        origins.push(RawLine { offset, origin: Origin { line: doc.line_key(i).unwrap().0, column: 0 } });
        for r in ranges {
            let a = offset+r.start; let b = offset+r.end;
            if end < a { rich.runs.push(inline::Run { range: end..a, flags: 0 }); }
            rich.runs.push(inline::Run { range: a..b, flags: inline::CODE }); end = b;
        }
        offset += doc.line_text(i).unwrap().len()+1;
    }
    if end < raw.len() { rich.runs.push(inline::Run { range: end..raw.len(), flags: 0 }); }
    ProseInput { lines: Vec::new(), rich: Arc::new(rich), origins }
}

fn source_code_ranges(doc: &Document, raw: &str) -> Vec<Vec<Range<usize>>> {
    use crate::markdown::{Content, Origin, inline::CODE};
    let lines: Vec<_> = raw.split('\n').collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in &lines { starts.push(offset); offset += line.len()+1; }
    let mut ranges = vec![Vec::<Range<usize>>::new(); lines.len()];
    for block in doc.blocks() {
        if matches!(block.content, Content::Code { .. }) {
            for i in block.lines.clone() {
                if let Some(line) = lines.get(i).filter(|line| !line.is_empty()) { ranges[i].push(0..line.len()); }
            }
            continue;
        }
        for element in block.elements() {
            let resolve = |byte| {
                let index = element.origins.partition_point(|line| line.offset <= byte).saturating_sub(1);
                let line = element.origins.get(index)?;
                doc.resolve(Origin { line: line.origin.line, column: line.origin.column+byte-line.offset })
            };
            for run in element.rich.runs.iter().filter(|r| r.flags & CODE != 0) {
                let first = element.rich.mapping.partition_point(|m| m.display.end <= run.range.start);
                for m in element.rich.mapping[first..].iter().take_while(|m| m.display.start < run.range.end) {
                    let a = m.display.start.max(run.range.start);
                    let b = m.display.end.min(run.range.end);
                    let source = if m.exact { m.source.start+a-m.display.start..m.source.start+b-m.display.start }
                        else { m.source.clone() };
                    let (Some(a), Some(b)) = (resolve(source.start), resolve(source.end)) else { continue; };
                    let mut i = starts.partition_point(|start| *start <= a).saturating_sub(1);
                    while i < lines.len() && starts[i] < b {
                        let start = a.saturating_sub(starts[i]);
                        let end = (b-starts[i]).min(lines[i].len());
                        if start < end { ranges[i].push(start..end); }
                        i += 1;
                    }
                }
            }
        }
    }
    // A combining mark next to a backtick can share the marker's grapheme.
    // Expand font roles to whole SOURCE graphemes; never emit invalid spans.
    for (line, ranges) in lines.iter().zip(&mut ranges) {
        ranges.sort_by_key(|r| r.start);
        let mut normalized: Vec<Range<usize>> = Vec::new();
        let mut cursor = 0;
        for (at, g) in line.grapheme_indices(true) {
            while ranges.get(cursor).is_some_and(|r| r.end <= at) { cursor += 1; }
            if ranges.get(cursor).is_some_and(|r| r.start < at+g.len()) {
                if let Some(last) = normalized.last_mut().filter(|r| r.end == at) { last.end = at+g.len(); }
                else { normalized.push(at..at+g.len()); }
            }
        }
        *ranges = normalized;
    }
    ranges
}
impl Preview {
    pub fn new(namespace: u32) -> Self {
        assert!(
            namespace > 0 && namespace < u32::MAX,
            "reserve a nonzero, non-transient namespace"
        );
        Self {
            namespace,
            document: None,
            texts: HashMap::new(),
            syntax: crate::syntax::Highlighter::default(),
            blocks: HashMap::new(),
            order: Vec::new(),
            revision: None,
            config: None,
            next_generation: 0,
            body_style: None,
            spacing: BlockSpacing::Compact,
            source_mode: false,
            prose: HashMap::new(),
            height: 0.,
            width: 0.,
            last_work: Work::default(),
        }
    }
    fn key(&self, id: Id) -> u64 {
        (u64::from(self.namespace) << 32) | u64::from(id)
    }
    pub fn release(&mut self, text: &mut TextService) {
        for c in self.texts.values() {
            if let Some(h) = c.paint {
                text.drop_paint(h);
            }
        }
        self.texts.clear();
        self.prose.clear();
        self.syntax.clear();
        self.blocks.clear();
        self.order.clear();
        self.revision = None;
        self.config = None;
        self.document = None;
    }
    fn ensure(
        &mut self,
        e: &Element,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        size: f32,
        width: f32,
        align: Align,
        flags: u8,
        syntax: &[PaintSpan],
        work: &mut Work,
    ) -> f32 {
        self.ensure_text(e.id, &e.rich, &e.origins, text, faces, theme, size, width, align, flags, syntax, work)
    }
    fn ensure_text(&mut self, id: Id, rich: &Arc<RichText>, origins: &[RawLine],
        text: &mut TextService, faces: Faces, theme: Theme, size: f32, width: f32,
        align: Align, flags: u8, syntax: &[PaintSpan], work: &mut Work,
    ) -> f32 {
        let effective = |f| {
            if theme.italic {
                f
            } else {
                f & !inline::EMPHASIS
            }
        };
        let base = faces.face(effective(flags));
        let style = Style {
            chain: base,
            wrap_em: Some((width / size).max(0.)),
            align,
            line_spacing: self.body_style.unwrap().line_spacing,
        };
        if let Some(c) = self.texts.get_mut(&id) {
            if Arc::ptr_eq(&c.rich, &rich)
                && c.faces == faces
                && c.theme == theme
                && c.style == style
                && c.size == size
                && c.flags == flags
                && c.syntax_spans == syntax
                && text.measure(c.handle).line_count() > 0
            {
                c.origins = origins.to_vec();
                return c.height;
            }
        }
        work.resolved_elements += 1;
        let mut fonts: Vec<FontSpan> = Vec::new();
        let mut run = 0;
        for (at, g) in rich.text.grapheme_indices(true) {
            while run < rich.runs.len() && rich.runs[run].range.end <= at {
                run += 1;
            }
            let f = rich.runs.get(run).map_or(flags, |r| flags | r.flags);
            let face = faces.face(effective(f));
            if face != base {
                if let Some(last) = fonts
                    .last_mut()
                    .filter(|s| s.chain == face && s.range.end == at)
                {
                    last.range.end = at + g.len();
                } else {
                    fonts.push(FontSpan {
                        range: at..at + g.len(),
                        chain: face,
                    });
                }
            }
        }
        let color = theme.role(flags);
        let mut paint_spans: Vec<PaintSpan> = Vec::new();
        for r in rich.runs.iter().filter(|_| !self.source_mode) {
            let c = theme.role(r.flags | flags);
            if c == color {
                continue;
            }
            if let Some(last) = paint_spans
                .last_mut()
                .filter(|s| s.color == c && s.range.end == r.range.start)
            {
                last.range.end = r.range.end;
            } else {
                paint_spans.push(PaintSpan {
                    range: r.range.clone(),
                    color: c,
                });
            }
        }
        paint_spans.extend_from_slice(syntax);
        let key = self.key(id);
        let fresh = !self.texts.contains_key(&id);
        let c = self.texts.entry(id).or_insert_with(|| TextCache {
            rich: Arc::new(RichText::default()),
            origins: Vec::new(),
            fonts: Vec::new(),
            paint_spans: Vec::new(),
            syntax_spans: Vec::new(),
            math: None,
            paint: None,
            handle: ShapedHandle::INVALID,
            style,
            paragraphs: Vec::new(),
            size,
            height: 0.,
            flags,
            color,
            faces,
            theme,
        });
        if fresh || c.rich.math != rich.math || c.style != style || c.faces != faces {
            c.math = rich.math.as_ref().map(|expr| {
                let mut layout = crate::math::Layout::new(expr, text, base, 1.);
                layout.fit(width / size);
                layout
            });
        }
        let input_changed = fresh || c.rich.text != rich.text || c.fonts != fonts;
        let mut shape = input_changed || c.style != style || text.measure(c.handle).line_count() == 0;
        {
            let old: HashMap<_, _> = c.paragraphs.iter().map(|p| ((p.origin.line, p.origin.column), p)).collect();
            let source = Source::new(rich, &fonts);
            let paragraphs: Vec<CachedParagraph> = source.parts.iter().enumerate().map(|(i, range)| {
                let raw = rich.source_byte(range.start);
                let line = &origins[origins.partition_point(|l| l.offset <= raw).saturating_sub(1)];
                let origin = Origin { line: line.origin.line, column: line.origin.column+raw-line.offset };
                let fonts = source.paragraph_fonts(i, ParagraphKey { namespace: 0, slot: 0, generation: 0 }).into_owned();
                let key = old.get(&(origin.line, origin.column)).filter(|p|
                    c.rich.text[p.range.clone()] == rich.text[range.clone()] && p.fonts == fonts).map(|p| p.key)
                    .unwrap_or_else(|| {
                        self.next_generation = self.next_generation.checked_add(1).expect("Markdown paragraph capacity");
                        ParagraphKey { namespace: key, slot: self.next_generation, generation: 0 }
                    });
                let top_em = old.get(&(origin.line, origin.column)).map_or(0., |p| p.top_em);
                CachedParagraph { key, origin, range: range.clone(), fonts, top_em }
            }).collect();
            shape |= !c.paragraphs.iter().map(|p| p.key).eq(paragraphs.iter().map(|p| p.key));
            c.paragraphs = paragraphs;
        }
        if c.paint_spans != paint_spans {
            let next = if paint_spans.is_empty() {
                None
            } else {
                work.paint_snapshots += 1;
                Some(
                    text.register_paint(&paint_spans)
                        .expect("Markdown paint pool"),
                )
            };
            if let Some(old) = c.paint {
                text.drop_paint(old);
            }
            c.paint = next;
            c.paint_spans = paint_spans;
        }
        c.syntax_spans = syntax.to_vec();
        c.rich = rich.clone();
        c.origins = origins.to_vec();
        c.fonts = fonts;
        c.style = style;
        c.size = size;
        c.flags = flags;
        c.color = color;
        c.faces = faces;
        c.theme = theme;
        if shape {
            c.shape(text, key, work);
        } else {
            c.height = c.math.as_ref().map_or_else(|| text.measure(c.handle).height_em(), |m| m.height()) * size;
        }
        c.height
    }
    fn measure_row(
        &mut self,
        row: &md::Row,
        header: bool,
        table: &TableCache,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        size: f32,
        work: &mut Work,
    ) -> f32 {
        work.measured_rows += 1;
        let width = table.width / table.align.len() as f32;
        let mut height = size * self.body_style.unwrap().line_spacing;
        for (i, e) in row.cells.iter().enumerate() {
            let align = match table.align[i] {
                md::Alignment::Left => Align::Left,
                md::Alignment::Center => Align::Center,
                md::Alignment::Right => Align::Right,
            };
            height = height.max(self.ensure(
                e,
                text,
                faces,
                theme,
                size,
                width - 20.,
                align,
                if header { inline::STRONG } else { 0 },
                &[],
                work,
            ));
        }
        height + 16.
    }
    /// No parsing occurs here. Stable column widths depend on viewport/schema,
    /// never newly streamed cell contents. Width/font changes may legitimately
    /// reflow cells; a changed row height only updates prefix-sum metadata.
    pub fn sync(
        &mut self,
        doc: &Document,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        width: f32,
        size: f32,
    ) -> Work {
        let style = Style { chain: faces.prose[0], wrap_em: Some(width/size), align: Align::Left, line_spacing: 1.25 };
        self.sync_styled(doc, text, faces, theme, style, size, BlockSpacing::Compact)
    }
    /// Use the caller's text layout policy; no reader-only leading or gap in
    /// PreserveSource mode. Markdown structure and inline spans remain derived.
    pub fn sync_styled(&mut self, doc: &Document, text: &mut TextService, faces: Faces,
        theme: Theme, style: Style, size: f32, spacing: BlockSpacing,
    ) -> Work {
        assert_eq!(style.chain, faces.prose[0], "body style must use the supplied prose face");
        let width = style.wrap_em.expect("Markdown view needs a wrap width") * size;
        if self.body_style != Some(style) || self.spacing != spacing || self.source_mode {
            self.revision = None;
        }
        self.body_style = Some(style); self.spacing = spacing; self.source_mode = false;
        assert!(
            width.is_finite() && width > 0. && size.is_finite() && size > 0.,
            "positive finite Markdown viewport/font size"
        );
        if self.document != Some(doc.identity()) {
            self.release(text);
            self.document = Some(doc.identity());
        }
        let config = (faces, theme, width, size);
        if self.revision == Some(doc.revision()) && self.config == Some(config) {
            return Work::default();
        }
        let mut work = Work::default();
        let config_changed = self.config != Some(config);
        let changes = self
            .revision
            .and_then(|r| doc.changes_since(r))
            .map(|c| c.collect::<Vec<_>>());
        let all = config_changed || changes.is_none();
        let mut changed = HashSet::new();
        let mut reconcile = HashSet::new();
        let mut dirty_rows: HashMap<Id, HashSet<Id>> = HashMap::new();
        if let Some(changes) = &changes {
            for c in changes {
                changed.extend(c.changed_blocks.iter().copied());
                reconcile.extend(c.reconcile.iter().copied());
                for &(b, r) in &c.dirty_rows {
                    dirty_rows.entry(b).or_default().insert(r);
                }
                for id in &c.removed_elements {
                    if let Some(old) = self.texts.remove(id) {
                        if let Some(p) = old.paint {
                            text.drop_paint(p);
                        }
                    }
                }
            }
        }
        let live = doc.blocks().iter().map(|b| b.id).collect::<HashSet<_>>();
        self.blocks.retain(|id, _| live.contains(id));
        self.syntax.retain(doc);
        // A viewer which missed the bounded change history reconciles liveness
        // from the current document, rather than displaying lost updates.
        if changes.is_none() {
            let live = doc
                .blocks()
                .iter()
                .flat_map(|b| b.elements().map(|e| e.id))
                .collect::<HashSet<_>>();
            self.texts.retain(|id, c| {
                if live.contains(id) {
                    true
                } else {
                    if let Some(p) = c.paint {
                        text.drop_paint(p);
                    }
                    false
                }
            });
        }
        for block in doc.blocks() {
            if spacing == BlockSpacing::PreserveSource && ordinary(block) { continue; }
            if !all && !changed.contains(&block.id) && self.blocks.contains_key(&block.id) {
                continue;
            }
            let old = self.blocks.remove(&block.id);
            let (content, height) = match &block.content {
                Content::Table(model) => {
                    let table_width = width.max(model.align.len() as f32 * size * 6.);
                    let mut t = match old.map(|b| b.content) {
                        Some(LayoutContent::Table(t)) => t,
                        _ => TableCache {
                            align: Vec::new(),
                            width: 0.,
                            rows: Vec::new(),
                            heights: Heights::default(),
                        },
                    };
                    let full = all
                        || reconcile.contains(&block.id)
                        || t.align != model.align
                        || t.width != table_width;
                    t.align = model.align.clone();
                    t.width = table_width;
                    if full {
                        t.rows = model.rows.iter().map(|r| r.id).collect();
                        t.heights = Heights::default();
                        for (i, row) in model.rows.iter().enumerate() {
                            let h = self.measure_row(
                                row,
                                i == 0,
                                &t,
                                text,
                                faces,
                                theme,
                                size,
                                &mut work,
                            );
                            t.heights.push(h);
                        }
                        work.indexed_rows += t.rows.len();
                    } else {
                        for c in changes.as_ref().unwrap() {
                            for splice in c.table_splices.iter().filter(|s| s.block == block.id) {
                                // Same row IDs: preserve height until the changed
                                // row is measured. Structural changes replay in order.
                                if t.rows[splice.range.clone()] != splice.inserted {
                                    work.indexed_rows += t
                                        .heights
                                        .splice(splice.range.clone(), splice.inserted.len());
                                    t.rows.splice(
                                        splice.range.clone(),
                                        splice.inserted.iter().copied(),
                                    );
                                }
                            }
                        }
                        if let Some(rows) = dirty_rows.get(&block.id) {
                            for &id in rows {
                                if let Some(i) = model.row_index(id) {
                                    let h = self.measure_row(
                                        &model.rows[i],
                                        i == 0,
                                        &t,
                                        text,
                                        faces,
                                        theme,
                                        size,
                                        &mut work,
                                    );
                                    t.heights.set(i, h);
                                }
                            }
                        }
                    }
                    debug_assert_eq!(t.rows.len(), model.rows.len());
                    let h = t.heights.total();
                    (LayoutContent::Table(t), h)
                }
                Content::Text { kind, element } => {
                    let (scale, flags, x, quote, marker) = match kind {
                        TextKind::Heading(n) => (
                            match n {
                                1 => 1.75,
                                2 => 1.4,
                                3 => 1.18,
                                _ => 1.05,
                            },
                            inline::STRONG,
                            0.,
                            false,
                            None,
                        ),
                        TextKind::Quote(n) => {
                            (1., 0, (*n as f32 * 16.).min(width / 3.), true, None)
                        }
                        TextKind::List { depth, marker } => (
                            1.,
                            0,
                            (22. + *depth as f32 * 16.).min(width / 2.),
                            false,
                            Some(marker.clone()),
                        ),
                        _ => (1., 0, 0., false, None),
                    };
                    let h = self.ensure(
                        element,
                        text,
                        faces,
                        theme,
                        size * scale,
                        (width - x).max(size),
                        Align::Left,
                        flags,
                        &[],
                        &mut work,
                    );
                    (
                        LayoutContent::Parts {
                            items: vec![Part {
                                id: element.id,
                                x,
                                y: 0.,
                            }],
                            quote,
                            code: false,
                            marker,
                        },
                        h,
                    )
                }
                Content::Code { lines, .. } => {
                    let mut items = Vec::new();
                    let mut y = 12.;
                    let syntax = self.syntax.block(block).to_vec();
                    for (e, paints) in lines.iter().zip(&syntax) {
                        let h = self.ensure(
                            e,
                            text,
                            faces,
                            theme,
                            size * 0.9,
                            width - 24.,
                            Align::Left,
                            inline::CODE,
                            paints,
                            &mut work,
                        );
                        items.push(Part {
                            id: e.id,
                            x: 12.,
                            y,
                        });
                        y += h;
                    }
                    (
                        LayoutContent::Parts {
                            items,
                            quote: false,
                            code: true,
                            marker: None,
                        },
                        y.max(size * self.body_style.unwrap().line_spacing) + 12.,
                    )
                }
                Content::Rule => (LayoutContent::Rule, 12.),
            };
            self.blocks.insert(
                block.id,
                BlockCache {
                    y: 0.,
                    height,
                    content,
                },
            );
        }
        self.order.clear();
        let mut live_prose = HashSet::new();
        if spacing == BlockSpacing::PreserveSource {
            let mut start = 0;
            for block in doc.blocks().iter().filter(|b| !ordinary(b)) {
                self.prose_group(doc, start..block.lines.start, text, faces, theme, width, size, &mut work, &mut live_prose);
                self.order.push(block.id); start = block.lines.end;
            }
            self.prose_group(doc, start..doc.line_count(), text, faces, theme, width, size, &mut work, &mut live_prose);
        } else { self.order.extend(doc.blocks().iter().map(|b| b.id)); }
        self.prose.retain(|id, _| live_prose.contains(id));
        let live_blocks: HashSet<_> = self.order.iter().copied().collect();
        self.blocks.retain(|id, _| live_blocks.contains(id));
        let mut live_texts = HashSet::new();
        for b in self.blocks.values() {
            if let LayoutContent::Parts { items, .. } = &b.content { live_texts.extend(items.iter().map(|p| p.id)); }
        }
        for b in doc.blocks() {
            if let Content::Table(t) = &b.content { live_texts.extend(t.rows.iter().flat_map(|r| r.cells.iter().map(|e| e.id))); }
        }
        self.texts.retain(|id, c| {
            let keep = live_texts.contains(id);
            if !keep && let Some(paint) = c.paint { text.drop_paint(paint); }
            keep
        });
        let mut y = 0.;
        self.width = width;
        for (index, id) in self.order.iter().enumerate() {
            // 8dp at the normal 16dp body size, like Tau 1. No phantom paragraph
            // gap at the bottom of every message/details fragment.
            if index > 0 && spacing == BlockSpacing::Compact {
                y += size * 0.5;
            }
            let b = self.blocks.get_mut(id).unwrap();
            b.y = y;
            y += b.height;
            if let LayoutContent::Table(t) = &b.content {
                self.width = self.width.max(t.width);
            }
        }
        self.height = y;
        self.revision = Some(doc.revision());
        self.config = Some(config);
        self.last_work = work;
        work
    }
    fn prose_group(&mut self, doc: &Document, range: Range<usize>, text: &mut TextService,
        faces: Faces, theme: Theme, width: f32, size: f32, work: &mut Work, live: &mut HashSet<Id>,
    ) {
        if range.is_empty() { return; }
        let id = doc.line_key(range.start).unwrap().0;
        live.insert(id);
        let unchanged = self.prose.get(&id).is_some_and(|p|
            p.lines.iter().copied().eq(range.clone().map(|i| doc.line_key(i).unwrap())));
        if !unchanged { self.prose.insert(id, prose_input(doc, range)); }
        let input = self.prose.remove(&id).unwrap();
        let height = self.ensure_text(id, &input.rich, &input.origins, text, faces, theme, size, width,
            self.body_style.unwrap().align, 0, &[], work);
        self.prose.insert(id, input);
        self.blocks.insert(id, BlockCache { y: 0., height, content: LayoutContent::Parts {
            items: vec![Part { id, x: 0., y: 0. }], quote: false, code: false, marker: None,
        } });
        self.order.push(id);
    }
    /// Shape exact editable source with semantic code fonts. The caller keeps
    /// using Sanscale's ShapedHandle/Layout for editing; there is no editor engine here.
    pub fn sync_source(&mut self, doc: &Document, text: &mut TextService, faces: Faces,
        style: Style, size: f32,
    ) -> (ShapedHandle, Option<PaintHandle>) {
        assert_eq!(style.chain, faces.prose[0], "body style must use the supplied prose face");
        let id = doc.line_key(0).expect("source has an empty first line").0;
        if self.document == Some(doc.identity()) && self.source_mode && self.revision == Some(doc.revision())
            && self.body_style == Some(style) && let Some(c) = self.texts.get(&id)
            && c.size == size && c.faces == faces && text.measure(c.handle).line_count() > 0 {
            self.last_work = Work::default(); return (c.handle, c.paint);
        }
        if self.document != Some(doc.identity()) || !self.source_mode { self.release(text); }
        self.document = Some(doc.identity()); self.source_mode = true;
        self.body_style = Some(style);
        if self.revision != Some(doc.revision()) || !self.prose.contains_key(&id) {
            self.prose.insert(id, source_input(doc));
        }
        let input = self.prose.remove(&id).unwrap();
        let syntax = self.syntax.source_spans(doc);
        let mut work = Work::default();
        self.height = self.ensure_text(id, &input.rich, &input.origins, text, faces, Theme::default(), size,
            style.wrap_em.unwrap_or(f32::MAX/size)*size, style.align, 0, &syntax, &mut work);
        self.prose.insert(id, input);
        self.revision = Some(doc.revision()); self.last_work = work;
        let c = &self.texts[&id];
        self.width = text.measure(c.handle).width_em().max(style.wrap_em.unwrap_or(0.))*size;
        self.config = Some((faces, Theme::default(), self.width, size));
        self.order = vec![id];
        self.blocks.insert(id, BlockCache { y: 0., height: self.height, content: LayoutContent::Parts {
            items: vec![Part { id, x: 0., y: 0. }], quote: false, code: false, marker: None,
        } });
        (c.handle, c.paint)
    }
    pub fn block_y(&self, id: Id) -> Option<f32> {
        if let Some(b) = self.blocks.get(&id) { return Some(b.y); }
        for b in self.blocks.values() {
            if let LayoutContent::Parts { items, .. } = &b.content {
                for p in items {
                    let c = &self.texts[&p.id];
                    if let Some(line) = c.paragraphs.iter().find(|line| line.origin.line == id) {
                        return Some(b.y+p.y+line.top_em*c.size);
                    }
                }
            }
        }
        None
    }
    /// Only visible table rows are traversed. Draw clips are shared by the
    /// viewport, so many cells remain one batch/segment rather than N draw calls.
    pub fn scene(
        &mut self,
        text: &mut TextService,
        doc: &Document,
        viewport: Rect,
        scroll: Vec2,
    ) -> Scene {
        let mut scene = Scene::default();
        let (_, theme, width, size) = self.config.unwrap();
        let mut placed = Vec::new();
        let mut marker_draws = Vec::new();
        let origin = Vec2::new(viewport.x - scroll.x, viewport.y - scroll.y);
        let deco = |x, y, w, h, color| Decoration {
            rect: Rect::new(origin.x + x, origin.y + y, w, h),
            color,
        };
        for &id in &self.order {
            let b = &self.blocks[&id];
            if b.y + b.height < scroll.y || b.y > scroll.y + viewport.height {
                continue;
            }
            match &b.content {
                LayoutContent::Parts {
                    items,
                    quote,
                    code,
                    marker,
                } => {
                    if *quote {
                        scene
                            .under
                            .push(deco(0., b.y, 3., b.height, theme.accent()));
                    }
                    if *code {
                        scene
                            .under
                            .push(deco(0., b.y, width, b.height, theme.panel()));
                    }
                    if let Some(marker) = marker {
                        let at = Vec2::new(origin.x + items[0].x - 22., origin.y + b.y);
                        marker_draws.push((marker.clone(), at));
                    }
                    for p in items {
                        let c = &self.texts[&p.id];
                        if b.y + p.y + c.height >= scroll.y
                            && b.y + p.y <= scroll.y + viewport.height
                        {
                            placed.push((p.id, Vec2::new(origin.x + p.x, origin.y + b.y + p.y)));
                        }
                    }
                }
                LayoutContent::Table(t) => {
                    let block = doc.blocks().iter().find(|b| b.id == id).unwrap();
                    let Content::Table(model) = &block.content else {
                        unreachable!()
                    };
                    let mut row = t.heights.row_at((scroll.y - b.y).max(0.));
                    let cw = t.width / t.align.len() as f32;
                    while row < t.rows.len() {
                        let y = b.y + t.heights.prefix(row);
                        if y > scroll.y + viewport.height {
                            break;
                        }
                        let h = t.heights.values[row];
                        scene.under.push(deco(
                            0.,
                            y,
                            t.width,
                            h,
                            if row == 0 {
                                Color([0.045, 0.080, 0.115, 1.])
                            } else if row % 2 == 0 {
                                theme.panel()
                            } else {
                                Color([0.017, 0.023, 0.032, 1.])
                            },
                        ));
                        scene.under.push(deco(0., y, t.width, 1., theme.grid()));
                        for col in 0..t.align.len() {
                            scene
                                .under
                                .push(deco(col as f32 * cw, y, 1., h, theme.grid()));
                            placed.push((
                                model.rows[row].cells[col].id,
                                Vec2::new(origin.x + col as f32 * cw + 10., origin.y + y + 8.),
                            ));
                        }
                        scene.under.push(deco(t.width - 1., y, 1., h, theme.grid()));
                        row += 1;
                    }
                    scene
                        .under
                        .push(deco(0., b.y + b.height - 1., t.width, 1., theme.grid()));
                }
                LayoutContent::Rule => {
                    scene
                        .under
                        .push(deco(0., b.y + 5., width, 1., theme.grid()))
                }
            }
        }
        for (id, at) in placed {
            let key = self.key(id);
            let c = self.texts.get_mut(&id).unwrap();
            if text.measure(c.handle).line_count() == 0 {
                c.shape(text, key, &mut Work::default());
            }
            if let Some(math) = &mut c.math {
                math.draw(text, c.style.chain, at, c.size, c.color, viewport, &mut scene.draws, &mut scene.over);
                scene.placed.push((id, at));
                continue;
            }
            scene.draws.push(Draw {
                block: c.handle,
                at,
                size: c.size,
                color: c.color,
                paint: c.paint,
                clip: Some(viewport),
            });
            if self.source_mode { scene.placed.push((id, at)); continue; }
            let layout = text.measure(c.handle);
            for r in &c.rich.runs {
                let flags = r.flags | c.flags;
                if flags & (inline::CODE | inline::STRIKE | inline::LINK) == 0 {
                    continue;
                }
                for span in layout.selection(r.range.clone()) {
                    let x = at.x + span.x_em * c.size;
                    let y = at.y + span.y_em * c.size;
                    let w = span.width_em * c.size;
                    if flags & inline::CODE != 0 && c.flags & inline::CODE == 0 {
                        scene.under.push(Decoration {
                            rect: Rect::new(x - 2., y, w + 4., span.height_em * c.size),
                            color: theme.panel(),
                        });
                    }
                    if flags & inline::STRIKE != 0 {
                        scene.over.push(Decoration {
                            rect: Rect::new(x, y + c.size * 0.6, w, 1.),
                            color: c.color,
                        });
                    }
                    if flags & inline::LINK != 0 {
                        // Font metrics and leading move the baseline; a fixed
                        // offset from the line top can cut through the glyphs.
                        let baseline = layout.line(span.line_index).unwrap().baseline_em;
                        scene.over.push(Decoration {
                            rect: Rect::new(x, at.y + (baseline + 0.1) * c.size, w, 1.),
                            color: theme.accent(),
                        });
                    }
                }
            }
            scene.placed.push((id, at));
        }
        // Shape every marker before any prepare/draw can bind atlas textures.
        for (marker, at) in marker_draws {
            let style = Style {
                chain: self.config.unwrap().0.prose[0],
                wrap_em: None,
                align: Align::Left,
                line_spacing: self.body_style.unwrap().line_spacing,
            };
            let block = text.shape_transient(&marker, &style).unwrap();
            scene.draws.push(Draw {
                block,
                at,
                size,
                color: theme.accent(),
                clip: Some(viewport),
                ..Default::default()
            });
        }
        scene
    }
    /// Application-owned activation policy. This only returns the authored URL;
    /// the consumer must validate its scheme and ask before launching anything.
    pub fn hit_link(&self, scene: &Scene, point: Vec2, text: &TextService) -> Option<String> {
        for &(id, at) in &scene.placed {
            let c = &self.texts[&id];
            if c.math.is_some() { continue; }
            if point.y < at.y || point.y > at.y + c.height || point.x < at.x {
                continue;
            }
            let hit = text.measure(c.handle).hit_test(Vec2::new(
                (point.x - at.x) / c.size,
                (point.y - at.y) / c.size,
            ))?;
            // Don't activate the last link by clicking blank space past its line.
            let layout = text.measure(c.handle);
            if layout
                .line(hit.line_index)
                .is_some_and(|line| point.x > at.x + line.width_em * c.size)
            {
                continue;
            }
            let raw = c.rich.source_byte(hit.byte_index);
            if let Some(link) = c.rich.links.iter().find(|link| link.source.contains(&raw)) {
                return Some(link.destination.clone());
            }
        }
        None
    }

    fn resolve_raw(c: &TextCache, doc: &Document, raw: usize) -> Option<usize> {
        let i = c.origins.partition_point(|l| l.offset <= raw).saturating_sub(1);
        let line = &c.origins[i];
        doc.resolve(Origin { line: line.origin.line, column: line.origin.column + raw - line.offset })
    }

    fn projected_selection(
        c: &TextCache,
        doc: &Document,
        range: &Range<usize>,
    ) -> Option<Range<usize>> {
        if c.math.is_some() {
            let first = c.rich.mapping.first()?;
            let last = c.rich.mapping.last()?;
            let start = Self::resolve_raw(c, doc, first.source.start)?;
            let end = Self::resolve_raw(c, doc, last.source.end)?;
            return (range.start < end && range.end > start).then_some(0..c.rich.text.len());
        }
        let mut selected = None::<Range<usize>>;
        for (byte, grapheme) in c.rich.text.grapheme_indices(true) {
            let raw = c.rich.source_byte(byte);
            let i = c
                .origins
                .partition_point(|l| l.offset <= raw)
                .saturating_sub(1);
            let line = &c.origins[i];
            let source = doc.resolve(Origin {
                line: line.origin.line,
                column: line.origin.column + raw - line.offset,
            })?;
            if range.contains(&source) {
                selected.get_or_insert(byte..byte).end = byte + grapheme.len();
            }
        }
        selected
    }

    /// Selection uses source coordinates so streaming edits and reflow never
    /// leave byte indexes pointing inside a newly shaped grapheme.
    pub fn selection(
        &self,
        scene: &Scene,
        text: &TextService,
        doc: &Document,
        range: Range<usize>,
    ) -> Vec<Decoration> {
        let mut out = Vec::new();
        for &(id, at) in &scene.placed {
            let c = &self.texts[&id];
            if let Some(range) = Self::projected_selection(c, doc, &range) {
                if let Some(math) = &c.math {
                    out.push(Decoration { rect: Rect::new(at.x, at.y, math.width*c.size, c.height),
                        color: Color([0.025, 0.10, 0.19, 1.]) });
                    continue;
                }
                for span in text.measure(c.handle).selection(range) {
                    out.push(Decoration {
                        rect: Rect::new(
                            at.x + span.x_em * c.size,
                            at.y + span.y_em * c.size,
                            span.width_em * c.size,
                            span.height_em * c.size,
                        ),
                        color: Color([0.025, 0.10, 0.19, 1.]),
                    });
                }
            }
        }
        out
    }
    pub fn copy_selection(&self, doc: &Document, range: Range<usize>) -> String {
        let mut out = Vec::new();
        for id in &self.order {
            match &self.blocks[id].content {
                LayoutContent::Parts { items, .. } => {
                    for p in items {
                        let c = &self.texts[&p.id];
                        if let Some(r) = Self::projected_selection(c, doc, &range) { out.push(c.rich.text[r].to_owned()); }
                    }
                }
                LayoutContent::Table(_) => {
                    let block = doc.blocks().iter().find(|b| b.id == *id).unwrap();
                    for e in block.elements() {
                        let c = &self.texts[&e.id];
                        if let Some(r) = Self::projected_selection(c, doc, &range) { out.push(c.rich.text[r].to_owned()); }
                    }
                }
                LayoutContent::Rule => {}
            }
        }
        out.join("\n")
    }

    /// Closest projected caret, including margins and gaps between text blocks.
    /// Distances are returned separately so clients can prefer the nearest line
    /// before horizontal proximity (also works for adjacent table cells).
    pub fn nearest_source(
        &self,
        scene: &Scene,
        point: Vec2,
        text: &TextService,
        doc: &Document,
    ) -> Option<(usize, f32, f32)> {
        let mut closest: Option<(usize, f32, f32)> = None;
        for &(id, at) in &scene.placed {
            let c = &self.texts[&id];
            let layout = text.measure(c.handle);
            let dy = (at.y - point.y).max(point.y - at.y - c.height).max(0.);
            let dx = (at.x - point.x)
                .max(point.x - at.x - c.math.as_ref().map_or_else(|| layout.width_em(), |m| m.width) * c.size)
                .max(0.);
            if closest.is_some_and(|(_, y, x)| dy > y || dy == y && dx >= x) {
                continue;
            }
            if let Some(math) = &c.math {
                let edge = if point.x < at.x + math.width*c.size*0.5 { 0 } else { c.rich.text.len() };
                if let Some(byte) = Self::resolve_raw(c, doc, c.rich.source_byte(edge)) {
                    closest = Some((byte, dy, dx));
                }
                continue;
            }
            let Some(hit) = layout.hit_test(Vec2::new(
                (point.x - at.x) / c.size,
                (point.y - at.y) / c.size,
            )) else {
                continue;
            };
            let raw = c.rich.source_byte(hit.byte_index);
            let i = c
                .origins
                .partition_point(|l| l.offset <= raw)
                .saturating_sub(1);
            let line = &c.origins[i];
            if let Some(byte) = doc.resolve(Origin {
                line: line.origin.line,
                column: line.origin.column + raw - line.offset,
            }) {
                closest = Some((byte, dy, dx));
            }
        }
        closest
    }

    /// Projected positions map through explicit source spans (entities, escaped
    /// delimiters and removed markup are not a constant-offset subtraction).
    pub fn hit_source(
        &self,
        scene: &Scene,
        point: Vec2,
        text: &TextService,
        doc: &Document,
    ) -> Option<usize> {
        for &(id, at) in &scene.placed {
            let c = &self.texts[&id];
            if point.y < at.y
                || point.y > at.y + c.height
                || point.x < at.x
                || point.x > at.x + c.style.wrap_em.unwrap_or(100.) * c.size
            {
                continue;
            }
            if let Some(math) = &c.math {
                if point.x > at.x + math.width*c.size { continue; }
                let edge = if point.x < at.x + math.width*c.size*0.5 { 0 } else { c.rich.text.len() };
                return Self::resolve_raw(c, doc, c.rich.source_byte(edge));
            }
            let hit = text.measure(c.handle).hit_test(Vec2::new(
                (point.x - at.x) / c.size,
                (point.y - at.y) / c.size,
            ))?;
            let raw = c.rich.source_byte(hit.byte_index);
            let i = c
                .origins
                .partition_point(|l| l.offset <= raw)
                .saturating_sub(1);
            let line = &c.origins[i];
            return doc.resolve(Origin {
                line: line.origin.line,
                column: line.origin.column + raw - line.offset,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_paragraphs_use_the_same_sanscale_geometry_as_source() {
        let (mut text, faces) = setup();
        let mut source = Preview::new(30000); let mut read = Preview::new(30001);
        for raw in ["", "\n", "\n\n", "one\ntwo", "one\n\ntwo", "one\n\n\ntwo", "\none\n", " \n\t\n", "words WWW iii café 🙂 repeat and wrap\n\nsecond paragraph with words"] {
            for width in [8., 90., 500.] {
                for leading in [1., 1.25, 1.6] {
                    let doc = Document::new(raw);
                    let style = Style { chain: faces.prose[0], wrap_em: Some(width/17.), align: Align::Left, line_spacing: leading };
                    let (edited, _) = source.sync_source(&doc, &mut text, faces, style, 17.);
                    read.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
                    let scene = read.scene(&mut text, &doc, Rect::new(0.,0.,width,5000.), Vec2::new(0.,0.));
                    assert_eq!(scene.draws.len(), 1, "prose is one Sanscale block, not one per syntax paragraph");
                    let a = text.measure(edited); let b = text.measure(scene.draws[0].block);
                    assert_eq!(a.len_bytes(), b.len_bytes(), "{raw:?}");
                    assert_eq!(a.line_count(), b.line_count());
                    assert_eq!(a.height_em(), b.height_em());
                    for byte in raw.grapheme_indices(true).map(|(i,_)| i).chain([raw.len()]) {
                        let x = a.caret_rect(a.caret_at(byte)); let y = b.caret_rect(b.caret_at(byte));
                        assert_eq!((x.x_em,x.y_em,x.height_em), (y.x_em,y.y_em,y.height_em), "{raw:?}, byte={byte}");
                    }
                    assert_eq!(read.copy_selection(&doc, 0..raw.len()), raw);
                }
            }
        }
    }

    #[test]
    fn grouped_inline_projection_retains_links_source_hits_and_paragraph_boundaries() {
        let (mut text, faces) = setup();
        let raw = "\nfirst **bold** &amp; café\n\nsecond [link](https://example.org) `code`\n\n\n*not across\n\nparagraphs*\n";
        let doc = Document::new(raw);
        let mut view = Preview::new(30002);
        let style = Style { chain: faces.prose[0], wrap_em: Some(40.), align: Align::Left, line_spacing: 1.4 };
        view.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
        let scene = view.scene(&mut text, &doc, Rect::new(0.,0.,680.,1000.), Vec2::new(0.,0.));
        assert_eq!(scene.draws.len(), 1);
        for block in doc.blocks() { assert!(view.block_y(block.id).is_some(), "grouping must retain syntax-block anchors"); }
        assert_eq!(view.copy_selection(&doc, 0..raw.len()), "\nfirst bold & café\n\nsecond link code\n\n\n*not across\n\nparagraphs*\n");
        let c = &view.texts[&view.order[0]];
        for (display, source) in [("bold", "bold"), ("&", "&amp;"), ("link", "link"), ("code", "code"), ("paragraphs", "paragraphs")] {
            let pos = c.rich.text.find(display).unwrap();
            let caret = text.measure(c.handle).caret_rect(text.measure(c.handle).caret_at(pos));
            let point = Vec2::new(caret.x_em*17.+0.01, (caret.y_em+caret.height_em*0.5)*17.);
            assert_eq!(view.hit_source(&scene, point, &text, &doc), raw.find(source));
            if display == "link" { assert_eq!(view.hit_link(&scene, point, &text).as_deref(), Some("https://example.org")); }
        }
        assert!(!view.selection(&scene, &text, &doc, raw.find("bold").unwrap()..raw.find("code").unwrap()+4).is_empty());
    }

    #[test]
    fn paragraph_group_edits_keep_unchanged_paragraph_identities() {
        let (mut text, faces) = setup();
        let raw = (0..1000).map(|i| format!("paragraph {i}\n\n")).collect::<String>();
        let mut doc = Document::new(&raw);
        let style = Style { chain: faces.prose[0], wrap_em: Some(30.), align: Align::Left, line_spacing: 1.25 };
        let mut read = Preview::new(30003); let mut source = Preview::new(30004);
        read.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
        source.sync_source(&doc, &mut text, faces, style, 17.);
        let keys = |v: &Preview| v.texts[&doc.line_key(0).unwrap().0].paragraphs.iter().map(|p| p.key).collect::<Vec<_>>();
        let before_read = keys(&read); let before_source = keys(&source);
        let at = raw.find("paragraph 500").unwrap();
        doc.edit(at..at+9, "altered").unwrap();
        read.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
        source.sync_source(&doc, &mut text, faces, style, 17.);
        for (v, before) in [(&read, before_read), (&source, before_source)] {
            let after = &v.texts[&doc.line_key(0).unwrap().0].paragraphs;
            assert_eq!(after.len(), before.len());
            assert_eq!(after.iter().zip(before).filter(|(a,b)| a.key != *b).count(), 1);
        }
        let work = read.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
        assert_eq!(work.layout_requests, 0);
        source.sync_source(&doc, &mut text, faces, style, 17.);
        assert_eq!(source.last_work.layout_requests, 0);
    }

    #[test]
    fn source_font_context_and_graphemes_are_preserved_by_shared_preparation() {
        let (mut text, faces) = setup();
        // Distinct supplied faces make font-role changes observable; system
        // mono-family discovery is tested by the consuming application's suite.
        let faces = Faces { mono: [faces.prose[1]; 4], ..faces };
        let style = Style { chain: faces.prose[0], wrap_em: Some(80.), align: Align::Left, line_spacing: 1.25 };
        for (case, raw) in ["before `é` after", "before `́x` after", "| a | `x\\|y` |\n| --- | --- |\n| b | c |", "before `multi\nline code` after", "```unknown\nWWWW iiii\n```\nordinary"].into_iter().enumerate() {
            let doc = Document::new(raw); let mut view = Preview::new(30300+case as u32);
            let (h, _) = view.sync_source(&doc, &mut text, faces, style, 17.);
            assert_eq!(text.measure(h).len_bytes(), raw.len());
            let scene = view.scene(&mut text, &doc, Rect::new(0.,0.,1360.,600.), Vec2::new(0.,0.));
            assert_eq!(scene.draws.len(), 1); assert_eq!(scene.draws[0].block, h);
            assert_eq!(view.copy_selection(&doc, 0..raw.len()), raw);
            let c = &view.texts[&doc.line_key(0).unwrap().0];
            assert!(c.fonts.iter().any(|f| f.chain == faces.mono[0]));
            for p in &c.paragraphs {
                let slice = &c.rich.text[p.range.clone()];
                let bounds: Vec<_> = slice.grapheme_indices(true).map(|(i,_)| i).chain([slice.len()]).collect();
                for f in &p.fonts { assert!(bounds.contains(&f.range.start) && bounds.contains(&f.range.end)); }
            }
            view.release(&mut text);
        }
        let mut doc = Document::new("```rust\nWWWW iiii\n```\nordinary WWW iii");
        let mut view = Preview::new(30101);
        view.sync_source(&doc, &mut text, faces, style, 17.);
        let id = doc.line_key(0).unwrap().0;
        let old: Vec<_> = view.texts[&id].paragraphs.iter().map(|p| p.key).collect();
        doc.edit(3..7, "python").unwrap();
        view.sync_source(&doc, &mut text, faces, style, 17.);
        assert_eq!(view.texts[&id].paragraphs[1].key, old[1], "language changes only repaint unchanged code");
        doc.edit(0..9, "intro").unwrap();
        let (warm, _) = view.sync_source(&doc, &mut text, faces, style, 17.);
        let c = &view.texts[&id];
        assert_ne!(c.paragraphs[1].key, old[1]); assert_ne!(c.paragraphs[3].key, old[3]);
        assert!(c.paragraphs[1].fonts.is_empty()); assert!(!c.paragraphs[3].fonts.is_empty());
        let mut cold = Preview::new(30102);
        let (fresh, _) = cold.sync_source(&doc, &mut text, faces, style, 17.);
        assert_eq!(text.measure(warm).height_em(), text.measure(fresh).height_em());
        for i in 0..text.measure(warm).line_count() {
            assert_eq!(text.measure(warm).line(i).unwrap().width_em, text.measure(fresh).line(i).unwrap().width_em);
        }
    }

    #[test]
    fn preserved_mixed_blocks_and_middle_edits_match_cold_views() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("\nprose **bold**\n\n# Heading\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```rs\nlet x = 1;\n```\n\nafter\n");
        let mut view = Preview::new(30200);
        for (i, insert) in ["", "\n", "inserted\n\n", "**bold** &amp; "].iter().enumerate() {
            doc.edit(0..0, insert).unwrap();
            for width in [8., 30.] {
                let style = Style { chain: faces.prose[0], wrap_em: Some(width), align: Align::Left, line_spacing: 1.37 };
                view.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
                let mut cold = Preview::new(30201+i as u32*2+u32::from(width > 10.));
                cold.sync_styled(&doc, &mut text, faces, Theme::default(), style, 17., BlockSpacing::PreserveSource);
                assert_eq!(view.height, cold.height);
                let clip = Rect::new(0.,0.,width*17.,2000.);
                let a = view.scene(&mut text, &doc, clip, Vec2::new(0.,0.));
                let b = cold.scene(&mut text, &doc, clip, Vec2::new(0.,0.));
                assert_eq!(a.draws.len(), b.draws.len());
                for (a,b) in a.draws.iter().zip(&b.draws) {
                    assert_eq!((a.at, a.size), (b.at, b.size));
                    assert_eq!(text.measure(a.block).height_em(), text.measure(b.block).height_em());
                }
                assert_eq!(view.copy_selection(&doc, 0..doc.source().len_bytes()), cold.copy_selection(&doc, 0..doc.source().len_bytes()));
                cold.release(&mut text);
            }
        }
    }

    #[test]
    fn spaces_keep_positive_advance_across_stream_and_font_boundaries() {
        let (mut text, faces) = setup();
        let samples = [
            "before **bold words joined here** after more normal words",
            "before _italic words joined here_ after more normal words",
            "before `code words joined here` after more normal words",
            "before [link words joined here](https://example.org) after more normal words",
            "before **bold _italic_ bold** after one two three four",
            "first line ending in four words\nsecond line with four words",
            "A repeated paragraph with ordinary spaces between words. ",
            "normal café words 🙂 emoji words αβ math letters words",
            "one two three four\n\nfive six seven eight\n\n**bold** and more words",
            "| plain words here | **bold words here** |\n| --- | --- |\n| some more words | `more code words` |",
        ];
        for (sample_index, sample) in samples.iter().enumerate() {
            for width in [130., 5000.] {
                let mut doc = Document::default();
                let mut view = Preview::new(20_000 + sample_index as u32*2 + u32::from(width > 200.));
                // Split at every character, including whitespace and delimiter
                // boundaries; assert geometry rather than merely copied text.
                for chunk in sample.chars() {
                    doc.append(&chunk.to_string()).unwrap();
                    view.sync(&doc, &mut text, faces, Theme::default(), width, 17.);
                    for c in view.texts.values() {
                        let layout = text.measure(c.handle);
                        for (byte, ch) in c.rich.text.char_indices().filter(|(_, c)| *c == ' ') {
                            let a = layout.caret_at(byte); let b = layout.caret_at(byte+ch.len_utf8());
                            let x = layout.caret_rect(a); let y = layout.caret_rect(b);
                            if a.line_index == b.line_index && layout.line_range(a.line_index).is_some_and(|r| byte+1 < r.end) {
                                assert!(y.x_em-x.x_em > 0.1, "space lost at byte {byte}: source={:?}, projected={:?}, width={width}, advance={}", doc.source().to_string(), c.rich.text, y.x_em-x.x_em);
                            }
                        }
                    }
                }
                assert_eq!(doc.source().to_string(), *sample);
                view.release(&mut text);
            }
        }
    }

    #[test]
    fn display_math_uses_real_scripts_and_fraction_rules_with_atomic_source_selection() {
        let (mut text, faces) = setup();
        for source in ["[\nP(\\text{at least one lost}) = 1-(1-p)^n\n]", r"\[\frac{1}{n^2}\]", r"$$\sqrt{x_1}$$"] {
            let doc = Document::new(source);
            let mut view = Preview::new(911);
            view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
            let scene = view.scene(&mut text, &doc, Rect::new(10., 20., 400., 200.), Vec2::new(0., 0.));
            assert!(scene.draws.iter().any(|d| d.size < 17.), "scripts/fraction content actually scales");
            assert!(scene.draws.len() > 1);
            if source.contains("frac") || source.contains("sqrt") { assert!(!scene.over.is_empty()); }
            let selection = view.selection(&scene, &text, &doc, 0..source.len());
            assert_eq!(selection.len(), 1, "math is one atomic selectable source range");
            let rect = selection[0].rect;
            let point = Vec2::new(rect.x+rect.width*0.25, rect.y+rect.height*0.5);
            let byte = view.hit_source(&scene, point, &text, &doc).unwrap();
            assert!(source.is_char_boundary(byte));
            assert!(view.nearest_source(&scene, point, &text, &doc).is_some());
            assert!(!view.copy_selection(&doc, 0..source.len()).contains("\\text"));
            let work = view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
            assert_eq!(work.layout_requests, 0);
            assert_eq!(doc.source().to_string(), source);
            view.release(&mut text);
        }
    }

    #[test]
    fn streamed_math_matches_cold_layout_and_real_fences_stay_literal() {
        let (mut text, faces) = setup();
        let source = "\\[\nP(\\text{at least one lost}) = 1-(1-p)^n\n\\]";
        let mut doc = Document::default();
        let mut view = Preview::new(912);
        for (step, c) in source.chars().enumerate() {
            doc.append(&c.to_string()).unwrap();
            view.sync(&doc, &mut text, faces, Theme::default(), 250., 17.);
            let fresh = Document::new(&doc.source().to_string());
            let mut cold = Preview::new(913 + step as u32);
            cold.sync(&fresh, &mut text, faces, Theme::default(), 250., 17.);
            assert!((cold.height-view.height).abs() < 0.001, "source={:?}, cold={}, warm={}", doc.source().to_string(), cold.height, view.height);
            assert_eq!(view.copy_selection(&doc, 0..doc.source().len_bytes()), cold.copy_selection(&fresh, 0..fresh.source().len_bytes()));
            cold.release(&mut text);
        }
        view.release(&mut text);
        let fenced = Document::new(&format!("```text\n{source}\n```"));
        view.sync(&fenced, &mut text, faces, Theme::default(), 400., 17.);
        assert!(view.texts.values().all(|c| c.math.is_none()));
        assert!(view.copy_selection(&fenced, 0..fenced.source().len_bytes()).contains("\\text"));
    }

    #[test]
    #[cfg(feature = "syntax")]
    fn code_language_edits_repaint_without_reshaping_or_changing_copy() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("```rust\nfn main() { let s = \"🐶\"; }\n```\n");
        let mut view = Preview::new(910);
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        let Content::Code { lines, .. } = &doc.blocks()[0].content else { panic!() };
        let id = lines[0].id;
        let old = view.texts[&id].handle;
        let copy = view.copy_selection(&doc, 0..doc.source().len_bytes());
        assert!(view.texts[&id].paint_spans.len() > 1);
        let work = view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        assert_eq!(work.paint_snapshots, 0);
        doc.edit(3..7, "text").unwrap();
        let work = view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        assert_eq!(work.layout_requests, 0);
        assert_eq!(view.texts[&id].handle, old);
        assert!(view.texts[&id].paint.is_none());
        assert_eq!(view.copy_selection(&doc, 0..doc.source().len_bytes()), copy);
        view.release(&mut text);
    }

    #[test]
    fn link_underlines_stay_below_each_measured_baseline() {
        let (mut text, faces) = setup();
        for source in [
            "See [https://example.org with **bold**, _italic_ and `code` labels that wrap](https://example.org) here.",
            "# [A heading link that wraps](https://example.org)",
            "[first line\nsecond line](https://example.org)",
            "> [A quoted link that wraps](https://example.org)",
            "| Link |\n| --- |\n| [A table link that wraps](https://example.org) |",
        ] {
            let doc = Document::new(source);
            for size in [12., 16., 28.] {
                let theme = Theme::default();
                let mut view = Preview::new(902);
                view.sync(&doc, &mut text, faces, theme, 130., size);
                let scene = view.scene(
                    &mut text,
                    &doc,
                    Rect::new(30., 60., 130., 1600.),
                    Vec2::new(7., 11.),
                );
                assert!(
                    scene.over.len() > 1,
                    "fixture must exercise multiple lines/runs"
                );
                let mut underlines = scene.over.iter();
                for &(id, at) in &scene.placed {
                    let c = &view.texts[&id];
                    let layout = text.measure(c.handle);
                    for run in &c.rich.runs {
                        if run.flags & inline::LINK == 0 {
                            continue;
                        }
                        for span in layout.selection(run.range.clone()) {
                            let line = layout.line(span.line_index).unwrap();
                            let baseline = at.y + line.baseline_em * c.size;
                            let underline = underlines.next().unwrap();
                            let gap_em = (underline.rect.y - baseline) / c.size;
                            assert!(
                                (0.04..0.2).contains(&gap_em),
                                "underline must sit just below baseline, not across glyphs: gap={gap_em}em, size={size}, source={source:?}"
                            );
                            assert!(
                                underline.rect.y + underline.rect.height
                                    <= at.y + (line.top_em + line.height_em) * c.size
                            );
                            assert!((underline.rect.x - (at.x + span.x_em * c.size)).abs() < 0.001);
                            assert!((underline.rect.width - span.width_em * c.size).abs() < 0.001);
                            assert_eq!(underline.rect.height, 1.);
                            assert_eq!(underline.color, theme.accent());
                        }
                    }
                }
                assert!(
                    underlines.next().is_none(),
                    "only link spans are underlined"
                );
                view.release(&mut text);
            }
        }
    }

    #[test]
    fn links_and_copy_follow_projected_text_even_when_only_destination_changes() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("**Read** [Docs](https://example.org/old) &amp; café");
        let mut view = Preview::new(900);
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        assert_eq!(
            view.copy_selection(&doc, 0..doc.source().len_bytes()),
            "Read Docs & café"
        );
        let id = doc.blocks()[0].elements().next().unwrap().id;
        let scene = view.scene(
            &mut text,
            &doc,
            Rect::new(0., 0., 500., 200.),
            Vec2::new(0., 0.),
        );
        let at = scene
            .placed
            .iter()
            .find(|(placed, _)| *placed == id)
            .unwrap()
            .1;
        let c = &view.texts[&id];
        let layout = text.measure(c.handle);
        let caret = layout.caret_rect(layout.caret_at("Read D".len()));
        let point = Vec2::new(
            at.x + caret.x_em * c.size,
            at.y + (caret.y_em + caret.height_em * 0.5) * c.size,
        );
        assert_eq!(
            view.hit_link(&scene, point, &text).as_deref(),
            Some("https://example.org/old")
        );
        assert!(
            !view
                .selection(&scene, &text, &doc, 0..doc.source().len_bytes())
                .is_empty()
        );
        let source = doc.source().to_string();
        let start = source.find("/old").unwrap();
        doc.edit(start..start + 4, "/new").unwrap();
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        let scene = view.scene(
            &mut text,
            &doc,
            Rect::new(0., 0., 500., 200.),
            Vec2::new(0., 0.),
        );
        assert_eq!(
            view.hit_link(&scene, point, &text).as_deref(),
            Some("https://example.org/new")
        );
        assert_eq!(
            view.copy_selection(&doc, 0..doc.source().len_bytes()),
            "Read Docs & café"
        );
    }

    #[test]
    fn soft_breaks_stream_and_copy_as_real_lines() {
        let (mut text, faces) = setup();
        for source in ["first\nsecond", "- first\n  second", "> first\n> second"] {
            let mut doc = Document::default();
            let mut view = Preview::new(901);
            for chunk in source.split_inclusive('\n') {
                doc.append(chunk).unwrap();
                view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
            }
            assert_eq!(doc.source().to_string(), source);
            assert_eq!(doc.blocks().len(), 1);
            let c = &view.texts[&doc.blocks()[0].id];
            assert_eq!(c.rich.text, "first\nsecond");
            assert_eq!(text.measure(c.handle).line_count(), 2);
            assert_eq!(
                view.copy_selection(&doc, 0..doc.source().len_bytes()),
                "first\nsecond"
            );
            doc.append("\n\nthird").unwrap();
            view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
            assert_eq!(doc.blocks().len(), 2, "blank lines still split paragraphs");
            view.release(&mut text);
        }
    }

    #[test]
    fn heights_append_update_splice_and_search() {
        let mut h = Heights::from(vec![10., 20., 30.]);
        assert_eq!(h.total(), 60.);
        assert_eq!(h.row_at(10.), 1);
        h.push(40.);
        assert_eq!(h.prefix(4), 100.);
        h.set(1, 25.);
        assert_eq!(h.total(), 105.);
        h.splice(1..2, 2);
        assert_eq!(h.values, vec![10., 0., 0., 30., 40.]);
        h.set(1, 4.);
        h.set(2, 6.);
        assert_eq!(h.total(), 90.);
    }
    fn setup() -> (TextService, Faces) {
        let mut text = TextService::new();
        let bytes: [&'static [u8]; 4] = [
            include_bytes!("../tests/fonts/DejaVuSans.ttf"),
            include_bytes!("../tests/fonts/DejaVuSans-Bold.ttf"),
            include_bytes!("../tests/fonts/DejaVuSans-Oblique.ttf"),
            include_bytes!("../tests/fonts/DejaVuSans-BoldOblique.ttf"),
        ];
        let prose = bytes.map(|bytes| {
            let f = text.map_font(Arc::new(bytes), 0).unwrap();
            text.register_chain(&[f]).expect("font chain capacity")
        });
        let faces = Faces { prose, mono: prose };
        (text, faces)
    }
    #[test]
    fn color_theme_never_parses_or_shapes_and_tables_update_one_row() {
        let (mut text, faces) = setup();
        let mut doc = Document::new(
            "# Hi\n\n**bold _both_** and `code` &amp; [link](url)\n\n| A | B |\n| --- | ---: |\n| x | **y** |\n",
        );
        let mut view = Preview::new(300);
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        let revision = doc.revision();
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        let theme = Theme {
            alternate: true,
            ..Default::default()
        };
        let w = view.sync(&doc, &mut text, faces, theme, 500., 17.);
        assert_eq!(w.layout_requests, 0);
        assert_eq!(doc.revision(), revision);
        #[cfg(feature = "perf-counters")]
        {
            let c = sanscale::profiling::work_counters();
            assert_eq!((c.shape_calls, c.flow_calls, c.source_reads), (0, 0, 0));
        }
        doc.append("| new | a much longer streamed cell that wraps over several lines without changing column widths |\n").unwrap();
        let w = view.sync(&doc, &mut text, faces, theme, 500., 17.);
        assert_eq!(w.measured_rows, 1);
        assert_eq!(w.layout_requests, 2);
        assert!(w.indexed_rows <= 1);
        view.release(&mut text);
    }
    #[test]
    fn missed_history_and_multiple_edits_reconcile_without_losing_rows() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("| A | B |\n| --- | --- |\n");
        let mut view = Preview::new(301);
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        for _ in 0..4 {
            doc.append("| x | y |\n").unwrap();
        }
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        for _ in 0..70 {
            doc.append("| xx | yy |\n").unwrap();
        }
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        let scene = view.scene(
            &mut text,
            &doc,
            Rect::new(0., 0., 400., 200.),
            Vec2::new(0., 0.),
        );
        assert!(!scene.draws.is_empty());
        assert!(scene.draws.len() < 20);
        view.release(&mut text);
    }
    #[test]
    fn changed_row_height_moves_following_blocks_without_reshaping_them() {
        let (mut text, faces) = setup();
        let mut doc =
            Document::new("| A | B |\n| --- | --- |\n| short | value |\n\nA following paragraph.");
        let mut view = Preview::new(302);
        view.sync(&doc, &mut text, faces, Theme::default(), 320., 17.);
        let after = doc.blocks()[1].id;
        let old_y = view.block_y(after).unwrap();
        let old_handle = view.texts[&after].handle;
        let at = doc.source().to_string().find("value").unwrap();
        doc.edit(
            at..at + 5,
            "a much longer cell that takes several visual lines to display",
        )
        .unwrap();
        let w = view.sync(&doc, &mut text, faces, Theme::default(), 320., 17.);
        assert_eq!(w.layout_requests, 1);
        assert_eq!(w.measured_rows, 1);
        assert!(view.block_y(after).unwrap() > old_y);
        assert_eq!(view.texts[&after].handle, old_handle);
        view.release(&mut text);
    }
    #[test]
    fn projected_clicks_map_entities_and_shifted_cells_back_to_source() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("| A | B |\n| --- | --- |\n| x | &amp; café |\n");
        let mut view = Preview::new(303);
        for value in ["x", "much longer"] {
            if value != "x" {
                let i = doc.source().to_string().find("| x |").unwrap() + 2;
                doc.edit(i..i + 1, value).unwrap();
            }
            view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
            let scene = view.scene(
                &mut text,
                &doc,
                Rect::new(10., 20., 500., 300.),
                Vec2::new(0., 0.),
            );
            let Content::Table(t) = &doc.blocks()[0].content else {
                panic!()
            };
            let id = t.rows[1].cells[1].id;
            let c = &view.texts[&id];
            let at = scene.placed.iter().find(|p| p.0 == id).unwrap().1;
            let layout = text.measure(c.handle);
            let caret = layout.caret_rect(layout.caret_at(0));
            let point = Vec2::new(
                at.x + caret.x_em * c.size,
                at.y + (caret.y_em + caret.height_em * 0.5) * c.size,
            );
            assert_eq!(
                view.hit_source(&scene, point, &text, &doc),
                doc.source().to_string().find("&amp;")
            );
        }
        view.release(&mut text);
    }
    #[test]
    fn reusing_a_view_for_another_document_cannot_alias_old_keys() {
        let (mut text, faces) = setup();
        let a = Document::new("old **content**");
        let b = Document::new("new _different words_");
        let mut view = Preview::new(304);
        view.sync(&a, &mut text, faces, Theme::default(), 400., 17.);
        view.sync(&b, &mut text, faces, Theme::default(), 400., 17.);
        let c = &view.texts[&b.blocks()[0].id];
        assert_eq!(
            text.measure(c.handle).len_bytes(),
            "new different words".len()
        );
        view.release(&mut text);
    }
    #[test]
    fn edited_layout_matches_a_fresh_view_including_table_structure() {
        let (mut text, faces) = setup();
        let mut doc = Document::new(
            "# Header\n\n| A | B |\n| --- | --- |\n| first | **second** |\n| third | fourth |\n\nAfter the table.\n",
        );
        let mut view = Preview::new(305);
        let mut seed = 73u64;
        for step in 0..100 {
            view.sync(&doc, &mut text, faces, Theme::default(), 380., 17.);
            let mut cold = Preview::new(10000 + step);
            cold.sync(&doc, &mut text, faces, Theme::default(), 380., 17.);
            assert!((view.height - cold.height).abs() < 0.02);
            for block in doc.blocks() {
                let a = &view.blocks[&block.id];
                let b = &cold.blocks[&block.id];
                assert!((a.y - b.y).abs() < 0.02);
                assert!((a.height - b.height).abs() < 0.02);
            }
            cold.release(&mut text);
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let s = doc.source().to_string();
            let points = s
                .char_indices()
                .map(|(i, _)| i)
                .chain([s.len()])
                .collect::<Vec<_>>();
            let a = seed as usize % points.len();
            let z = (a + ((seed >> 24) as usize) % 4).min(points.len() - 1);
            doc.edit(
                points[a]..points[z],
                [" | ", "\n", "**", "é", "", ":---:"][(seed >> 32) as usize % 6],
            )
            .unwrap();
        }
        view.release(&mut text);
    }
    #[test]
    fn combined_faces_grapheme_boundaries_and_real_hard_breaks() {
        let (mut text, faces) = setup();
        let doc = Document::new("***both*** **a**\u{301}  \nnext");
        let mut view = Preview::new(306);
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        let c = &view.texts[&doc.blocks()[0].id];
        assert_eq!(c.rich.text, "both a\u{301}\nnext");
        assert_eq!(
            c.fonts,
            vec![
                FontSpan {
                    range: 0..4,
                    chain: faces.prose[3]
                },
                FontSpan {
                    range: 5..8,
                    chain: faces.prose[1]
                }
            ]
        );
        assert_eq!(text.measure(c.handle).line_count(), 2);
        assert_eq!(text.measure(c.handle).len_bytes(), c.rich.text.len());
        let w = view.sync(
            &doc,
            &mut text,
            faces,
            Theme {
                italic: false,
                ..Default::default()
            },
            500.,
            17.,
        );
        assert_eq!(w.layout_requests, 1);
        assert_eq!(
            view.texts[&doc.blocks()[0].id].fonts[0].chain,
            faces.prose[1]
        );
        view.release(&mut text);
    }
}
