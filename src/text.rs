//! Text rendering for button LCDs, using fonts embedded into the binary plus any the
//! config names in `defaults.fonts`.
//!
//! Text arrives as [`Line`]s of styled spans (see [`crate::markup`]). Each line is cut to
//! [`MAX_LINE_CHARS`] display columns, counted over grapheme clusters so a character is
//! never split and a wide character (an emoji, a CJK ideograph) takes two columns. Only
//! the first [`MAX_LINES`] lines are kept. The text is then scaled as large as fits the
//! button, and every line placed by its alignment.
//!
//! Glyphs are looked up per character, in this order: the configured font for the span's
//! style (regular, bold, italic or bold italic), the embedded DejaVu Sans Mono face of
//! that style, the configured regular font (for bold/italic text, so a script only a
//! configured regular font covers is not lost in bold), the configured emoji font, the
//! embedded monochrome Noto Emoji, the configured extra font (typically CJK), and
//! finally the style font's "missing glyph" box. Glyphs from the emoji and extra fonts
//! are fitted into their display columns, keeping the monospace grid. A glyph counts when the font has an outline for
//! it or a colour bitmap (CBDT/sbix, PNG or BGRA): bitmaps are drawn as scaled pictures in
//! their own colours. COLR and SVG colour glyphs cannot be drawn and are skipped the same
//! way as missing ones, falling back to the next font. Configured fonts are scanned when
//! loaded (see [`scan_font`]) so a font with nothing drawable is refused and one with
//! gaps is reported. There is no text shaping: variation selectors and zero-width joiners
//! are dropped, and an emoji made of several code points (skin tone, ZWJ sequence, flag)
//! shows only its first one.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font, FontArc, FontVec, GlyphId, GlyphImageFormat, PxScale, ScaleFont};
use image::{Rgb, RgbImage, RgbaImage};
use mirajazz::types::ImageFormat;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::color::Color;
use crate::markup::{plain_lines, Align, Line, Style};

/// Maximum number of display columns shown per line on a button: six ordinary
/// characters, or three wide ones such as emoji.
///
/// Matches the config documentation for the `text` and `text_exec` commands.
pub const MAX_LINE_CHARS: usize = 6;

/// Maximum number of lines shown per button.
///
/// Matches the config documentation for the `text` and `text_exec` commands.
pub const MAX_LINES: usize = 3;

/// The keys `defaults.fonts` may contain, in documentation order: the four text styles,
/// the emoji fallback and the last-resort extra font (e.g. CJK). Also the vocabulary
/// `tests/man_pages.rs` requires `dak-config.5` to document.
pub const FONT_KEYS: &[&str] = &["regular", "bold", "italic", "bold_italic", "emoji", "extra"];

/// Largest font file `defaults.fonts` may name; bigger files are refused rather than
/// read into memory whole. Big enough for the largest real fonts (the ~112 MiB Noto
/// Sans CJK "Super OTC" collection), small enough to catch a path to something that is
/// plainly not a font.
pub const MAX_FONT_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// DejaVu Sans Mono (regular, bold, oblique, bold oblique) embedded into the binary, so
/// text rendering works without any font files installed on the host. All four faces
/// share one character width, so mixed styles stay on a monospace grid. Free to
/// redistribute, see `fonts/LICENSE.txt`.
static FONT_BYTES: &[u8] = include_bytes!("../fonts/DejaVuSansMono.ttf");
/// See [`FONT_BYTES`].
static FONT_BOLD_BYTES: &[u8] = include_bytes!("../fonts/DejaVuSansMono-Bold.ttf");
/// See [`FONT_BYTES`].
static FONT_ITALIC_BYTES: &[u8] = include_bytes!("../fonts/DejaVuSansMono-Oblique.ttf");
/// See [`FONT_BYTES`].
static FONT_BOLD_ITALIC_BYTES: &[u8] = include_bytes!("../fonts/DejaVuSansMono-BoldOblique.ttf");
/// Monochrome Noto Emoji (unmodified), the embedded fallback for characters the text
/// fonts lack. SIL Open Font License 1.1, see `fonts/OFL.txt`.
static EMOJI_BYTES: &[u8] = include_bytes!("../fonts/NotoEmoji-VariableFont_wght.ttf");

/// How tall a fitted glyph's em box (emoji, CJK from `extra`) may be, as a share of the
/// text line height. Below 1 so stacked lines of CJK keep a visible gap: at 1.0 three
/// lines of Noto Sans CJK nearly touched on the keypad.
const FITTED_EM_FILL: f32 = 0.93;

/// Characters dropped before drawing because, without text shaping, they have nothing
/// to show: text/emoji variation selectors and the zero-width joiner.
const IGNORED_CHARS: &[char] = &['\u{FE0E}', '\u{FE0F}', '\u{200D}'];

/// The default text colour (white), used by [`render_text`].
fn default_text_color() -> Color {
    Color::rgb(0xff, 0xff, 0xff)
}

/// The default background colour (black), used by [`render_text`] and [`render_error_image`].
fn default_background() -> Color {
    Color::rgb(0x00, 0x00, 0x00)
}

/// The font files named by `defaults.fonts`, already `~`/`$`-expanded. `None` keeps the
/// embedded font for that slot. A path may end in `#N` to pick face `N` (0-based) of a
/// `.ttc`/`.otc` collection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontPaths {
    /// Font for text that is neither bold nor italic.
    pub regular: Option<String>,
    /// Font for bold text.
    pub bold: Option<String>,
    /// Font for italic text.
    pub italic: Option<String>,
    /// Font for bold italic text.
    pub bold_italic: Option<String>,
    /// Fallback font for characters none of the text fonts have (typically emoji).
    pub emoji: Option<String>,
    /// Last-resort font, after the emoji fonts (typically CJK).
    pub extra: Option<String>,
}

impl FontPaths {
    /// The path configured under `key` (one of [`FONT_KEYS`]), for generic handling.
    pub fn slot_mut(&mut self, key: &str) -> Option<&mut Option<String>> {
        match key {
            "regular" => Some(&mut self.regular),
            "bold" => Some(&mut self.bold),
            "italic" => Some(&mut self.italic),
            "bold_italic" => Some(&mut self.bold_italic),
            "emoji" => Some(&mut self.emoji),
            "extra" => Some(&mut self.extra),
            _ => None,
        }
    }
}

/// What characters a configured font cannot draw turn into, as used in the startup
/// warning: the embedded font the slot falls back to, or (for `extra`, the last font of
/// the lookup) the missing-glyph box.
fn fallback_description(key: &str) -> &'static str {
    match key {
        "regular" => "fall back to the embedded DejaVu Sans Mono",
        "bold" => "fall back to the embedded DejaVu Sans Mono Bold",
        "italic" => "fall back to the embedded DejaVu Sans Mono Oblique",
        "bold_italic" => "fall back to the embedded DejaVu Sans Mono Bold Oblique",
        "emoji" => "fall back to the embedded Noto Emoji",
        _ => "are shown as a missing-glyph box",
    }
}

