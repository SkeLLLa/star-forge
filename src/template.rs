//! Styled group templates: starship-like `format` strings parsed once at config load into an
//! AST, rendered per request from a badge-value lookup.
//!
//! Syntax: `{badge}` variable; `[text](style)` styled span (nestable); `( .. )` conditional
//! section; `\[ \] \( \) \{ \} \\` escapes (a backslash before anything else stays literal).
//!
//! Semantics decisions (starship-compatible where documented, otherwise chosen here):
//! - A `( .. )` section renders only if at least one `{var}` inside it (recursively, through
//!   spans and shown nested sections) is non-empty. A section with no variables at all is never
//!   rendered (mirrors starship's "any variable present" test; plain text does not count).
//! - A span whose rendered content is empty emits nothing (no stray escapes).
//! - Nested span style = outer style with the inner style string's tokens applied on top
//!   (`none` resets). After an inner span closes, the outer style is re-emitted.
//! - `Dialect::Ansi` output is raw SGR (`ESC[..m`), never wrapped for any shell (starship does
//!   the wrapping). `Dialect::Tmux` emits `#[..]` styles (`#[default]` closes a span) and doubles
//!   every literal `#` (template text and badge values); the dialect itself adds no control bytes (ESC in badge values passes through; tmux shows it
//!   literally). Both
//!   share the AST and the style-inheritance logic; only open/close/escape differ.
//! - Palette names are checked before builtin color names; palette values resolve one level
//!   (hex / named / 0-255), never to other palette names, so there are no loops.

use std::collections::BTreeMap;

/// Lowercased name -> color spec (`#rrggbb`, named, `bright-*`, 0-255).
pub type Palette = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    /// 0-15: the 8 named colors, then their bright variants.
    Named(u8),
    Fixed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    fn tmux(self) -> String {
        match self {
            Self::Named(n) => {
                let name = NAMED[usize::from(n % 8)].replace("purple", "magenta");
                if n < 8 { name } else { format!("bright{name}") }
            }
            Self::Fixed(n) => format!("colour{n}"),
            Self::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    fn sgr(self, bg: bool) -> String {
        match self {
            Self::Named(n) => {
                let base = match (n < 8, bg) {
                    (true, false) => 30,
                    (true, true) => 40,
                    (false, false) => 90 - 8,
                    (false, true) => 100 - 8,
                };
                (base + u16::from(n)).to_string()
            }
            Self::Fixed(n) => format!("{};5;{n}", if bg { 48 } else { 38 }),
            Self::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if bg { 48 } else { 38 }),
        }
    }
}

/// How a rendered line is consumed: starship (ANSI SGR) or a tmux `#()` status segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dialect {
    #[default]
    Ansi,
    Tmux,
}

