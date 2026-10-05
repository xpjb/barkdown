//! Small, bounded native math subset. No TeX execution, macros, files or network.
//! Unsupported commands leave the complete authored expression literal.
use sanscale::{Align, Color, Draw, FontChainHandle, Rect, ShapedHandle, Style, TextService, Vec2};
use std::ops::Range;

const MAX_BYTES: usize = 4096;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Text(String),
    Row(Vec<Expr>),
    Scripts { base: Box<Expr>, sub: Option<Box<Expr>>, sup: Option<Box<Expr>> },
    Fraction(Box<Expr>, Box<Expr>),
    Sqrt(Box<Expr>),
}
impl Expr {
    /// Readable, mathematically explicit copy/inline representation. Display
    /// expressions additionally get real positioned scripts/fraction rules.
    pub fn plain(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Row(row) => row.iter().map(Self::plain).collect(),
            Self::Fraction(a, b) => format!("({})/({})", a.plain(), b.plain()),
            Self::Sqrt(a) => format!("√({})", a.plain()),
            Self::Scripts { base, sub, sup } => {
                let mut s = base.plain();
                for (expr, chars, mapped, marker) in [
                    (sub, "0123456789+-=()aehijklmnoprstuvx", "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ", '_'),
                    (sup, "0123456789+-=()abcdefghijklmnoprstuvwxyz", "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ᵃᵇᶜᵈᵉᶠᵍʰⁱʲᵏˡᵐⁿᵒᵖʳˢᵗᵘᵛʷˣʸᶻ", '^'),
                ] {
                    if let Some(expr) = expr {
                        let value = expr.plain();
                        let converted: Option<String> = value.chars().map(|c| {
                            chars.chars().position(|x| x == c).and_then(|i| mapped.chars().nth(i))
                        }).collect();
                        if let Some(value) = converted { s.push_str(&value); }
                        else { s.push(marker); s.push('('); s.push_str(&value); s.push(')'); }
                    }
                }
                s
            }
        }
    }
}