/// Decoded colour bitmaps, keyed by the font's data address and the glyph id; `None`
/// records a bitmap that failed to decode, so it is not retried on every draw.
type BitmapCache = HashMap<(usize, u16), Option<Arc<RgbaImage>>>;

/// A font's em box (the square its characters are designed in) in units of its own line
/// box at scale 1 (`ab_glyph`'s `as_scaled(1.0)`, where ascent - descent = 1).
#[derive(Debug, Clone, Copy, PartialEq)]
struct EmBox {
    /// Height of the em (units per em) relative to the line box: 0.86 for DejaVu Sans
    /// Mono, 0.69 for Noto Sans CJK, whose line box is unusually tall.
    size: f32,
    /// Distance of the em box's vertical centre above the baseline.
    centre: f32,
}

/// Em boxes of the fonts in use, keyed by the font's data address.
type EmBoxCache = HashMap<usize, EmBox>;

/// Computes `font`'s [`EmBox`]: the em is placed by the OS/2 typographic ascender and
/// descender when they span exactly one em, as CJK fonts' do (Noto Sans CJK: 880 above
/// and 120 below the baseline, of 1000); otherwise it is centred in the line box - many
/// fonts (Noto Emoji among them) copy their line box into those fields instead.
fn em_box(font: &FontArc) -> EmBox {
    let line = (font.ascent_unscaled() - font.descent_unscaled()).max(f32::EPSILON);
    let upem = font.units_per_em().unwrap_or(line).max(f32::EPSILON);
    let typo_top = ttf_parser::Face::parse(font.font_data(), 0)
        .ok()
        .and_then(|face| Some((face.typographic_ascender()?, face.typographic_descender()?)))
        .filter(|(top, bottom)| {
            *top > 0 && (f32::from(*top) - f32::from(*bottom) - upem).abs() < 1.0
        })
        .map(|(top, _)| f32::from(top));
    let top = typo_top.unwrap_or(font.ascent_unscaled() - (line - upem) / 2.0);
    EmBox {
        size: upem / line,
        centre: (top - upem / 2.0) / line,
    }
}

/// The fonts text is drawn with: a lookup chain per style plus the emoji fallback chain.
/// Parsed once (at startup for configured fonts) and shared behind an [`Arc`].
#[derive(Clone)]
pub struct FontSet {
    /// Lookup chain per style, indexed by [`style_index`]: the configured font (if any)
    /// first, then the embedded face.
    faces: [Vec<FontArc>; 4],
    /// Fallback chain for characters no face of the style has: the configured emoji
    /// font (if any), then the embedded Noto Emoji.
    emoji: Vec<FontArc>,
    /// The configured regular font, tried for bold/italic text after that style's own
    /// fonts (`None` without one).
    regular: Option<FontArc>,
    /// The configured extra font, tried last (`None` without one).
    extra: Option<FontArc>,
    /// Colour bitmaps decoded so far, shared by every clone of the set.
    bitmaps: Arc<Mutex<BitmapCache>>,
    /// Em boxes computed so far, shared by every clone of the set.
    em_boxes: Arc<Mutex<EmBoxCache>>,
}

/// What loading `defaults.fonts` had to say beyond errors: one-line warnings (always
/// shown) and details such as the full list of undrawable code points (debug output).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontReport {
    /// Warnings shown at startup, prefixed with `defaults.fonts.<slot>`.
    pub warnings: Vec<String>,
    /// Extra detail for `-d scene`, prefixed the same way.
    pub details: Vec<String>,
}

/// The result of scanning a font's character map with [`scan_font`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontScan {
    /// Characters the font maps (not counting those in [`is_blank_by_design`]).
    pub total: usize,
    /// Of those, the code points with neither an outline nor a colour bitmap dak can
    /// decode (COLR/SVG-only colour glyphs, JPEG/TIFF sbix images, empty glyphs).
    pub undrawable: Vec<u32>,
    /// Characters drawn from colour bitmaps (CBDT/sbix) rather than outlines.
    pub bitmaps: usize,
    /// Whether the font has COLR or SVG colour tables, whose colour dak ignores.
    pub vector_colour: bool,
}

impl fmt::Debug for FontSet {
    /// Fonts have no useful textual form; show only how many fonts each chain holds.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontSet")
            .field(
                "faces",
                &self.faces.iter().map(Vec::len).collect::<Vec<_>>(),
            )
            .field("emoji", &self.emoji.len())
            .finish()
    }
}

/// The index of `style`'s face in [`FontSet::faces`]: regular, bold, italic, bold italic.
fn style_index(style: &Style) -> usize {
    usize::from(style.bold) + 2 * usize::from(style.italic)
}

/// Parses one of the embedded fonts; they are known-good, so failure is a build bug.
fn embedded_font(bytes: &'static [u8]) -> FontArc {
    FontArc::try_from_slice(bytes).expect("embedded font is valid")
}

impl FontSet {
    /// The embedded fonts only, parsed on first use and shared afterwards.
    pub fn embedded() -> Arc<FontSet> {
        static EMBEDDED: OnceLock<Arc<FontSet>> = OnceLock::new();
        EMBEDDED
            .get_or_init(|| {
                Arc::new(FontSet {
                    faces: [
                        vec![embedded_font(FONT_BYTES)],
                        vec![embedded_font(FONT_BOLD_BYTES)],
                        vec![embedded_font(FONT_ITALIC_BYTES)],
                        vec![embedded_font(FONT_BOLD_ITALIC_BYTES)],
                    ],
                    emoji: vec![embedded_font(EMOJI_BYTES)],
                    regular: None,
                    extra: None,
                    bitmaps: Arc::default(),
                    em_boxes: Arc::default(),
                })
            })
            .clone()
    }

