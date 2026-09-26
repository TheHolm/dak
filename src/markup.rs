//! Button text markup: turns the raw text of a `text`, `text_value` or `text_exec` entry
//! into lines of styled spans.
//!
//! Two syntaxes are understood, picked by the entry's (or `defaults`') `markup` key:
//!
//! * [`Markup::None`]: the text is shown as written, every line one unstyled span.
//! * [`Markup::Tmux`] (the default): tmux's status-line style tags. `#[attr,attr ...]`
//!   changes the style of the text that follows it, `##` is a literal `#`, and a `#`
//!   followed by anything else is shown as-is. Attributes (separated by commas and/or
//!   spaces) are `bold`, `nobold`, `italics`/`italic`, `noitalics`/`noitalic`, `none`
//!   (bold and italic off), `default` (everything back to the defaults), `fg=<colour>`,
//!   `bg=<colour>` (a colour or `default`) and `align=left|centre|center|right`. The
//!   non-tmux extension `u=<hex>` inserts the Unicode character with that code point;
//!   further bare hex tokens right after it insert more, so `#[u=1F44D,1F3FD]` and
//!   `#[u=1F44D,u=1F3FD]` are the same.
//!
//! Styles carry over from one line to the next until changed; alignment applies to a
//! whole line, the last `align` in effect at the end of a line deciding it. A tag that
//! does not parse (unknown attribute, invalid colour or code point, missing `]` on the
//! same line) is shown literally and reported through [`Parsed::warnings`], so odd
//! output stays visible instead of silently vanishing.

use crate::color::Color;

/// The `markup` values a setup entry or `defaults` accepts, in documentation order.
/// Also the vocabulary `tests/man_pages.rs` requires `dak-config.5` to document.
pub const MARKUP_VALUES: &[&str] = &["none", "tmux"];

/// Which markup syntax a button's text is parsed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Markup {
    /// No markup: the text is shown exactly as written.
    None,
    /// tmux-style `#[...]` tags (see the module documentation). The default.
    #[default]
    Tmux,
}

impl Markup {
    /// Parses a config `markup` value (one of [`MARKUP_VALUES`]).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "tmux" => Ok(Self::Tmux),
            other => Err(format!(
                "unknown markup \"{other}\", expected \"none\" or \"tmux\""
            )),
        }
    }

    /// The config spelling of this markup, as accepted by [`Markup::parse`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tmux => "tmux",
        }
    }
}

/// Horizontal placement of one line of text on the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Flush with the left edge.
    Left,
    /// Centred (the default, and the only placement before markup existed).
    #[default]
    Center,
    /// Flush with the right edge.
    Right,
}

/// The look of a run of text. `None` colours mean "the button's configured colour"
/// (`text_color` for `fg`, no highlight for `bg`), resolved at draw time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Style {
    /// Drawn with the bold face.
    pub bold: bool,
    /// Drawn with the italic (oblique) face.
    pub italic: bool,
    /// Text colour override.
    pub fg: Option<Color>,
    /// Highlight colour painted behind the span's characters.
    pub bg: Option<Color>,
}

/// A run of text drawn in one [`Style`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The characters of the run (never containing a newline).
    pub text: String,
    /// How they are drawn.
    pub style: Style,
}

/// One line of button text: its spans, left to right, and its placement.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    /// The styled runs making up the line; adjacent spans never share a style.
    pub spans: Vec<Span>,
    /// Where the line is placed horizontally.
    pub align: Align,
}

impl Line {
    /// The line's visible text with all styling dropped.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// Appends `text` in `style`, merging it into the last span when that has the same
    /// style so a line never carries needless splits.
    fn push(&mut self, text: &str, style: &Style) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.style == *style => last.text.push_str(text),
            _ => self.spans.push(Span {
                text: text.to_string(),
                style: style.clone(),
            }),
        }
    }
}