pub fn parse(source: &str) -> Option<Expr> {
    if source.len() > MAX_BYTES { return None; }
    let mut p = Parser { s: source, at: 0 };
    let expr = p.row(0, false)?;
    (!matches!(&expr, Expr::Row(v) if v.is_empty())).then_some(expr)
}
struct Parser<'a> { s: &'a str, at: usize }
impl Parser<'_> {
    fn peek(&self) -> Option<char> { self.s[self.at..].chars().next() }
    fn take(&mut self) -> Option<char> { let c = self.peek()?; self.at += c.len_utf8(); Some(c) }
    fn space(&mut self) { while self.peek().is_some_and(char::is_whitespace) { self.take(); } }
    fn row(&mut self, depth: usize, grouped: bool) -> Option<Expr> {
        if depth > 32 { return None; }
        let mut row = Vec::new();
        loop {
            self.space();
            if self.peek() == Some('}') {
                if !grouped { return None; }
                self.take(); break;
            }
            if self.peek().is_none() { if grouped { return None; } else { break; } }
            let mut base = self.atom(depth+1)?;
            let (mut sub, mut sup) = (None, None);
            loop {
                self.space();
                let target = match self.peek() { Some('_') => &mut sub, Some('^') => &mut sup, _ => break };
                if target.is_some() { return None; }
                self.take(); self.space();
                *target = Some(Box::new(self.atom(depth+1)?));
            }
            if sub.is_some() || sup.is_some() { base = Expr::Scripts { base: Box::new(base), sub, sup }; }
            // Adjacent ordinary atoms shape as one run, keeping normal kerning.
            if let Expr::Text(s) = &base && let Some(Expr::Text(last)) = row.last_mut() {
                last.push_str(s);
            } else { row.push(base); }
        }
        Some(Expr::Row(row))
    }
    fn group(&mut self, depth: usize) -> Option<Expr> {
        self.space();
        if self.take()? != '{' { return None; }
        self.row(depth+1, true)
    }
    fn atom(&mut self, depth: usize) -> Option<Expr> {
        if depth > 32 { return None; }
        match self.take()? {
            '{' => self.row(depth+1, true),
            '^' | '_' | '}' | '$' | '#' | '%' | '&' => None,
            '\\' => {
                let start = self.at;
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) { self.take(); }
                let command = &self.s[start..self.at];
                if command.is_empty() {
                    return Some(Expr::Text(match self.take()? {
                        ',' | ';' | ':' | ' ' => " ".into(), '!' => "".into(),
                        c @ ('{' | '}' | '_' | '%' | '#' | '$' | '|') => c.to_string(), _ => return None,
                    }));
                }
                match command {
                    "text" | "mathrm" | "operatorname" => {
                        self.space();
                        if self.take()? != '{' { return None; }
                        let start = self.at;
                        while !matches!(self.peek(), None | Some('}' | '{' | '\\')) { self.take(); }
                        let end = self.at;
                        if self.take()? != '}' { return None; }
                        Some(Expr::Text(self.s[start..end].to_owned()))
                    }
                    "frac" | "dfrac" | "tfrac" => Some(Expr::Fraction(Box::new(self.group(depth)?), Box::new(self.group(depth)?))),
                    "sqrt" => Some(Expr::Sqrt(Box::new(self.group(depth)?))),
                    "left" | "right" => {
                        self.space();
                        if self.peek() == Some('.') { self.take(); Some(Expr::Text(String::new())) }
                        else { self.atom(depth+1) }
                    },
                    "quad" | "qquad" => Some(Expr::Text("  ".into())),
                    name => Some(Expr::Text(match name {
                        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
                        "epsilon" | "varepsilon" => "ε", "theta" => "θ", "lambda" => "λ",
                        "mu" => "μ", "nu" => "ν", "pi" => "π", "rho" => "ρ", "sigma" => "σ",
                        "tau" => "τ", "phi" | "varphi" => "φ", "psi" => "ψ", "omega" => "ω",
                        "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
                        "Pi" => "Π", "Sigma" => "Σ", "Phi" => "Φ", "Psi" => "Ψ", "Omega" => "Ω",
                        "times" => " × ", "cdot" => " · ", "pm" => " ± ", "mp" => " ∓ ",
                        "le" | "leq" => " ≤ ", "ge" | "geq" => " ≥ ", "ne" | "neq" => " ≠ ",
                        "approx" => " ≈ ", "equiv" => " ≡ ", "infty" => "∞", "partial" => "∂",
                        "sum" => "∑", "prod" => "∏", "int" => "∫", "in" => " ∈ ",
                        "to" | "rightarrow" => " → ", "Rightarrow" | "implies" => " ⇒ ",
                        "ldots" | "dots" => "…", "cdots" => "⋯", "lvert" | "rvert" => "|",
                        "sin" => "sin", "cos" => "cos", "tan" => "tan", "log" => "log", "ln" => "ln",
                        "exp" => "exp", "lim" => "lim", "min" => "min", "max" => "max",
                        _ => return None,
                    }.into())),
                }
            }
            c => Some(Expr::Text(c.to_string())),
        }
    }
}