    /// Builds the font set for `paths`: each configured file is read, scanned and parsed
    /// now and put in front of the embedded font of its slot. Every problem (missing
    /// file, file too large, not a font, collection index out of range, nothing
    /// drawable) is an error, prefixed with `defaults.fonts.<slot>`; characters a font
    /// cannot draw and ignored COLR/SVG colour come back in the [`FontReport`].
    pub fn load(paths: &FontPaths) -> Result<(FontSet, FontReport), Vec<String>> {
        let embedded = FontSet::embedded();
        let mut set = (*embedded).clone();
        set.bitmaps = Arc::default();
        set.em_boxes = Arc::default();
        let mut errors = Vec::new();
        let mut report = FontReport::default();
        let slots = [
            ("regular", &paths.regular),
            ("bold", &paths.bold),
            ("italic", &paths.italic),
            ("bold_italic", &paths.bold_italic),
            ("emoji", &paths.emoji),
            ("extra", &paths.extra),
        ];
        for (index, (key, path)) in slots.into_iter().enumerate() {
            let Some(path) = path else { continue };
            match load_font_file(path) {
                Ok((font, scan)) => {
                    report_scan(key, path, &scan, &mut report);
                    match key {
                        "emoji" => set.emoji.insert(0, font),
                        "extra" => set.extra = Some(font),
                        _ => {
                            if key == "regular" {
                                set.regular = Some(font.clone());
                            }
                            set.faces[index].insert(0, font);
                        }
                    }
                }
                Err(error) => errors.push(format!("defaults.fonts.{key}: {error}")),
            }
        }
        if errors.is_empty() {
            Ok((set, report))
        } else {
            Err(errors)
        }
    }

    /// The font whose metrics lay out every line: the first regular face.
    fn primary(&self) -> &FontArc {
        &self.faces[0][0]
    }

    /// Finds the font and glyph to draw `ch` in `style` with, following the lookup
    /// order in the module documentation, skipping bitmaps that fail to decode.
    fn resolve(&self, ch: char, style: &Style) -> Resolved<'_> {
        let index = style_index(style);
        let chain = &self.faces[index];
        // The configured regular font backs up bold/italic text (for regular text it is
        // already first in `chain`).
        let regular = self.regular.iter().filter(|_| index != 0);
        let candidates = chain
            .iter()
            .chain(regular)
            .map(|font| (font, false))
            .chain(self.emoji.iter().map(|font| (font, true)))
            .chain(self.extra.iter().map(|font| (font, true)));
        for (font, from_emoji) in candidates {
            match drawable_glyph(font, ch) {
                Some((id, false)) => {
                    return Resolved {
                        font,
                        id,
                        fitted: from_emoji,
                        bitmap: None,
                    }
                }
                Some((id, true)) => {
                    if let Some(bitmap) = self.bitmap(font, id) {
                        return Resolved {
                            font,
                            id,
                            fitted: true,
                            bitmap: Some(bitmap),
                        };
                    }
                }
                None => {}
            }
        }
        let font = &chain[0];
        Resolved {
            font,
            id: font.glyph_id(ch),
            fitted: false,
            bitmap: None,
        }
    }

    /// `font`'s em box, from the cache or computed now.
    fn em_box(&self, font: &FontArc) -> EmBox {
        let key = font.font_data().as_ptr() as usize;
        let mut cache = self
            .em_boxes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *cache.entry(key).or_insert_with(|| em_box(font))
    }

    /// The decoded colour bitmap of glyph `id` in `font` (its largest strike), from the
    /// cache or decoded now; `None` when it cannot be decoded.
    fn bitmap(&self, font: &FontArc, id: GlyphId) -> Option<Arc<RgbaImage>> {
        let key = (font.font_data().as_ptr() as usize, id.0);
        let mut cache = self
            .bitmaps
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| decode_bitmap(font, id).map(Arc::new))
            .clone()
    }
}

/// How one character is drawn, as found by [`FontSet::resolve`].
struct Resolved<'a> {
    /// The font the glyph comes from.
    font: &'a FontArc,
    /// The glyph within `font`.
    id: GlyphId,
    /// Whether the glyph is fitted into its cells (emoji chain or a bitmap) rather than
    /// advancing by its own width.
    fitted: bool,
    /// The decoded picture when the glyph is a colour bitmap.
    bitmap: Option<Arc<RgbaImage>>,
}

/// `font`'s glyph for `ch` when it has a usable one: present in the character map and
/// either whitespace (which legitimately has no outline), an outline, or a colour
/// bitmap in a format [`decode_bitmap`] handles. The flag is true for a bitmap. COLR/SVG
/// colour glyphs without an outline qualify for neither, so the next font is tried.
fn drawable_glyph(font: &FontArc, ch: char) -> Option<(GlyphId, bool)> {
    let id = font.glyph_id(ch);
    if id.0 == 0 {
        return None;
    }
    if ch.is_whitespace() || font.outline(id).is_some() {
        return Some((id, false));
    }
    let image = font.glyph_raster_image2(id, u16::MAX)?;
    matches!(
        image.format,
        GlyphImageFormat::Png | GlyphImageFormat::BitmapPremulBgra32
    )
    .then_some((id, true))
}

/// Decodes the largest colour bitmap `font` has for glyph `id` into straight-alpha RGBA:
/// PNG through the `image` crate, premultiplied BGRA by hand. `None` for anything else
/// or data that does not decode.
fn decode_bitmap(font: &FontArc, id: GlyphId) -> Option<RgbaImage> {
    let image = font.glyph_raster_image2(id, u16::MAX)?;
    match image.format {
        GlyphImageFormat::Png => {
            image::load_from_memory_with_format(image.data, image::ImageFormat::Png)
                .ok()
                .map(|decoded| decoded.to_rgba8())
        }
        GlyphImageFormat::BitmapPremulBgra32 => {
            let (width, height) = (u32::from(image.width), u32::from(image.height));
            if image.data.len() < (width * height * 4) as usize {
                return None;
            }
            let mut rgba = RgbaImage::new(width, height);
            for (pixel, bgra) in rgba.pixels_mut().zip(image.data.as_chunks::<4>().0) {
                let alpha = bgra[3];
                // Undo the premultiplication so blending can treat all bitmaps alike.
                let straight = |value: u8| {
                    if alpha == 0 {
                        0
                    } else {
                        ((u32::from(value) * 255 + u32::from(alpha) / 2) / u32::from(alpha))
                            .min(255) as u8
                    }
                };
                *pixel = image::Rgba([
                    straight(bgra[2]),
                    straight(bgra[1]),
                    straight(bgra[0]),
                    alpha,
                ]);
            }
            Some(rgba)
        }
        _ => None,
    }
}

/// Characters a font legitimately draws as nothing, left out of [`scan_font`]'s counts:
/// whitespace, controls, format/invisible characters (joiners, directional marks,
/// variation selectors, tags) and characters that are blank by design (object
/// replacement/interlinear annotation marks, Hangul fillers, the blank Braille cell).
pub fn is_blank_by_design(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || matches!(
            ch as u32,
            0x00AD
                | 0x034F
                | 0x061C
                | 0x115F
                | 0x1160
                | 0x17B4
                | 0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x206F
                | 0x2800
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF9..=0xFFFC
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
        )
}

/// An outline sink that only notes whether anything was drawn; the scan needs to know
/// whether a glyph has an outline, not its shape.
struct NoOutline;

impl ttf_parser::OutlineBuilder for NoOutline {
    /// Ignored.
    fn move_to(&mut self, _: f32, _: f32) {}
    /// Ignored.
    fn line_to(&mut self, _: f32, _: f32) {}
    /// Ignored.
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
    /// Ignored.
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
    /// Ignored.
    fn close(&mut self) {}
}