/// The result of [`parse`]: every line of the text (not yet cut down to what fits a
/// button) plus a message for each malformed tag that was shown literally instead.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The lines of the text, in order.
    pub lines: Vec<Line>,
    /// One human-readable message per tag that could not be parsed.
    pub warnings: Vec<String>,
}

/// Wraps plain text lines (no markup at all) as unstyled, centred [`Line`]s.
pub fn plain_lines(lines: &[String]) -> Vec<Line> {
    lines
        .iter()
        .map(|text| {
            let mut line = Line::default();
            line.push(text, &Style::default());
            line
        })
        .collect()
}

/// Parses `text` with the given `markup` into lines of styled spans.
///
/// Lines are split like [`str::lines`] (`\n` or `\r\n`, no trailing empty line). With
/// [`Markup::None`] every line is a single unstyled span; with [`Markup::Tmux`] the tags
/// described in the module documentation are applied.
pub fn parse(text: &str, markup: Markup) -> Parsed {
    match markup {
        Markup::None => Parsed {
            lines: plain_lines(&text.lines().map(str::to_string).collect::<Vec<_>>()),
            warnings: Vec::new(),
        },
        Markup::Tmux => parse_tmux(text),
    }
}

/// One validated effect of a tag attribute, applied in order once the whole tag parsed.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    /// Turn bold on or off.
    Bold(bool),
    /// Turn italic on or off.
    Italic(bool),
    /// Set (or, with `None`, reset) the text colour.
    Fg(Option<Color>),
    /// Set (or, with `None`, reset) the highlight colour.
    Bg(Option<Color>),
    /// Set the line alignment.
    Align(Align),
    /// Reset bold, italic, both colours and the alignment.
    Default,
    /// Insert this character.
    Char(char),
}

/// Parses tmux-style markup; see [`parse`].
fn parse_tmux(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut style = Style::default();
    let mut align = Align::default();

    for raw_line in text.lines() {
        let mut line = Line::default();
        let mut rest = raw_line;
        while let Some(hash) = rest.find('#') {
            line.push(&rest[..hash], &style);
            let after = &rest[hash + 1..];
            if let Some(tail) = after.strip_prefix('#') {
                line.push("#", &style);
                rest = tail;
            } else if let Some(body_and_tail) = after.strip_prefix('[') {
                match body_and_tail.find(']') {
                    Some(close) => {
                        let body = &body_and_tail[..close];
                        match parse_tag(body) {
                            Ok(ops) => {
                                for op in ops {
                                    apply(op, &mut style, &mut align, &mut line);
                                }
                            }
                            Err(error) => {
                                parsed
                                    .warnings
                                    .push(format!("markup tag \"#[{body}]\" {error}"));
                                line.push(&format!("#[{body}]"), &style);
                            }
                        }
                        rest = &body_and_tail[close + 1..];
                    }
                    None => {
                        parsed.warnings.push(format!(
                            "markup tag \"#[{body_and_tail}\" is missing its closing \"]\""
                        ));
                        line.push(&format!("#[{body_and_tail}"), &style);
                        rest = "";
                    }
                }
            } else {
                line.push("#", &style);
                rest = after;
            }
        }
        line.push(rest, &style);
        line.align = align;
        parsed.lines.push(line);
    }
    parsed
}

/// Applies one tag effect to the running style/alignment, inserting characters into
/// `line` with the style in effect at that point of the tag.
fn apply(op: Op, style: &mut Style, align: &mut Align, line: &mut Line) {
    match op {
        Op::Bold(on) => style.bold = on,
        Op::Italic(on) => style.italic = on,
        Op::Fg(colour) => style.fg = colour,
        Op::Bg(colour) => style.bg = colour,
        Op::Align(value) => *align = value,
        Op::Default => {
            *style = Style::default();
            *align = Align::default();
        }
        Op::Char(ch) => line.push(ch.encode_utf8(&mut [0; 4]), style),
    }
}