impl Dialect {
    /// Escapes literal text for this dialect (tmux re-expands `#` sequences in `#()` output).
    pub fn escape(self, s: &str) -> std::borrow::Cow<'_, str> {
        if self == Self::Tmux && s.contains('#') {
            s.replace('#', "##").into()
        } else {
            s.into()
        }
    }

    /// Starts `eff`; `reset_first` when an outer style is active and must be replaced.
    fn open(self, eff: &Style, reset_first: bool) -> String {
        match self {
            Self::Ansi => eff.sgr(reset_first),
            Self::Tmux => {
                let reset = if reset_first { "#[default]" } else { "" };
                format!("{reset}{}", eff.tmux())
            }
        }
    }

    const fn close(self) -> &'static str {
        match self {
            Self::Ansi => "\x1b[0m",
            Self::Tmux => "#[default]",
        }
    }

    /// Re-applies the outer style after an inner span closed.
    fn restore(self, parent: &Style) -> String {
        match self {
            Self::Ansi => parent.sgr(false),
            Self::Tmux => parent.tmux(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dimmed: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverted: bool,
    hidden: bool,
    strike: bool,
}

impl Style {
    /// `ESC[..m` for the full state; `reset_first` prefixes `0` so an inner span replaces
    /// (rather than adds to) whatever the outer span left active.
    fn sgr(&self, reset_first: bool) -> String {
        let mut codes: Vec<String> = Vec::new();
        if reset_first {
            codes.push("0".into());
        }
        for (on, code) in [
            (self.bold, "1"),
            (self.dimmed, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverted, "7"),
            (self.hidden, "8"),
            (self.strike, "9"),
        ] {
            if on {
                codes.push(code.into());
            }
        }
        if let Some(c) = self.bg {
            codes.push(c.sgr(true));
        }
        if let Some(c) = self.fg {
            codes.push(c.sgr(false));
        }
        format!("\x1b[{}m", codes.join(";"))
    }

    /// `#[fg=..,bg=..,attrs]`; empty for the default style.
    fn tmux(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = self.fg {
            parts.push(format!("fg={}", c.tmux()));
        }
        if let Some(c) = self.bg {
            parts.push(format!("bg={}", c.tmux()));
        }
        for (on, name) in [
            (self.bold, "bold"),
            (self.dimmed, "dim"),
            (self.italic, "italics"),
            (self.underline, "underscore"),
            (self.blink, "blink"),
            (self.inverted, "reverse"),
            (self.hidden, "hidden"),
            (self.strike, "strikethrough"),
        ] {
            if on {
                parts.push(name.into());
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("#[{}]", parts.join(","))
        }
    }

    fn apply(&mut self, ops: &[Op]) {
        for op in ops {
            match op {
                Op::Fg(c) => self.fg = Some(*c),
                Op::Bg(c) => self.bg = Some(*c),
                Op::Modifier(f) => f(self),
                Op::None => *self = Self::default(),
            }
        }
    }
}

/// One style-string token, applied in order onto the inherited style.
#[derive(Debug, Clone)]
enum Op {
    Fg(Color),
    Bg(Color),
    Modifier(fn(&mut Style)),
    None,
}

const NAMED: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
];

fn builtin_color(s: &str) -> Option<Color> {
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?.to_be_bytes();
        return Some(Color::Rgb(v[1], v[2], v[3]));
    }
    let (name, off) = s.strip_prefix("bright-").map_or((s, 0), |n| (n, 8));
    if let Some(i) = NAMED.iter().position(|n| *n == name) {
        return Some(Color::Named(u8::try_from(i).ok()? + off));
    }
    s.parse::<u8>().ok().map(Color::Fixed)
}

fn color(tok: &str, pal: &Palette) -> Result<Color, String> {
    if let Some(spec) = pal.get(tok) {
        return builtin_color(&spec.to_ascii_lowercase())
            .ok_or_else(|| format!("palette color `{tok}` has invalid value `{spec}`"));
    }
    builtin_color(tok).ok_or_else(|| format!("unknown color `{tok}`"))
}

fn parse_style(s: &str, pal: &Palette) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    for raw in s.split_whitespace() {
        let tok = raw.to_ascii_lowercase();
        ops.push(match tok.as_str() {
            "none" => Op::None,
            "bold" => Op::Modifier(|s| s.bold = true),
            "italic" => Op::Modifier(|s| s.italic = true),
            "underline" => Op::Modifier(|s| s.underline = true),
            "dimmed" => Op::Modifier(|s| s.dimmed = true),
            "inverted" => Op::Modifier(|s| s.inverted = true),
            "blink" => Op::Modifier(|s| s.blink = true),
            "hidden" => Op::Modifier(|s| s.hidden = true),
            "strikethrough" => Op::Modifier(|s| s.strike = true),
            t => {
                if let Some(c) = t.strip_prefix("fg:") {
                    Op::Fg(color(c, pal)?)
                } else if let Some(c) = t.strip_prefix("bg:") {
                    Op::Bg(color(c, pal)?)
                } else {
                    Op::Fg(color(t, pal)?)
                }
            }
        });
    }
    Ok(ops)
}