/// Scans face `index` of the font in `data`: every character its Unicode character maps
/// list (minus [`is_blank_by_design`] ones) is checked for an outline or a decodable
/// colour bitmap, without drawing anything. Takes a few milliseconds for a typical font
/// and ~0.15 s for a 65k-glyph CJK face (see `NOTES.md`). `None` when the data is not a
/// font.
pub fn scan_font(data: &[u8], index: u32) -> Option<FontScan> {
    use ttf_parser::{Face, RasterImageFormat, Tag};
    let face = Face::parse(data, index).ok()?;
    let raw = face.raw_face();
    let mut scan = FontScan {
        vector_colour: raw.table(Tag::from_bytes(b"COLR")).is_some()
            || raw.table(Tag::from_bytes(b"SVG ")).is_some(),
        ..FontScan::default()
    };
    let mut glyphs = std::collections::BTreeMap::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap
            .subtables
            .into_iter()
            .filter(|table| table.is_unicode())
        {
            subtable.codepoints(|code| {
                if let Some(id) = subtable.glyph_index(code) {
                    if id.0 != 0 {
                        glyphs.entry(code).or_insert(id);
                    }
                }
            });
        }
    }
    for (code, id) in glyphs {
        let Some(ch) = char::from_u32(code) else {
            continue;
        };
        if is_blank_by_design(ch) {
            continue;
        }
        scan.total += 1;
        if face.outline_glyph(id, &mut NoOutline).is_some() {
            continue;
        }
        match face.glyph_raster_image(id, u16::MAX) {
            Some(image)
                if matches!(
                    image.format,
                    RasterImageFormat::PNG | RasterImageFormat::BitmapPremulBgra32
                ) =>
            {
                scan.bitmaps += 1
            }
            _ => scan.undrawable.push(code),
        }
    }
    Some(scan)
}

/// Adds the warnings (and debug details) a font's scan calls for to `report`: ignored
/// COLR/SVG colour, and characters that cannot be drawn and so fall back to the slot's
/// embedded font.
fn report_scan(key: &str, path: &str, scan: &FontScan, report: &mut FontReport) {
    if scan.vector_colour {
        report.warnings.push(format!(
            "defaults.fonts.{key}: \"{path}\" has COLR/SVG colour glyphs, which dak cannot draw; its monochrome outlines are used instead"
        ));
    }
    if !scan.undrawable.is_empty() {
        report.warnings.push(format!(
            "defaults.fonts.{key}: {} of {} characters cannot be drawn; they {}",
            scan.undrawable.len(),
            scan.total,
            fallback_description(key)
        ));
        let list: Vec<String> = scan
            .undrawable
            .iter()
            .map(|code| format!("U+{code:04X}"))
            .collect();
        report.details.push(format!(
            "defaults.fonts.{key}: characters that cannot be drawn: {}",
            list.join(" ")
        ));
    }
}