/// Parses the attributes between `#[` and `]` into their effects, failing on the first
/// one that is not understood so the caller can show the whole tag literally.
fn parse_tag(body: &str) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    // Whether the previous token was `u=...` or a bare hex continuing it, so another
    // bare hex token adds one more character to that list.
    let mut in_code_points = false;
    for token in body
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|token| !token.is_empty())
    {
        if let Some(hex) = token.strip_prefix("u=") {
            ops.push(Op::Char(code_point(hex)?));
            in_code_points = true;
            continue;
        }
        if in_code_points && is_hex(token) {
            ops.push(Op::Char(code_point(token)?));
            continue;
        }
        in_code_points = false;
        let op = match token {
            "bold" => Op::Bold(true),
            "nobold" => Op::Bold(false),
            "italics" | "italic" => Op::Italic(true),
            "noitalics" | "noitalic" => Op::Italic(false),
            "none" => {
                ops.push(Op::Bold(false));
                Op::Italic(false)
            }
            "default" => Op::Default,
            _ => match token.split_once('=') {
                Some(("fg", value)) => Op::Fg(colour(value)?),
                Some(("bg", value)) => Op::Bg(colour(value)?),
                Some(("align", value)) => Op::Align(match value {
                    "left" => Align::Left,
                    "centre" | "center" => Align::Center,
                    "right" => Align::Right,
                    other => {
                        return Err(format!(
                        "has unknown alignment \"{other}\", expected left, centre, center or right"
                    ))
                    }
                }),
                _ if is_hex(token) => {
                    return Err(format!(
                        "has a bare code point \"{token}\" not preceded by \"u=\""
                    ))
                }
                _ => return Err(format!("has unknown attribute \"{token}\"")),
            },
        };
        ops.push(op);
    }
    Ok(ops)
}