#[derive(Debug)]
enum Node {
    Text(String),
    Var(String),
    Span(Vec<Op>, Vec<Self>),
    Cond(Vec<Self>),
}

/// A parsed `format`: the AST plus its distinct variables in first-use order.
#[derive(Debug)]
pub struct Template {
    nodes: Vec<Node>,
    vars: Vec<String>,
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    pal: &'a Palette,
    vars: Vec<String>,
}

impl Parser<'_> {
    fn seq(&mut self, close: Option<char>, open_at: usize) -> Result<Vec<Node>, String> {
        let mut nodes: Vec<Node> = Vec::new();
        loop {
            let at = self.pos;
            let Some(&c) = self.chars.get(at) else {
                return close.map_or(Ok(nodes), |c| {
                    Err(format!("at {}: unclosed group, missing `{c}`", open_at + 1))
                });
            };
            self.pos += 1;
            let lit = match c {
                _ if Some(c) == close => return Ok(nodes),
                '\\' => match self.chars.get(self.pos) {
                    Some(&n) if "[](){}\\".contains(n) => {
                        self.pos += 1;
                        n
                    }
                    _ => '\\',
                },
                '{' => {
                    let name = self.until('}', at)?;
                    if name.is_empty()
                        || !name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                    {
                        return Err(format!("at {}: invalid variable name `{name}`", at + 1));
                    }
                    if !self.vars.contains(&name) {
                        self.vars.push(name.clone());
                    }
                    nodes.push(Node::Var(name));
                    continue;
                }
                '[' => {
                    let children = self.seq(Some(']'), at)?;
                    if self.chars.get(self.pos) != Some(&'(') {
                        return Err(format!(
                            "at {}: `]` must be followed by `(style)`",
                            self.pos
                        ));
                    }
                    let sat = self.pos;
                    self.pos += 1;
                    let style = self.until(')', sat)?;
                    let ops = parse_style(&style, self.pal)
                        .map_err(|e| format!("at {}: {e}", sat + 1))?;
                    nodes.push(Node::Span(ops, children));
                    continue;
                }
                '(' => {
                    let children = self.seq(Some(')'), at)?;
                    nodes.push(Node::Cond(children));
                    continue;
                }
                ']' | ')' | '}' => {
                    return Err(format!(
                        "at {}: unmatched `{c}` (escape as `\\{c}`)",
                        at + 1
                    ));
                }
                c => c,
            };
            match nodes.last_mut() {
                Some(Node::Text(t)) => t.push(lit),
                _ => nodes.push(Node::Text(lit.to_string())),
            }
        }
    }

    /// Raw text up to (and consuming) `end`; no nesting or escapes.
    fn until(&mut self, end: char, open_at: usize) -> Result<String, String> {
        let rest = &self.chars[self.pos..];
        let n = rest
            .iter()
            .position(|c| *c == end)
            .ok_or_else(|| format!("at {}: unclosed, missing `{end}`", open_at + 1))?;
        let s: String = rest[..n].iter().collect();
        self.pos += n + 1;
        Ok(s)
    }
}

fn render(
    nodes: &[Node],
    get: &dyn Fn(&str) -> String,
    d: Dialect,
    parent: &Style,
    out: &mut String,
) -> bool {
    let mut any = false;
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(&d.escape(t)),
            Node::Var(v) => {
                let val = get(v);
                any |= !val.is_empty();
                out.push_str(&d.escape(&val));
            }
            Node::Cond(children) => {
                let mut inner = String::new();
                if render(children, get, d, parent, &mut inner) {
                    out.push_str(&inner);
                    any = true;
                }
            }
            Node::Span(ops, children) => {
                let mut eff = parent.clone();
                eff.apply(ops);
                let mut inner = String::new();
                any |= render(children, get, d, &eff, &mut inner);
                if inner.is_empty() {
                    continue;
                }
                if eff == *parent {
                    out.push_str(&inner);
                    continue;
                }
                let plain = Style::default();
                out.push_str(&d.open(&eff, *parent != plain));
                out.push_str(&inner);
                out.push_str(d.close());
                if *parent != plain {
                    out.push_str(&d.restore(parent));
                }
            }
        }
    }
    any
}

