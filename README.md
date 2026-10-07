# 🐶 Barkdown

**Woof. Markdown with a little bite.**

A window-independent, incremental Markdown model and Sanscale view shared by
Tau Beta and Compendium. No app, networking, image loader or font discovery is
hidden inside the library. Consumers supply fonts and own link activation.

- Rope-backed source, streaming UTF-8, stable block/cell identities and source maps.
- Headings, lists, quotes, tables, inline styles, selection/copy and passive links.
- Fenced-code syntax colors shared by rendered previews and source-preserving
  editors; Rust, Python, JavaScript, JSON, shell and the other bundled Syntect
  grammars. Unknown/omitted language names stay plain. No external grammar downloads.
- Syntax is immutable paint, not layout: colors never change caret geometry.
  Multiline strings/comments are parsed as a whole fence. Unchanged fences reuse
  paint; edited fences are rehighlighted within a 512 KiB / 16 KiB-per-line budget.
  Over-budget code remains plain and copyable. `--no-default-features` omits Syntect.
- Math: `\(…\)`, `\[…\]`, `$…$`, `$$…$$`, plus standalone bracketed
  equations containing TeX/script syntax (the LLM probability-example form).
  Standalone equations get native positioned scripts, fractions and root rules;
  inline math gets a readable Unicode projection. Supported commands include
  `\text`, `\frac`, `\sqrt`, common Greek letters/operators and scripts.
  This is **not full LaTeX**: no macros/environments/matrices or TeX execution.
  Expressions are bounded to 4 KiB / 32 nesting levels. Unknown commands remain
  literal, never silently omitted. Display equations are atomically selectable;
  copy returns their readable text, while the Document retains exact TeX source.
  Blank lines still separate Markdown paragraphs; display math must stay within
  a paragraph. Actual code fences/code spans always stay literal.
- Raw citation transport markers project to `[citation unavailable]` when no
  source metadata is available; opaque IDs are never fabricated into links.
- Tabs render with four spaces of advance in Sanscale, not tofu, retaining their
  original source bytes/caret stops. Configurable tab stops are not implemented.
- Explicit ownership: call `Preview::release` before discarding a view or clearing
  its text service. Font reloads, source edits and width changes have separate paths.

```rust,ignore
let mut doc = barkdown::Document::new("");
let mut view = barkdown::Preview::new(unique_namespace);
doc.append(chunk)?;
view.sync(&doc, &mut text, faces, theme, width, font_size);
let scene = view.scene(&mut text, &doc, viewport, scroll);
// Draw scene.under, scene.draws, scene.over with the consumer's clipping/pass.
view.release(&mut text);

// Or: retain source text and its existing editor geometry (Compendium).
let mut highlighter = barkdown::syntax::Highlighter::default();
let spans = highlighter.source_spans(&doc); // original-source UTF-8 byte ranges
```

The preserved [dialect/design notes](src/markdown/README.md) describe supported
syntax and incremental parsing bounds. This is a young API, not a full CommonMark
conformance claim. Source markers remain visible in a source-preserving editor;
`Preview` is the rendered Markdown path.

## Source and rendered note layouts

`Preview::sync` keeps the existing compact chat policy. Notes can pass their
normal Sanscale `Style` to `sync_styled(..., style, size,
BlockSpacing::PreserveSource)`. Its base chain must be `faces.prose[0]`, and its
wrap width must be positive. Consecutive ordinary paragraphs and real empty lines
share existing Sanscale blocks; inline fonts/paint remain spans. The adapter joins
already-projected content with rebased origins, never reparses across paragraphs.
Headings, code panels, tables and math retain their specialised structures.

`Preview::sync_source(doc, text, faces, style, size)` prepares literal source with
semantic code fonts through the **same** text-cache/paragraph shaping path and
returns `(ShapedHandle, Option<PaintHandle>)`. Consumers keep Sanscale's existing
measurement/caret/selection APIs, and may also use the view's scene/source-map
queries. Supply separate views/namespaces for simultaneously displayed widths or
presentations; share one Document. Per-paragraph identities include text/font
inputs, so editing a line does not reshape the entire grouped note. Draw colors
remain independent of layout. No new layout engine or Sanscale API is introduced.

For plain text, both presentations have identical geometry, including repeated,
leading and trailing empty lines. Markup removal and structural rendering can
legitimately change dimensions. Caller typography is also the appropriate policy
for any consumer-owned unstyled fallback.

## Provenance and pins

Extracted from `xpjb/tau`'s `markdown/` at
`9a510f3ce4994566d3e391399d89ba34d0c3943d`, previously `tau-markdown`.
That component originated in `xpjb/sanscale`'s Markdown editor example at
`2be4f316c3870d8cd9ba6e12b5e10b2af385bf2c`. Tau's selection, copy, link metadata
and baseline-relative underline fixes are retained. MIT OR Apache-2.0;
both license texts are included. Deterministic DejaVu test fonts carry their own
license in `tests/fonts/`; production fonts are supplied by each app.

Sanscale is pinned to `15ad1f03e17a368e2ff4f1b269f60bad13483638` (master as
verified October 5, 2026), including prepared-text residency fix `4325844` and source-preserving tab advances.
Consumers must use the same Git source/revision to share one TextService type.

## Checks

Use the host's managed Cargo; nextest, not Cargo's built-in test runner. No Clippy.

```sh
cargo check --locked --all-targets
cargo nextest run --locked --all-features
cargo check --locked --no-default-features
cargo doc --locked --no-deps --all-features
```