/// Whether `token` consists of hex digits only (and is not empty).
fn is_hex(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Parses a `u=` code point: 1-6 hex digits naming a Unicode scalar value (surrogates
/// and values above U+10FFFF are rejected).
fn code_point(hex: &str) -> Result<char, String> {
    if !is_hex(hex) || hex.len() > 6 {
        return Err(format!(
            "has invalid code point \"{hex}\", expected 1 to 6 hex digits"
        ));
    }
    let value = u32::from_str_radix(hex, 16).expect("validated hex digits");
    char::from_u32(value).ok_or_else(|| format!("has invalid code point U+{value:04X}"))
}

/// Parses an `fg=`/`bg=` value: `default` (reset) or a colour [`Color::parse`] accepts.
fn colour(value: &str) -> Result<Option<Color>, String> {
    if value == "default" {
        return Ok(None);
    }
    Color::parse(value)
        .map(Some)
        .map_err(|error| format!("has invalid colour: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `text` as tmux markup and returns its lines, asserting no warnings.
    fn tmux(text: &str) -> Vec<Line> {
        let parsed = parse(text, Markup::Tmux);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        parsed.lines
    }

    /// Shorthand for a style with the given flags and no colours.
    fn flags(bold: bool, italic: bool) -> Style {
        Style {
            bold,
            italic,
            ..Style::default()
        }
    }

    /// `Markup::parse` accepts exactly the documented values and round-trips them.
    #[test]
    fn markup_parse_round_trips() {
        for value in MARKUP_VALUES {
            assert_eq!(Markup::parse(value).unwrap().as_str(), *value);
        }
        assert!(Markup::parse("bbcode").unwrap_err().contains("bbcode"));
        assert_eq!(Markup::default(), Markup::Tmux);
    }

    /// With no markup, tags are ordinary characters and every line is one span.
    #[test]
    fn none_keeps_text_verbatim() {
        let parsed = parse("#[bold]a##\nb", Markup::None);
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].text(), "#[bold]a##");
        assert_eq!(parsed.lines[0].spans[0].style, Style::default());
        assert_eq!(parsed.lines[1].text(), "b");
    }

    /// Plain text without any `#` parses to one unstyled, centred span per line.
    #[test]
    fn tmux_plain_text() {
        let lines = tmux("CPU\n42%");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].text(), "CPU");
        assert_eq!(lines[0].align, Align::Center);
    }

    /// Bold and italic switch on and off, splitting the line into styled spans.
    #[test]
    fn tmux_bold_and_italic() {
        let lines = tmux("a#[bold]b#[italics]c#[nobold]d#[noitalics]e");
        let styles: Vec<_> = lines[0]
            .spans
            .iter()
            .map(|span| (span.text.as_str(), span.style.clone()))
            .collect();
        assert_eq!(
            styles,
            vec![
                ("a", flags(false, false)),
                ("b", flags(true, false)),
                ("c", flags(true, true)),
                ("d", flags(false, true)),
                ("e", flags(false, false)),
            ]
        );
    }

    /// `italic`/`noitalic` are accepted as aliases, and `none` turns both flags off.
    #[test]
    fn tmux_aliases_and_none() {
        let lines = tmux("#[bold,italic]x#[none]y#[italic]z#[noitalic]w");
        assert_eq!(lines[0].spans[0].style, flags(true, true));
        assert_eq!(lines[0].spans[1].style, flags(false, false));
        assert_eq!(lines[0].spans[2].style, flags(false, true));
        assert_eq!(lines[0].spans[3].style, flags(false, false));
    }

    /// `fg=`/`bg=` take colour names or hex, and `default` resets them.
    #[test]
    fn tmux_colours() {
        let lines = tmux("#[fg=red,bg=#0000ff]a#[fg=default]b#[bg=default]c");
        let spans = &lines[0].spans;
        assert_eq!(spans[0].style.fg.as_ref().unwrap().channels(), [255, 0, 0]);
        assert_eq!(spans[0].style.bg.as_ref().unwrap().channels(), [0, 0, 255]);
        assert!(spans[1].style.fg.is_none());
        assert!(spans[1].style.bg.is_some());
        assert_eq!(spans[2].style, Style::default());
    }

    /// Attributes may be separated by spaces as well as commas, as in tmux.
    #[test]
    fn tmux_space_separator() {
        let lines = tmux("#[fg=red bold]x");
        assert!(lines[0].spans[0].style.bold);
        assert!(lines[0].spans[0].style.fg.is_some());
    }

    /// Alignment is per line, the last `align` on a line wins and it carries over.
    #[test]
    fn tmux_alignment() {
        let lines = tmux("#[align=left]a#[align=right]b\nc\n#[align=centre]d\n#[align=center]e");
        assert_eq!(lines[0].align, Align::Right);
        assert_eq!(lines[1].align, Align::Right);
        assert_eq!(lines[2].align, Align::Center);
        assert_eq!(lines[3].align, Align::Center);
    }

    /// Styles carry over to the following lines until changed.
    #[test]
    fn tmux_style_carries_across_lines() {
        let lines = tmux("#[bold,fg=green]a\nb\n#[default]c");
        assert!(lines[1].spans[0].style.bold);
        assert!(lines[1].spans[0].style.fg.is_some());
        assert_eq!(lines[2].spans[0].style, Style::default());
    }

    /// `default` also resets the alignment.
    #[test]
    fn tmux_default_resets_alignment() {
        let lines = tmux("#[align=left]a\n#[default]b");
        assert_eq!(lines[0].align, Align::Left);
        assert_eq!(lines[1].align, Align::Center);
    }

    /// `##` is a literal `#`, and a lone `#` not starting a tag stays as-is.
    #[test]
    fn tmux_hash_escapes() {
        assert_eq!(tmux("a##b")[0].text(), "a#b");
        assert_eq!(tmux("#1 #")[0].text(), "#1 #");
        assert_eq!(tmux("###[bold]x")[0].text(), "#x");
        assert!(tmux("###[bold]x")[0].spans[1].style.bold);
    }

    /// `u=` inserts a code point, in repeated or comma-continued form, in the style in
    /// effect at that point of the tag.
    #[test]
    fn tmux_code_points() {
        assert_eq!(tmux("#[u=1F600]")[0].text(), "\u{1F600}");
        assert_eq!(tmux("#[u=1F44D,u=1F3FD]")[0].text(), "\u{1F44D}\u{1F3FD}");
        assert_eq!(tmux("#[u=1F44D,1F3FD]")[0].text(), "\u{1F44D}\u{1F3FD}");
        assert_eq!(tmux("#[u=1f44d 1f3fd]")[0].text(), "\u{1F44D}\u{1F3FD}");
        let lines = tmux("#[bold,u=41,42,fg=red,u=43]");
        assert_eq!(lines[0].spans[0].text, "AB");
        assert!(lines[0].spans[0].style.bold);
        assert_eq!(lines[0].spans[1].text, "C");
        assert!(lines[0].spans[1].style.fg.is_some());
    }

    /// Returns the single warning produced by parsing `text` and the literal first line.
    fn malformed(text: &str) -> (String, String) {
        let parsed = parse(text, Markup::Tmux);
        assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
        (parsed.warnings[0].clone(), parsed.lines[0].text())
    }

    /// Unknown attributes, bad colours and bad alignments leave the tag literal.
    #[test]
    fn tmux_malformed_tags_are_literal() {
        let (warning, text) = malformed("a#[blink]b");
        assert!(warning.contains("unknown attribute \"blink\""), "{warning}");
        assert_eq!(text, "a#[blink]b");
        let (warning, _) = malformed("#[fg=notacolour]x");
        assert!(warning.contains("invalid colour"), "{warning}");
        let (warning, _) = malformed("#[align=top]x");
        assert!(warning.contains("alignment"), "{warning}");
    }

    /// Invalid code points (too long, surrogate, above U+10FFFF, not hex) are rejected.
    #[test]
    fn tmux_invalid_code_points() {
        for tag in [
            "#[u=1234567]",
            "#[u=D800]",
            "#[u=110000]",
            "#[u=xyz]",
            "#[u=]",
        ] {
            let (warning, text) = malformed(tag);
            assert!(warning.contains("code point"), "{tag}: {warning}");
            assert_eq!(text, tag);
        }
    }

    /// A bare hex token without a preceding `u=` is an error, and a non-hex attribute
    /// ends the code point list.
    #[test]
    fn tmux_bare_hex_needs_u() {
        let (warning, _) = malformed("#[bold,1F600]");
        assert!(warning.contains("not preceded"), "{warning}");
        let (warning, _) = malformed("#[u=41,bold,42]");
        assert!(warning.contains("not preceded"), "{warning}");
    }

    /// A tag without its closing `]` on the same line is shown literally.
    #[test]
    fn tmux_unterminated_tag() {
        let parsed = parse("a#[bold\nb]", Markup::Tmux);
        assert_eq!(parsed.warnings.len(), 1);
        assert!(parsed.warnings[0].contains("closing"));
        assert_eq!(parsed.lines[0].text(), "a#[bold");
        assert_eq!(parsed.lines[1].text(), "b]");
        assert!(!parsed.lines[1].spans[0].style.bold);
    }

    /// Empty tags are accepted and change nothing; empty lines yield empty lines.
    #[test]
    fn tmux_empty_tag_and_lines() {
        let lines = tmux("#[]a\n\nb");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text(), "a");
        assert!(lines[1].spans.is_empty());
    }

    /// `plain_lines` wraps each string as one unstyled span.
    #[test]
    fn plain_lines_wraps_strings() {
        let lines = plain_lines(&["x".to_string(), String::new()]);
        assert_eq!(lines[0].text(), "x");
        assert!(lines[1].spans.is_empty());
    }
}