impl Template {
    /// Errors carry a 1-based char position; the caller adds the group name.
    pub fn parse(src: &str, pal: &Palette) -> Result<Self, String> {
        let mut p = Parser {
            chars: src.chars().collect(),
            pos: 0,
            pal,
            vars: Vec::new(),
        };
        let nodes = p.seq(None, 0)?;
        Ok(Self {
            nodes,
            vars: p.vars,
        })
    }

    /// Distinct `{name}` variables, in first-use order.
    pub fn vars(&self) -> &[String] {
        &self.vars
    }

    pub fn render(&self, d: Dialect, get: impl Fn(&str) -> String) -> String {
        let mut out = String::new();
        render(&self.nodes, &get, d, &Style::default(), &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> Palette {
        Palette::from([
            ("mauve".into(), "#C6A0F6".into()),
            ("sky".into(), "117".into()),
        ])
    }

    fn rend(src: &str, vals: &[(&str, &str)]) -> String {
        rend_in(Dialect::Ansi, src, vals)
    }

    fn rend_in(d: Dialect, src: &str, vals: &[(&str, &str)]) -> String {
        let t = Template::parse(src, &pal()).unwrap_or_else(|e| panic!("{src}: {e}"));
        t.render(d, |n| {
            vals.iter()
                .find(|(k, _)| *k == n)
                .map_or_else(String::new, |(_, v)| (*v).to_string())
        })
    }

    fn sgr_of(style: &str) -> String {
        let mut s = Style::default();
        s.apply(&parse_style(style, &pal()).unwrap());
        s.sgr(false)
    }

    #[test]
    fn style_colors() {
        assert_eq!(sgr_of("red"), "\x1b[31m");
        assert_eq!(sgr_of("FG:Green"), "\x1b[32m");
        assert_eq!(sgr_of("bg:blue"), "\x1b[44m");
        assert_eq!(sgr_of("bright-red"), "\x1b[91m");
        assert_eq!(sgr_of("bg:bright-white"), "\x1b[107m");
        assert_eq!(sgr_of("#ff8000"), "\x1b[38;2;255;128;0m");
        assert_eq!(sgr_of("bg:#000000"), "\x1b[48;2;0;0;0m");
        assert_eq!(sgr_of("fg:208"), "\x1b[38;5;208m");
        assert_eq!(sgr_of("bg:7"), "\x1b[48;5;7m");
        assert_eq!(sgr_of("mauve"), "\x1b[38;2;198;160;246m");
        assert_eq!(sgr_of("bg:sky"), "\x1b[48;5;117m");
    }

    #[test]
    fn style_modifiers_and_none() {
        assert_eq!(sgr_of("bold italic underline red"), "\x1b[1;3;4;31m");
        assert_eq!(
            sgr_of("dimmed blink inverted hidden strikethrough"),
            "\x1b[2;5;7;8;9m"
        );
        assert_eq!(sgr_of("bold red none blue"), "\x1b[34m");
    }

    #[test]
    fn style_errors() {
        let e = |s: &str| parse_style(s, &pal()).unwrap_err();
        assert!(e("fg:nope").contains("`nope`"));
        assert!(e("#12345").contains("#12345"));
        assert!(e("256").contains("`256`"));
        assert!(e("bg:bright-nope").contains("bright-nope"));
        let bad = Palette::from([("x".into(), "zzz".into())]);
        assert!(parse_style("x", &bad).unwrap_err().contains("palette"));
    }

    #[test]
    fn parse_errors() {
        let e = |s: &str| Template::parse(s, &pal()).unwrap_err();
        assert!(e("[a](red").contains("at 4"));
        assert!(e("[a]").contains("`(style)`"));
        assert!(e("(a").contains("at 1"));
        assert!(e("a)").contains("unmatched"));
        assert!(e("{a").contains("at 1"));
        assert!(e("{}").contains("invalid variable"));
        assert!(e("[a](nope)").contains("unknown color"));
    }

    #[test]
    fn escapes_and_text() {
        assert_eq!(rend(r"\[x\] \(y\) \{z\} \\ \q", &[]), r"[x] (y) {z} \ \q");
    }

    #[test]
    fn vars_collected_in_order_deduped() {
        let t = Template::parse("{b} [{a}](red) ({b})", &pal()).unwrap();
        assert_eq!(t.vars(), ["b", "a"]);
    }

    #[test]
    fn conditional_dropped_when_vars_empty() {
        assert_eq!(rend("x(<{a}>)y", &[("a", "")]), "xy");
        assert_eq!(rend("x(<{a}>)y", &[("a", "1")]), "x<1>y");
        assert_eq!(rend("x(<{a}{b}>)y", &[("b", "2")]), "x<2>y");
        // Nested: inner var decides through spans and sections.
        assert_eq!(rend("(a[({b})](red))", &[("b", "")]), "");
        // No variables: never rendered.
        assert_eq!(rend("x(plain)y", &[]), "xy");
    }

    #[test]
    fn unstyled_and_empty_spans_emit_nothing() {
        assert_eq!(rend("[{a}](red)", &[("a", "")]), "");
        assert_eq!(rend("[{a}]()", &[("a", "v")]), "v");
        assert_eq!(rend("", &[]), "");
    }

    #[test]
    fn span_resets_after() {
        assert_eq!(rend("[{a}](bold red)", &[("a", "v")]), "\x1b[1;31mv\x1b[0m");
    }

    #[test]
    fn nesting_inherits_and_reemits_outer() {
        let got = rend("[a[{x}](bg:blue)b](bold red)", &[("x", "X")]);
        assert_eq!(got, "\x1b[1;31ma\x1b[0;1;44;31mX\x1b[0m\x1b[1;31mb\x1b[0m");
        // `none` in the inner span drops the outer style.
        let got = rend("[a[{x}](none)](red)", &[("x", "X")]);
        assert_eq!(got, "\x1b[31ma\x1b[0mX\x1b[0m\x1b[31m\x1b[0m");
    }

    #[test]
    fn powerline_example() {
        let src = "[ {a} ](fg:#000000 bg:#c6a0f6)[\u{e0b0}](fg:#c6a0f6 bg:#89b4fa)\
                   [ {b} ](fg:#000000 bg:#89b4fa)[\u{e0b0}](fg:#89b4fa)";
        let got = rend(src, &[("a", "main"), ("b", "3")]);
        let want = "\x1b[48;2;198;160;246;38;2;0;0;0m main \x1b[0m\
                    \x1b[48;2;137;180;250;38;2;198;160;246m\u{e0b0}\x1b[0m\
                    \x1b[48;2;137;180;250;38;2;0;0;0m 3 \x1b[0m\
                    \x1b[38;2;137;180;250m\u{e0b0}\x1b[0m";
        assert_eq!(got, want);
    }

    fn tmux(src: &str, vals: &[(&str, &str)]) -> String {
        rend_in(Dialect::Tmux, src, vals)
    }

    #[test]
    fn tmux_escape_and_span() {
        assert_eq!(Dialect::Tmux.escape("a#b"), "a##b");
        assert_eq!(Dialect::Ansi.escape("a#b"), "a#b");
        assert_eq!(
            tmux("#[{a}](fg:#5a5a5a bg:#81C784 bold)#", &[("a", "x#y")]),
            "###[fg=#5a5a5a,bg=#81c784,bold]x##y#[default]##"
        );
        assert_eq!(tmux("[{a}]()", &[("a", "v")]), "v");
        assert_eq!(tmux("[{a}](red)", &[("a", "")]), "");
    }

    #[test]
    fn tmux_nesting_reemits_outer() {
        assert_eq!(
            tmux("[a[{x}](bg:blue)b](bold red)", &[("x", "X")]),
            "#[fg=red,bold]a#[default]#[fg=red,bg=blue,bold]X#[default]#[fg=red,bold]b#[default]"
        );
        assert_eq!(
            tmux("[a[{x}](none)](red)", &[("x", "X")]),
            "#[fg=red]a#[default]X#[default]#[fg=red]#[default]"
        );
    }

    #[test]
    fn tmux_colors_and_attrs() {
        let t = |s: &str| {
            let mut st = Style::default();
            st.apply(&parse_style(s, &pal()).unwrap());
            st.tmux()
        };
        assert_eq!(
            t("purple bg:bright-purple"),
            "#[fg=magenta,bg=brightmagenta]"
        );
        assert_eq!(
            t("fg:208 bg:bright-white"),
            "#[fg=colour208,bg=brightwhite]"
        );
        assert_eq!(t("mauve bg:sky"), "#[fg=#c6a0f6,bg=colour117]");
        assert_eq!(
            t("bold dimmed italic underline blink inverted hidden strikethrough"),
            "#[bold,dim,italics,underscore,blink,reverse,hidden,strikethrough]"
        );
        assert_eq!(t("bold none"), "");
    }

    #[test]
    fn tmux_powerline_example() {
        let src = "[ {a} ](fg:#000000 bg:#c6a0f6)[\u{e0b0}](fg:#c6a0f6 bg:#89b4fa)\
                   [ {b} ](fg:#000000 bg:#89b4fa)[\u{e0b0}](fg:#89b4fa)";
        let got = tmux(src, &[("a", "main"), ("b", "3")]);
        let want = "#[fg=#000000,bg=#c6a0f6] main #[default]\
                    #[fg=#c6a0f6,bg=#89b4fa]\u{e0b0}#[default]\
                    #[fg=#000000,bg=#89b4fa] 3 #[default]\
                    #[fg=#89b4fa]\u{e0b0}#[default]";
        assert_eq!(got, want);
    }

    #[test]
    fn more_parse_errors_and_palette_one_level() {
        let e = |s: &str| Template::parse(s, &pal()).unwrap_err();
        assert!(e("a]").contains("unmatched `]`"));
        assert!(e("a}").contains("unmatched `}`"));
        assert!(e("{a b}").contains("invalid variable name `a b`"));
        assert!(e("[a](bold nope)").contains("unknown color `nope`"));
        // Palette values never reference other palette names.
        let chain = Palette::from([("a".into(), "b".into()), ("b".into(), "red".into())]);
        let err = Template::parse("[x](a)", &chain).unwrap_err();
        assert!(
            err.contains("palette color `a`") && err.contains("`b`"),
            "{err}"
        );
    }

    #[test]
    fn escapes_inside_constructs() {
        assert_eq!(rend(r"[\]{a}](red)", &[("a", "v")]), "\x1b[31m]v\x1b[0m");
        assert_eq!(rend(r"(\)\{{a})", &[("a", "v")]), "){v");
        assert_eq!(rend("a\\", &[]), "a\\");
    }

    #[test]
    fn nested_conditionals() {
        let src = "<(a{x}(b{y}))>";
        assert_eq!(rend(src, &[]), "<>");
        assert_eq!(rend(src, &[("x", "1")]), "<a1>");
        assert_eq!(rend(src, &[("y", "2")]), "<ab2>");
        assert_eq!(rend(src, &[("x", "1"), ("y", "2")]), "<a1b2>");
    }

    #[test]
    fn ansi_background_and_none_reset() {
        assert_eq!(
            rend("[{a}](bg:208 fg:#010203)", &[("a", "v")]),
            "\x1b[48;5;208;38;2;1;2;3mv\x1b[0m"
        );
        assert_eq!(rend("[{a}](none)", &[("a", "v")]), "v");
    }
}