/// Reads, scans and parses one configured font file. A trailing `#N` on `spec` selects
/// face `N` of a font collection; files over [`MAX_FONT_FILE_BYTES`] are refused, and so
/// is a font none of whose characters can be drawn (typically a COLR/SVG-only colour
/// emoji font).
pub fn load_font_file(spec: &str) -> Result<(FontArc, FontScan), String> {
    let (path, index) = split_face_index(spec);
    let metadata = std::fs::metadata(path).map_err(|error| format!("\"{path}\": {error}"))?;
    if !metadata.is_file() {
        return Err(format!("\"{path}\" is not a regular file"));
    }
    if metadata.len() > MAX_FONT_FILE_BYTES {
        return Err(format!(
            "\"{path}\" is larger than {} MiB",
            MAX_FONT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("\"{path}\": {error}"))?;
    let not_a_font = || {
        if index == 0 {
            format!("\"{path}\" is not a TrueType/OpenType font")
        } else {
            format!("\"{path}\" is not a font collection with a face #{index}")
        }
    };
    let scan = scan_font(&bytes, index).ok_or_else(not_a_font)?;
    if scan.total == scan.undrawable.len() {
        return Err(if scan.vector_colour {
            format!("\"{path}\" has no glyphs dak can draw (COLR/SVG-only colour fonts are not supported)")
        } else {
            format!("\"{path}\" has no glyphs dak can draw")
        });
    }
    let font = FontVec::try_from_vec_and_index(bytes, index).map_err(|_| not_a_font())?;
    Ok((FontArc::new(font), scan))
}

/// Splits a font spec into its path and collection face index: `"a.ttc#2"` is face 2 of
/// `a.ttc`, anything without a `#<digits>` suffix is face 0 of the whole path. A file
/// that really exists under the full spec wins, so a name ending in `#1` still works.
fn split_face_index(spec: &str) -> (&str, u32) {
    if let Some((path, index)) = spec.rsplit_once('#') {
        if let Ok(index) = index.parse::<u32>() {
            if !Path::new(spec).exists() {
                return (path, index);
            }
        }
    }
    (spec, 0)
}

/// Extracts up to three lines of up to six display columns from `text`, as shown on a
/// button, without any markup.
///
/// Lines are split on newline characters and each is cut after the last whole grapheme
/// cluster fitting [`MAX_LINE_CHARS`] columns; only the first [`MAX_LINES`] lines are kept.
pub fn button_text(text: &str) -> Vec<String> {
    text.lines()
        .take(MAX_LINES)
        .map(|line| {
            let mut used = 0;
            let mut out = String::new();
            for grapheme in line.graphemes(true) {
                let Some(cells) = grapheme_cells(grapheme) else {
                    out.push_str(grapheme);
                    continue;
                };
                if used + cells > MAX_LINE_CHARS {
                    break;
                }
                used += cells;
                out.push_str(grapheme);
            }
            out
        })
        .collect()
}

/// The number of display columns `grapheme` takes, at least one, or `None` when it
/// consists only of [`IGNORED_CHARS`] and draws nothing.
fn grapheme_cells(grapheme: &str) -> Option<usize> {
    if grapheme.chars().all(|ch| IGNORED_CHARS.contains(&ch)) {
        return None;
    }
    Some(grapheme.width().max(1))
}

/// Renders `lines` into an image of `image_format.size` using the embedded font.
///
/// The font is scaled so that all lines fit within the button while still using as
/// much space as possible, and the text block is centered horizontally and vertically.
/// Returns a white-on-black image; the caller encodes it into the format requested by
/// `image_format`. Use [`render_text_colored`] for configurable colours.
pub fn render_text(
    lines: &[String],
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    render_text_colored(
        lines,
        &default_background(),
        &default_text_color(),
        image_format,
    )
}

/// Renders `lines` in `text_color` onto a `background`-filled button image.
///
/// This is [`render_text`] with the button's configured colours: the canvas is filled
/// with `background` and glyphs are alpha-blended in `text_color`, so anti-aliased edges
/// stay correct on a non-black background.
pub fn render_text_colored(
    lines: &[String],
    background: &Color,
    text_color: &Color,
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    render_lines(
        &plain_lines(lines),
        background,
        text_color,
        &FontSet::embedded(),
        image_format,
    )
}

/// Renders the label "Error" in red, centered, used when an async command fails,
/// times out or is interrupted. Deliberately unaffected by the configured colours and
/// fonts.
pub fn render_error_image(
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    render_text_colored(
        &["Error".to_string()],
        &default_background(),
        &Color::rgb(0xff, 0x00, 0x00),
        image_format,
    )
}

/// One glyph to draw, positioned in units of a 1 px font scale relative to its line.
struct PlacedGlyph {
    /// The font the glyph comes from.
    font: FontArc,
    /// The glyph within `font`.
    id: GlyphId,
    /// Left edge of the glyph's origin from the line start, at scale 1.
    x: f32,
    /// Multiplier on the line's font scale (below 1 for emoji shrunk into their cells).
    scale: f32,
    /// For a glyph vertically centred on the line (emoji, CJK) instead of sitting on
    /// the shared baseline: its em box centre above its baseline, at scale 1 of its own
    /// font (see [`EmBox::centre`]).
    centred: Option<f32>,
    /// The glyph's colour; `None` for the button's text colour. Unused for bitmaps,
    /// which carry their own colours.
    fg: Option<Color>,
    /// For a colour bitmap glyph: its picture and the width (at scale 1) of the cells it
    /// is fitted into, centred on the line.
    bitmap: Option<(Arc<RgbaImage>, f32)>,
}

/// A highlight rectangle behind one grapheme cluster, at scale 1 relative to its line.
struct Highlight {
    /// Left edge from the line start.
    x: f32,
    /// Width.
    width: f32,
    /// Fill colour.
    colour: Color,
}

/// A line cut to fit and broken into positioned glyphs, all at scale 1.
struct LaidOutLine {
    /// Glyphs to draw.
    glyphs: Vec<PlacedGlyph>,
    /// Highlights to paint before the glyphs.
    highlights: Vec<Highlight>,
    /// Total advance of the line.
    width: f32,
    /// Horizontal placement.
    align: Align,
}

/// Cuts `line` to [`MAX_LINE_CHARS`] columns and positions its glyphs at scale 1.
///
/// Characters from a text face advance by their own width (so a proportional configured
/// font keeps its spacing); a character from the emoji chain is fitted into its cells'
/// width (one cell being the primary font's `M` advance) and centred there.
fn lay_out(line: &Line, fonts: &FontSet) -> LaidOutLine {
    let primary = fonts.primary().as_scaled(1.0);
    let cell = primary.h_advance(primary.glyph_id('M'));
    let mut laid = LaidOutLine {
        glyphs: Vec::new(),
        highlights: Vec::new(),
        width: 0.0,
        align: line.align,
    };
    let mut used = 0;
    'spans: for span in &line.spans {
        for grapheme in span.text.graphemes(true) {
            let Some(cells) = grapheme_cells(grapheme) else {
                continue;
            };
            if used + cells > MAX_LINE_CHARS {
                break 'spans;
            }
            used += cells;
            let start = laid.width;
            let mut chars = grapheme.chars().filter(|ch| !IGNORED_CHARS.contains(ch));
            let first = chars.next().expect("grapheme has a drawable character");
            let resolved = fonts.resolve(first, &span.style);
            let (font, id) = (resolved.font, resolved.id);
            if let Some(bitmap) = resolved.bitmap {
                // A colour picture: fitted into its cells when drawn, keeping its aspect.
                let target = cells as f32 * cell;
                laid.glyphs.push(PlacedGlyph {
                    font: font.clone(),
                    id,
                    x: start,
                    scale: 1.0,
                    centred: Some(0.0),
                    fg: None,
                    bitmap: Some((bitmap, target)),
                });
                laid.width += target;
            } else if resolved.fitted {
                // Fit the glyph (emoji, CJK) into its cells: as large as makes its em box
                // [`FITTED_EM_FILL`] of the text line's height, but never wider than its
                // cells. So a font
                // with a tall line box relative to its em (Noto Sans CJK) is enlarged to
                // fill its cells instead of leaving gaps beside each character. The rest
                // of the cluster (skin tone, joined emoji) cannot be combined without
                // shaping and is dropped.
                let target = cells as f32 * cell;
                let natural = font.as_scaled(1.0).h_advance(id).max(f32::EPSILON);
                let em = fonts.em_box(font);
                let scale = (FITTED_EM_FILL / em.size.max(f32::EPSILON)).min(target / natural);
                laid.glyphs.push(PlacedGlyph {
                    font: font.clone(),
                    id,
                    x: start + (target - natural * scale) / 2.0,
                    scale,
                    centred: Some(em.centre),
                    fg: span.style.fg.clone(),
                    bitmap: None,
                });
                laid.width += target;
            } else {
                for (index, ch) in std::iter::once(first).chain(chars).enumerate() {
                    let (font, id) = if index == 0 {
                        (font, id)
                    } else {
                        // A combining mark: only an outline can sit on its base character.
                        let resolved = fonts.resolve(ch, &span.style);
                        if resolved.bitmap.is_some() {
                            continue;
                        }
                        (resolved.font, resolved.id)
                    };
                    laid.glyphs.push(PlacedGlyph {
                        font: font.clone(),
                        id,
                        x: laid.width,
                        scale: 1.0,
                        centred: None,
                        fg: span.style.fg.clone(),
                        bitmap: None,
                    });
                    // Combining marks sit on the preceding base character.
                    if ch.width() != Some(0) {
                        laid.width += font.as_scaled(1.0).h_advance(id);
                    }
                }
            }
            if let Some(colour) = &span.style.bg {
                laid.highlights.push(Highlight {
                    x: start,
                    width: laid.width - start,
                    colour: colour.clone(),
                });
            }
        }
    }
    laid
}

/// Renders styled `lines` onto a `background`-filled button-sized image with `fonts`.
///
/// Lines beyond [`MAX_LINES`] and columns beyond [`MAX_LINE_CHARS`] are dropped. The
/// font is scaled as large as keeps the widest line and all lines inside the button, the
/// block is centred vertically and each line placed by its alignment. Spans without an
/// `fg` are drawn in `text_color`; a span's `bg` highlights the cells behind it. Glyph
/// coverage is blended over whatever is already under it, so anti-aliased edges stay
/// correct on highlights too.
pub fn render_lines(
    lines: &[Line],
    background: &Color,
    text_color: &Color,
    fonts: &FontSet,
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    let (width, height) = (image_format.size.0 as u32, image_format.size.1 as u32);
    if width == 0 || height == 0 {
        return Err("render_text: image format size must be non-zero".into());
    }

    let mut image = RgbImage::from_pixel(width, height, background.to_rgb());
    let laid: Vec<LaidOutLine> = lines
        .iter()
        .take(MAX_LINES)
        .map(|line| lay_out(line, fonts))
        .collect();

    // Reference metrics from a 1px scale; every metric scales linearly, so the font
    // size can be solved from the width and height budgets below.
    let primary = fonts.primary();
    let reference = primary.as_scaled(1.0);
    let cell = reference.h_advance(reference.glyph_id('M'));
    let line_height = reference.ascent() - reference.descent();
    let line_gap = reference.line_gap();

    let line_count = laid.len().max(1) as f32;
    let widest = laid.iter().map(|line| line.width).fold(cell, f32::max);

    // Largest uniform scale that keeps every column and all lines inside the button.
    let scale_x = width as f32 / widest;
    let scale_y = height as f32 / (line_count * line_height + (line_count - 1.0) * line_gap);
    let scale = scale_x.min(scale_y);

    let scaled = primary.as_scaled(scale);
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_height = ascent - descent;
    let gap = scaled.line_gap();
    let total_height = line_count * line_height + (line_count - 1.0) * gap;
    let top = (height as f32 - total_height) / 2.0;

    for (index, line) in laid.iter().enumerate() {
        // Baseline of each line: `index` full lines above it.
        let baseline = top + ascent + index as f32 * (line_height + gap);
        let left = match line.align {
            Align::Left => 0.0,
            Align::Center => (width as f32 - line.width * scale) / 2.0,
            Align::Right => width as f32 - line.width * scale,
        };

        for highlight in &line.highlights {
            let x0 = (left + highlight.x * scale).round().max(0.0) as u32;
            let x1 = ((left + (highlight.x + highlight.width) * scale)
                .round()
                .max(0.0) as u32)
                .min(width);
            let y0 = (baseline - ascent).round().max(0.0) as u32;
            let y1 = ((baseline - descent).round().max(0.0) as u32).min(height);
            for y in y0..y1 {
                for x in x0..x1 {
                    image.put_pixel(x, y, highlight.colour.to_rgb());
                }
            }
        }

        for glyph in &line.glyphs {
            if let Some((picture, cells_width)) = &glyph.bitmap {
                // Fit the picture into its cells and the line height, keeping its aspect,
                // centred in that box.
                let (box_width, box_height) = (cells_width * scale, line_height);
                let (picture_width, picture_height) = (
                    picture.width().max(1) as f32,
                    picture.height().max(1) as f32,
                );
                let fit = (box_width / picture_width).min(box_height / picture_height);
                let (draw_width, draw_height) = (
                    (picture_width * fit).round().max(1.0) as u32,
                    (picture_height * fit).round().max(1.0) as u32,
                );
                let x0 = left + glyph.x * scale + (box_width - draw_width as f32) / 2.0;
                let y0 = baseline - ascent + (box_height - draw_height as f32) / 2.0;
                let scaled_picture = image::imageops::resize(
                    picture.as_ref(),
                    draw_width,
                    draw_height,
                    image::imageops::FilterType::Triangle,
                );
                blend_picture(
                    &mut image,
                    &scaled_picture,
                    x0.round() as i32,
                    y0.round() as i32,
                );
                continue;
            }
            let glyph_scale = scale * glyph.scale;
            let glyph_baseline = match glyph.centred {
                // Put the glyph's em box centre on the primary font's line centre.
                Some(em_centre) => {
                    let centre = baseline - (ascent + descent) / 2.0;
                    centre + em_centre * glyph_scale
                }
                None => baseline,
            };
            let positioned = glyph.id.with_scale_and_position(
                PxScale::from(glyph_scale),
                point(left + glyph.x * scale, glyph_baseline),
            );
            let foreground = glyph.fg.as_ref().unwrap_or(text_color).channels();
            if let Some(outline) = glyph.font.outline_glyph(positioned) {
                // draw() reports glyph-local pixel coordinates, so translate them by
                // the glyph's pixel bounds origin to reach image coordinates.
                let (offset_x, offset_y) = (
                    outline.px_bounds().min.x as i32,
                    outline.px_bounds().min.y as i32,
                );
                outline.draw(|px, py, coverage| {
                    let (px, py) = (px as i32 + offset_x, py as i32 + offset_y);
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        // Blend the glyph's colour over what is already there by its
                        // coverage, so partially covered edge pixels mix rather than
                        // replace (the background, a highlight, or a neighbour's edge).
                        let alpha = coverage.clamp(0.0, 1.0);
                        let backdrop = image.get_pixel(px as u32, py as u32).0;
                        let channel = |index: usize| {
                            (foreground[index] as f32 * alpha
                                + backdrop[index] as f32 * (1.0 - alpha))
                                .round()
                                .clamp(0.0, 255.0) as u8
                        };
                        image.put_pixel(
                            px as u32,
                            py as u32,
                            Rgb([channel(0), channel(1), channel(2)]),
                        );
                    }
                });
            }
        }
    }

    Ok(image::DynamicImage::ImageRgb8(image))
}