pub(crate) struct Region { pub full: Range<usize>, pub inner: Range<usize> }
/// Scan outside code. Single-dollar currency/whitespace ambiguities stay literal.
pub(crate) fn regions(s: &str, code: &[(Range<usize>, Range<usize>)]) -> Vec<Region> {
    let trimmed = s.trim();
    if trimmed.starts_with("[\n") && trimmed.ends_with("\n]") {
        let at = s.len() - s.trim_start().len();
        let inner = at+2..at+trimmed.len()-2;
        if s[inner.clone()].contains(['\\', '^', '_']) && code.is_empty() {
            return vec![Region { full: at..at+trimmed.len(), inner }];
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut code_index = 0;
    while i < s.len() {
        while code.get(code_index).is_some_and(|(full, _)| full.end <= i) { code_index += 1; }
        if let Some((full, _)) = code.get(code_index).filter(|(full, _)| full.contains(&i)) { i = full.end; continue; }
        let tail = &s[i..];
        let delimiter = if tail.starts_with("\\(") { Some((2, "\\)", false)) }
            else if tail.starts_with("\\[") { Some((2, "\\]", false)) }
            else if tail.starts_with("$$") { Some((2, "$$", false)) }
            else if tail.starts_with('$') && tail[1..].chars().next().is_some_and(|c| !c.is_whitespace() && c != '$') { Some((1, "$", true)) }
            else { None };
        if let Some((n, close, single)) = delimiter {
            let previous_count = out.len();
            let start = i+n;
            let mut end = start;
            while end < s.len() && end-start <= MAX_BYTES {
                if single && s.as_bytes()[end] == b'\n' { break; }
                if s[end..].starts_with(close) && end > start
                    && (!single || (!s[..end].ends_with(char::is_whitespace)
                        && !s[end+1..].starts_with(|c: char| c.is_ascii_digit()))) {
                    out.push(Region { full: i..end+close.len(), inner: start..end });
                    i = end+close.len(); break;
                }
                if s.as_bytes()[end] == b'\\' && !s[end..].starts_with(close) {
                    end += 1;
                    if end == s.len() { break; }
                }
                end += s[end..].chars().next().unwrap().len_utf8();
            }
            if out.len() != previous_count { continue; }
        }
        if s.as_bytes()[i] == b'\\' { i += 1; if i == s.len() { break; } }
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    out
}

#[derive(Clone)]
struct Glyphs { text: String, handle: ShapedHandle, at: Vec2, scale: f32 }
#[derive(Clone, Default)]
pub(crate) struct Layout {
    pub width: f32, pub ascent: f32, pub descent: f32,
    glyphs: Vec<Glyphs>, rules: Vec<Rect>,
}
impl Layout {
    pub fn height(&self) -> f32 { self.ascent + self.descent }
    fn append(&mut self, mut other: Self, x: f32, y: f32) {
        self.width = self.width.max(x + other.width);
        self.ascent = self.ascent.max(other.ascent - y);
        self.descent = self.descent.max(other.descent + y);
        for part in &mut other.glyphs { part.at.x += x; part.at.y += y; }
        for rule in &mut other.rules { rule.x += x; rule.y += y; }
        self.glyphs.extend(other.glyphs); self.rules.extend(other.rules);
    }
    pub fn new(expr: &Expr, text: &mut TextService, chain: FontChainHandle, scale: f32) -> Self {
        match expr {
            Expr::Text(s) => {
                let style = Style { chain, wrap_em: None, align: Align::Left, line_spacing: 1. };
                let handle = text.shape_transient(s, &style).expect("valid math text");
                let l = text.measure(handle);
                let ascent = l.line(0).map_or(0., |l| l.baseline_em) * scale;
                Self { width: l.width_em()*scale, ascent, descent: l.height_em()*scale-ascent,
                    glyphs: vec![Glyphs { text: s.clone(), handle, at: Vec2::new(0., -ascent), scale }], rules: vec![] }
            }
            Expr::Row(row) => {
                let mut out = Self::default();
                for part in row { let item = Self::new(part, text, chain, scale); out.append(item, out.width, 0.); }
                out
            }
            Expr::Scripts { base, sub, sup } => {
                let mut out = Self::new(base, text, chain, scale);
                let x = out.width + 0.05*scale;
                if let Some(sup) = sup { let item = Self::new(sup, text, chain, scale*0.68); out.append(item, x, -0.6*scale); }
                if let Some(sub) = sub { let item = Self::new(sub, text, chain, scale*0.68); out.append(item, x, 0.35*scale); }
                out
            }
            Expr::Fraction(a, b) => {
                let a = Self::new(a, text, chain, scale*0.85);
                let b = Self::new(b, text, chain, scale*0.85);
                let width = a.width.max(b.width) + 0.3*scale;
                let mut out = Self::default();
                let (ax, ay) = ((width-a.width)*0.5, -0.3*scale-a.descent);
                let (bx, by) = ((width-b.width)*0.5, 0.1*scale+b.ascent);
                out.append(a, ax, ay); out.append(b, bx, by); out.width = width;
                out.rules.push(Rect::new(0., -0.15*scale, width, 0.05*scale)); out
            }
            Expr::Sqrt(expr) => {
                let mut out = Self::new(&Expr::Text("√".into()), text, chain, scale);
                let a = Self::new(expr, text, chain, scale);
                let (x, y, w) = (out.width, -a.ascent, a.width);
                out.append(a, x, 0.); out.rules.push(Rect::new(x, y, w, 0.04*scale)); out
            }
        }
    }
    pub fn fit(&mut self, width: f32) {
        let scale = (width / self.width.max(0.001)).min(1.);
        self.width *= scale; self.ascent *= scale; self.descent *= scale;
        for part in &mut self.glyphs { part.at.x *= scale; part.at.y *= scale; part.scale *= scale; }
        for rule in &mut self.rules { rule.x *= scale; rule.y *= scale; rule.width *= scale; rule.height *= scale; }
    }
    pub fn draw(&mut self, text: &mut TextService, chain: FontChainHandle, at: Vec2,
        size: f32, color: Color, clip: Rect, draws: &mut Vec<Draw>, rules: &mut Vec<crate::preview::Decoration>) {
        for part in &mut self.glyphs {
            if text.measure(part.handle).line_count() == 0 {
                part.handle = text.shape_transient(&part.text,
                    &Style { chain, wrap_em: None, align: Align::Left, line_spacing: 1. }).unwrap();
            }
            draws.push(Draw { block: part.handle, at: Vec2::new(at.x+part.at.x*size, at.y+(self.ascent+part.at.y)*size),
                size: size*part.scale, color, clip: Some(clip), paint: None });
        }
        rules.extend(self.rules.iter().map(|r| crate::preview::Decoration {
            rect: Rect::new(at.x+r.x*size, at.y+(self.ascent+r.y)*size, r.width*size, r.height*size), color,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probability_example_and_safe_fallbacks() {
        let expr = parse(r"P(\text{at least one lost}) = 1-(1-p)^n").unwrap();
        assert_eq!(expr.plain(), "P(at least one lost)=1-(1-p)ⁿ");
        assert_eq!(parse(r"\frac{\alpha_1}{\sqrt{x^2}}").unwrap().plain(), "(α₁)/(√(x²))");
        for source in [r"\unknown{x}", r"\input{/etc/passwd}", "x^", "{x", "x}", "x^^2"] { assert!(parse(source).is_none(), "{source}"); }
        assert!(parse(&"{".repeat(1000)).is_none());
    }
    #[test]
    fn adjacent_unfinished_math_always_makes_streaming_progress() {
        for source in ["$x$$unfinished", "$$x$$$$", r"\(x\)\(unfinished", r"\[x\]$5"] {
            assert_eq!(regions(source, &[]).len(), 1, "{source}");
        }
        assert_eq!(crate::markdown::inline::parse("$x$$unfinished").text, "x$unfinished");
    }
    #[test]
    fn delimiters_currency_escapes_and_code() {
        for source in ["[\nP(\\text{at least one lost}) = 1-(1-p)^n\n]", r"\[x^n\]", r"\(x^n\)", "$x^n$", "$$\nx^n\n$$"] {
            assert_eq!(regions(source, &[]).len(), 1, "{source}");
        }
        for source in [r"\$5 and $10", "$5 and $10", "[ordinary text]", r"\\(literal)", "$ unfinished $"] {
            assert!(regions(source, &[]).is_empty(), "{source}");
        }
        let source = "`$x$`";
        assert!(regions(source, &[(0..source.len(), 1..source.len()-1)]).is_empty());
    }
}