/// Alpha-blends `picture` onto `image` with its top-left corner at (`x`, `y`), clipping
/// whatever falls outside the image.
fn blend_picture(image: &mut RgbImage, picture: &RgbaImage, x: i32, y: i32) {
    for (px, py, pixel) in picture.enumerate_pixels() {
        let (tx, ty) = (x + px as i32, y + py as i32);
        if tx < 0 || ty < 0 || tx as u32 >= image.width() || ty as u32 >= image.height() {
            continue;
        }
        let alpha = f32::from(pixel.0[3]) / 255.0;
        let backdrop = image.get_pixel(tx as u32, ty as u32).0;
        let channel = |index: usize| {
            (f32::from(pixel.0[index]) * alpha + f32::from(backdrop[index]) * (1.0 - alpha))
                .round()
                .clamp(0.0, 255.0) as u8
        };
        image.put_pixel(
            tx as u32,
            ty as u32,
            Rgb([channel(0), channel(1), channel(2)]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markup::{parse, Markup};

    /// The button-sized (60x60) image format every rendering test uses.
    fn format() -> ImageFormat {
        ImageFormat {
            mode: mirajazz::types::ImageMode::None,
            size: (60, 60),
            rotation: mirajazz::types::ImageRotation::Rot0,
            mirror: mirajazz::types::ImageMirroring::None,
        }
    }

    /// Renders tmux-markup `text` white on black with the embedded fonts.
    fn render_markup(text: &str) -> RgbImage {
        let parsed = parse(text, Markup::Tmux);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        render_lines(
            &parsed.lines,
            &default_background(),
            &default_text_color(),
            &FontSet::embedded(),
            format(),
        )
        .expect("render")
        .to_rgb8()
    }

    /// Sum of the red channel over the whole image, a proxy for how much "ink" it holds.
    fn ink(image: &RgbImage) -> u64 {
        image.pixels().map(|p| p.0[0] as u64).sum()
    }

    /// Mean x of the lit pixels (red above 100), weighting each pixel equally.
    fn ink_centre_x(image: &RgbImage) -> f32 {
        let lit: Vec<u32> = image
            .enumerate_pixels()
            .filter(|(_, _, p)| p.0[0] > 100)
            .map(|(x, _, _)| x)
            .collect();
        lit.iter().sum::<u32>() as f32 / lit.len().max(1) as f32
    }

    /// The button text extraction keeps only the first three lines and the first six
    /// characters of each.
    #[test]
    fn button_text_limits_lines_and_chars() {
        assert_eq!(
            button_text("abcdefghij\nklmnopqrs\nthird\nfourth"),
            vec!["abcdef", "klmnop", "third"]
        );
    }

    /// A single short line is preserved as-is.
    #[test]
    fn button_text_keeps_short_text() {
        assert_eq!(button_text("Hi"), vec!["Hi"]);
    }

    /// Empty input yields no lines.
    #[test]
    fn button_text_empty_input() {
        assert!(button_text("").is_empty());
    }

    /// Wide characters count as two columns and grapheme clusters are never split.
    #[test]
    fn button_text_counts_display_columns() {
        assert_eq!(
            button_text("\u{1F600}\u{1F600}\u{1F600}\u{1F600}"),
            vec!["\u{1F600}\u{1F600}\u{1F600}"]
        );
        assert_eq!(button_text("ab\u{1F600}cdef"), vec!["ab\u{1F600}cd"]);
        // "e" + combining acute is one cluster of one column.
        assert_eq!(
            button_text("e\u{301}bcdefg"),
            vec!["e\u{301}bcdef".to_string()]
        );
    }

    /// Rendering writes the text into a button-sized image with visible, grayscale pixels.
    #[test]
    fn render_text_draws_glyphs() {
        let image = render_text(&["Hi".to_string()], format())
            .expect("render")
            .to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
        let has_pixels = image.pixels().any(|p| p.0 != [0, 0, 0]);
        assert!(has_pixels, "expected some non-black pixels");
    }

    /// Rendering scales the text so two full-width lines still fit vertically.
    #[test]
    fn render_text_fits_two_lines() {
        let image = render_text(&["abcdef".to_string(), "ghijkl".to_string()], format())
            .expect("render")
            .to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
    }

    /// A zero-sized image format is rejected because no glyphs can be rendered.
    #[test]
    fn render_image_rejects_zero_size() {
        let mut format = format();
        format.size = (0, 0);
        let error = render_text(&["x".to_string()], format)
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-zero"), "unexpected error: {error}");
    }

    /// Returns the smallest and largest y of any pixel with a value above `threshold`.
    fn lit_row_range(image: &image::RgbImage, threshold: u8) -> (Option<u32>, Option<u32>) {
        let rows: Vec<u32> = image
            .enumerate_rows()
            .filter_map(|(y, mut row)| {
                let lit = row.any(|(_, _, p)| p.0[0] > threshold);
                lit.then_some(y)
            })
            .collect();
        (rows.first().copied(), rows.last().copied())
    }

    /// Two rendered lines are vertically centered across the button (the lit band
    /// straddles the middle of the image) instead of being squashed into the top-left.
    #[test]
    fn render_text_centers_two_lines() {
        let image = render_text(&["line1".to_string(), "line2 ".to_string()], format())
            .expect("render")
            .to_rgb8();

        let (min_row, max_row) = lit_row_range(&image, 40);
        let (min_row, max_row) = (
            min_row.expect("expected lit pixels"),
            max_row.expect("expected lit pixels"),
        );

        // Two scaled lines occupy roughly rows 13..45: nothing in the outer 12 rows,
        // and lit pixels on both sides of the vertical center at row 30.
        assert!(min_row >= 12, "text starts too high at row {min_row}");
        assert!(
            min_row < 30,
            "text starts in the lower half at row {min_row}"
        );
        assert!(max_row > 30, "text ends in the upper half at row {max_row}");
    }

    /// Three rendered lines still fit vertically and stay centered, spanning almost the
    /// full button height.
    #[test]
    fn render_text_centers_three_lines() {
        let image = render_text(
            &["one".to_string(), "two".to_string(), "3".to_string()],
            format(),
        )
        .expect("render")
        .to_rgb8();

        let (min_row, max_row) = lit_row_range(&image, 40);
        let (min_row, max_row) = (
            min_row.expect("expected lit pixels"),
            max_row.expect("expected lit pixels"),
        );

        // Three scaled lines fill the whole height (roughly rows 6..58), so the lit
        // band must start near the top, end near the bottom, and enclose the center.
        assert!(min_row <= 8, "text starts too low at row {min_row}");
        assert!(max_row >= 54, "text ends too high at row {max_row}");
        assert!(min_row < 30 && max_row > 30, "text is not centered");
    }

    /// `render_text_colored` fills the canvas with the background and draws glyphs in
    /// the text colour.
    #[test]
    fn render_text_colored_uses_configured_colours() {
        let image = render_text_colored(
            &["M".to_string()],
            &Color::parse("#0000ff").unwrap(),
            &Color::parse("#00ff00").unwrap(),
            format(),
        )
        .expect("render")
        .to_rgb8();
        assert!(
            image.pixels().any(|p| p.0 == [0, 0, 255]),
            "expected plain blue background pixels"
        );
        assert!(
            image
                .pixels()
                .any(|p| p.0[1] > 200 && p.0[0] < 60 && p.0[2] < 60),
            "expected a green glyph pixel"
        );
    }

    /// The error renderer produces a red-on-black label in the requested size.
    #[test]
    fn render_error_image_is_red() {
        let image = render_error_image(format()).expect("render").to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
        assert!(
            image
                .pixels()
                .any(|p| p.0[0] > 100 && p.0[1] == 0 && p.0[2] == 0),
            "expected red pixels"
        );
    }

    /// All four embedded DejaVu faces share one advance, so mixing styles keeps the
    /// monospace grid.
    #[test]
    fn embedded_faces_share_one_advance() {
        let fonts = FontSet::embedded();
        let advances: Vec<f32> = fonts
            .faces
            .iter()
            .map(|chain| {
                let font = chain[0].as_scaled(1.0);
                font.h_advance(font.glyph_id('M'))
            })
            .collect();
        assert!(
            advances.windows(2).all(|pair| pair[0] == pair[1]),
            "{advances:?}"
        );
    }

    /// Bold text carries noticeably more ink than regular text.
    #[test]
    fn bold_is_heavier() {
        let regular = ink(&render_markup("MMM"));
        let bold = ink(&render_markup("#[bold]MMM"));
        assert!(bold > regular * 11 / 10, "bold {bold} vs regular {regular}");
    }

    /// Italic text leans right: its top rows sit further right than its bottom rows,
    /// unlike upright text.
    #[test]
    fn italic_is_slanted() {
        /// Mean lit x of rows `rows` of `image`.
        fn mean_x(image: &RgbImage, rows: std::ops::Range<u32>) -> f32 {
            let xs: Vec<u32> = image
                .enumerate_pixels()
                .filter(|(_, y, p)| rows.contains(y) && p.0[0] > 100)
                .map(|(x, _, _)| x)
                .collect();
            xs.iter().sum::<u32>() as f32 / xs.len().max(1) as f32
        }
        let slant = |image: &RgbImage| {
            let (top, bottom) = lit_row_range(image, 100);
            let (top, bottom) = (top.unwrap(), bottom.unwrap());
            let third = (bottom - top) / 3;
            mean_x(image, top..top + third) - mean_x(image, bottom - third..bottom + 1)
        };
        let upright = slant(&render_markup("l"));
        let italic = slant(&render_markup("#[italics]l"));
        assert!(
            italic > upright + 2.0,
            "italic {italic} vs upright {upright}"
        );
    }

    /// `fg` colours only its span and `bg` paints a highlight behind its cells.
    #[test]
    fn fg_and_bg_colour_spans() {
        let image = render_markup("#[fg=red]A#[fg=default,bg=blue]B");
        assert!(image
            .pixels()
            .any(|p| p.0[0] > 200 && p.0[1] < 40 && p.0[2] < 40));
        assert!(image.pixels().any(|p| p.0 == [0, 0, 255]));
        assert!(image.pixels().any(|p| p.0 == [255, 255, 255]));
        // The highlight only covers the right half (the "B" cell).
        assert!(image
            .enumerate_pixels()
            .all(|(x, _, p)| x >= 28 || p.0[2] < 100));
    }

    /// Left and right alignment move the ink toward that edge.
    #[test]
    fn alignment_moves_ink() {
        let text = "#[align=left]abcdef\n#[align=left]i";
        let left = ink_centre_x(&render_markup(text));
        let right = ink_centre_x(&render_markup(&text.replace("left", "right")));
        let centre = ink_centre_x(&render_markup(&text.replace("left", "centre")));
        assert!(left < centre && centre < right, "{left} {centre} {right}");
    }

    /// An emoji is drawn from the embedded Noto Emoji rather than as DejaVu's
    /// missing-glyph box, and in the span's colour.
    #[test]
    fn emoji_is_drawn_from_noto() {
        let fonts = FontSet::embedded();
        let resolved = fonts.resolve('\u{1F600}', &Style::default());
        assert!(resolved.fitted);
        assert!(resolved.bitmap.is_none());
        assert_ne!(resolved.id.0, 0);
        assert!(!fonts.resolve('A', &Style::default()).fitted);
        let image = render_markup("#[fg=yellow,u=1F600]");
        let lit = image
            .pixels()
            .filter(|p| p.0[0] > 200 && p.0[1] > 200 && p.0[2] < 40)
            .count();
        assert!(lit > 300, "only {lit} yellow pixels");
    }

    /// Variation selectors and joiners draw nothing, and a lone one yields an empty line.
    #[test]
    fn ignored_characters_draw_nothing() {
        let fonts = FontSet::embedded();
        let lines = parse("\u{FE0F}\u{200D}", Markup::None).lines;
        let laid = lay_out(&lines[0], &fonts);
        assert!(laid.glyphs.is_empty());
        let lines = parse("\u{2764}\u{FE0F}", Markup::None).lines;
        assert_eq!(lay_out(&lines[0], &fonts).glyphs.len(), 1);
    }

    /// Lines are cut to six columns across span boundaries, and to three lines.
    #[test]
    fn layout_cuts_lines_and_columns() {
        let fonts = FontSet::embedded();
        let lines = parse("abc#[bold]defgh", Markup::Tmux).lines;
        assert_eq!(lay_out(&lines[0], &fonts).glyphs.len(), 6);
        let lines = parse("a\u{1F600}\u{1F600}\u{1F600}", Markup::Tmux).lines;
        assert_eq!(lay_out(&lines[0], &fonts).glyphs.len(), 3);
        // Four lines render without error, only three are drawn.
        render_markup("a\nb\nc\nd");
    }

    /// Every embedded font passes the startup scan cleanly: all mapped characters are
    /// drawable, except U+1D3D (MODIFIER LETTER CAPITAL OU), which DejaVu Sans Mono
    /// Oblique 2.37 maps to an empty glyph. None carries COLR/SVG colour.
    #[test]
    fn embedded_fonts_scan_clean() {
        for (name, bytes, expected) in [
            ("regular", FONT_BYTES, vec![]),
            ("bold", FONT_BOLD_BYTES, vec![]),
            ("oblique", FONT_ITALIC_BYTES, vec![0x1D3D]),
            ("bold oblique", FONT_BOLD_ITALIC_BYTES, vec![]),
            ("emoji", EMOJI_BYTES, vec![]),
        ] {
            let scan = scan_font(bytes, 0).expect("embedded font parses");
            assert!(scan.total > 1000, "{name}: {}", scan.total);
            assert_eq!(scan.undrawable, expected, "{name}");
            assert!(!scan.vector_colour, "{name}");
            assert_eq!(scan.bitmaps, 0, "{name}");
        }
    }

    /// Blank-by-design characters include whitespace, joiners, variation selectors and
    /// Hangul fillers, but not ordinary letters or emoji.
    #[test]
    fn blank_by_design_characters() {
        for ch in [
            ' ',
            '\u{200D}',
            '\u{FE0F}',
            '\u{3164}',
            '\u{FFFC}',
            '\u{E0041}',
        ] {
            assert!(is_blank_by_design(ch), "{:04X}", ch as u32);
        }
        for ch in ['A', '\u{1F600}', '\u{4E00}'] {
            assert!(!is_blank_by_design(ch), "{:04X}", ch as u32);
        }
    }

    /// `split_face_index` separates a `#N` collection index, but leaves other `#`s alone.
    #[test]
    fn split_face_index_parses_suffix() {
        assert_eq!(split_face_index("/x/a.ttc#2"), ("/x/a.ttc", 2));
        assert_eq!(split_face_index("/x/a.ttf"), ("/x/a.ttf", 0));
        assert_eq!(split_face_index("/x/a#b.ttf"), ("/x/a#b.ttf", 0));
    }
}
