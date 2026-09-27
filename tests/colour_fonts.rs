//! Tests for colour-bitmap emoji and the startup scan of configured fonts, using tiny
//! fonts built in memory (`tests/common/font_builder.rs`) instead of real font files:
//! a CBDT bitmap font is drawn in its own colours, a font with nothing drawable is
//! refused, and fonts with gaps or ignored COLR/SVG colour produce the documented
//! warnings.

mod common;

use common::font_builder::{build_font, solid_png, write_font, ColourTables, Glyph};
use dak::actions::load_config_from_path;
use dak::color::Color;
use dak::markup::{parse, Markup};
use dak::text::{render_lines, scan_font, FontPaths, FontSet, Undrawable, MAX_FONT_FILE_BYTES};
use mirajazz::types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation};

// See src/lib.rs for why this is needed on FreeBSD only.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

use crate::common::{error_texts, write_config_with_defaults};

/// The button-sized image format the rendering tests use.
const FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::None,
    size: (60, 60),
    rotation: ImageRotation::Rot0,
    mirror: ImageMirroring::None,
};

/// A font with one red 32 px CBDT bitmap for U+1F600, as colour emoji fonts have.
fn bitmap_font() -> Vec<u8> {
    build_font(
        &[('\u{1F600}', Glyph::Png(solid_png(32, [255, 0, 0, 255]), 32))],
        ColourTables::default(),
    )
}

/// Loads `defaults.fonts.emoji` = a font file with `bytes`, returning the load result.
fn load_emoji_font(
    name: &str,
    bytes: &[u8],
) -> (
    std::path::PathBuf,
    Result<dak::actions::LoadedConfig, Vec<String>>,
) {
    let font = write_font(name, bytes);
    let path = write_config_with_defaults(
        &format!(r#"{{"fonts": {{"emoji": "{}"}}}}"#, font.display()),
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    (font, config)
}

/// Renders tmux-markup `text` white on black with `fonts`.
fn render(text: &str, fonts: &FontSet) -> image::RgbImage {
    render_lines(
        &parse(text, Markup::Tmux).lines,
        &Color::rgb(0, 0, 0),
        &Color::rgb(255, 255, 255),
        fonts,
        FORMAT,
    )
    .unwrap()
    .to_rgb8()
}

/// The scan sees a bitmap font's glyph as a drawable bitmap and no colour tables.
#[test]
fn scan_counts_bitmap_glyphs() {
    let scan = scan_font(&bitmap_font(), 0).unwrap();
    assert_eq!(scan.total, 1);
    assert_eq!(scan.bitmaps, 1);
    assert!(scan.undrawable.is_empty());
    assert!(!scan.vector_colour);
}

/// The scan skips blank-by-design characters (a space, a Hangul filler) and lists the
/// empty glyphs of real characters as undrawable.
#[test]
fn scan_skips_blank_characters() {
    let font = build_font(
        &[
            ('A', Glyph::Square),
            ('B', Glyph::Empty),
            (' ', Glyph::Empty),
            ('\u{3164}', Glyph::Empty),
        ],
        ColourTables::default(),
    );
    let scan = scan_font(&font, 0).unwrap();
    assert_eq!(scan.total, 2);
    assert_eq!(scan.outlines, 1);
    assert_eq!(scan.undrawable, vec![('B' as u32, Undrawable::Empty)]);
}

/// Garbage is not a font.
#[test]
fn scan_rejects_garbage() {
    assert!(scan_font(b"not a font", 0).is_none());
}

/// A CBDT colour emoji is drawn in its own colour (red), ignoring the white text
/// colour and even an explicit `fg`, and fitted inside the button.
#[test]
fn bitmap_emoji_drawn_in_own_colours() {
    let path = write_font("bitmap.ttf", &bitmap_font());
    let (fonts, report) = FontSet::load(&FontPaths {
        emoji: Some(path.to_str().unwrap().to_string()),
        ..Default::default()
    })
    .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let image = render("#[fg=blue,u=1F600]", &fonts);
    let red = image.pixels().filter(|p| p.0 == [255, 0, 0]).count();
    assert!(red > 400, "only {red} red pixels");
    assert!(
        !image.pixels().any(|p| p.0[2] > 100),
        "fg must not tint bitmaps"
    );
    // A second draw (from the decoded-bitmap cache) is identical.
    assert_eq!(render("#[fg=blue,u=1F600]", &fonts), image);
}

/// A square bitmap in a 2-column slot is fitted by height: it is not wider than tall,
/// and stays inside the button.
#[test]
fn bitmap_emoji_keeps_its_aspect() {
    let path = write_font("aspect.ttf", &bitmap_font());
    let (fonts, _) = FontSet::load(&FontPaths {
        emoji: Some(path.to_str().unwrap().to_string()),
        ..Default::default()
    })
    .unwrap();
    let image = render("#[u=1F600]", &fonts);
    let red: Vec<(u32, u32)> = image
        .enumerate_pixels()
        .filter(|(_, _, p)| p.0[0] > 200)
        .map(|(x, y, _)| (x, y))
        .collect();
    let width = red.iter().map(|p| p.0).max().unwrap() - red.iter().map(|p| p.0).min().unwrap();
    let height = red.iter().map(|p| p.1).max().unwrap() - red.iter().map(|p| p.1).min().unwrap();
    assert!(width.abs_diff(height) <= 2, "{width}x{height}");
}

/// A bitmap emoji is blended over a `bg` highlight: the highlight shows around it.
#[test]
fn bitmap_emoji_over_highlight() {
    let half = build_font(
        &[('\u{1F600}', Glyph::Png(solid_png(32, [255, 0, 0, 128]), 32))],
        ColourTables::default(),
    );
    let path = write_font("half.ttf", &half);
    let (fonts, _) = FontSet::load(&FontPaths {
        emoji: Some(path.to_str().unwrap().to_string()),
        ..Default::default()
    })
    .unwrap();
    let image = render("#[bg=blue,u=1F600]", &fonts);
    // Half-transparent red over blue mixes both channels.
    assert!(image
        .pixels()
        .any(|p| p.0[0] > 100 && p.0[2] > 100 && p.0[1] < 20));
}

/// Loading a bitmap emoji font through the config works and warns about nothing.
#[test]
fn config_accepts_bitmap_font_quietly() {
    let (font, config) = load_emoji_font("colour.ttf", &bitmap_font());
    let config = config.unwrap();
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    // The -d fonts listing names the font but has nothing undrawable to show.
    assert!(config
        .font_details
        .iter()
        .any(|line| line == "    1 characters: 0 outline, 1 colour bitmap, 0 undrawable"));
    assert!(!config
        .font_details
        .iter()
        .any(|line| line.contains("undrawable,")));
    let _ = std::fs::remove_file(font);
}

/// A COLR-only font (colour glyphs without outlines) has nothing dak can draw: a config
/// error naming COLR/SVG.
#[test]
fn config_rejects_colr_only_font() {
    let font = build_font(
        &[('\u{1F600}', Glyph::Empty)],
        ColourTables {
            colr: true,
            svg: false,
        },
    );
    let (_, config) = load_emoji_font("colr.ttf", &font);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors
            .contains("has no glyphs dak can draw (COLR/SVG-only colour fonts are not supported)"),
        "{errors}"
    );
}

/// An SVG-only font is refused the same way.
#[test]
fn config_rejects_svg_only_font() {
    let font = build_font(
        &[('\u{1F600}', Glyph::Empty)],
        ColourTables {
            colr: false,
            svg: true,
        },
    );
    let (_, config) = load_emoji_font("svg.ttf", &font);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("COLR/SVG-only"), "{errors}");
}

/// A font with no drawable glyph and no colour tables is refused with the plain message.
#[test]
fn config_rejects_empty_font() {
    let font = build_font(&[('A', Glyph::Empty)], ColourTables::default());
    let (_, config) = load_emoji_font("empty.ttf", &font);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("has no glyphs dak can draw"), "{errors}");
    assert!(!errors.contains("COLR"), "{errors}");
}

/// A COLR/SVG font that also has outlines loads, with one warning that its colour is
/// ignored.
#[test]
fn config_warns_about_ignored_vector_colour() {
    let font = build_font(
        &[('\u{1F600}', Glyph::Square)],
        ColourTables {
            colr: true,
            svg: true,
        },
    );
    let (_, config) = load_emoji_font("both.ttf", &font);
    let warnings = config.unwrap().warnings.join("\n");
    assert!(
        warnings.contains("has COLR/SVG colour glyphs, which dak cannot draw; its monochrome outlines are used instead"),
        "{warnings}"
    );
}

/// A font with some undrawable characters loads with a count-only warning naming the
/// embedded font of the slot they fall back to; the code points go to the details.
#[test]
fn config_warns_about_partly_drawable_font() {
    let font = build_font(
        &[
            ('A', Glyph::Square),
            ('B', Glyph::Empty),
            ('C', Glyph::Empty),
        ],
        ColourTables::default(),
    );
    let path = write_font("partial.ttf", &font);
    for (key, fallback) in [
        ("regular", "DejaVu Sans Mono"),
        ("bold", "DejaVu Sans Mono Bold"),
        ("italic", "DejaVu Sans Mono Oblique"),
        ("bold_italic", "DejaVu Sans Mono Bold Oblique"),
        ("emoji", "Noto Emoji"),
        ("extra", ""),
    ] {
        let config_path = write_config_with_defaults(
            &format!(r#"{{"fonts": {{"{key}": "{}"}}}}"#, path.display()),
            r#"{"on_start": {"actions": {}}}"#,
        );
        let config = load_config_from_path(config_path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&config_path);
        let outcome = if fallback.is_empty() {
            "are shown as a missing-glyph box".to_string()
        } else {
            format!("fall back to the embedded {fallback}")
        };
        assert_eq!(
            config.warnings,
            vec![format!(
                "defaults.fonts.{key}: 2 of 3 characters cannot be drawn; they {outcome}"
            )]
        );
        assert!(
            config
                .font_details
                .iter()
                .any(|line| line == "    undrawable, empty glyph: U+0042-0043"),
            "{:?}",
            config.font_details
        );
    }
}

/// Characters a configured font cannot draw really are drawn from the embedded font:
/// the empty "B" still produces ink.
#[test]
fn undrawable_characters_fall_back_when_rendering() {
    let font = build_font(
        &[('A', Glyph::Square), ('B', Glyph::Empty)],
        ColourTables::default(),
    );
    let path = write_font("fallback.ttf", &font);
    let (fonts, _) = FontSet::load(&FontPaths {
        regular: Some(path.to_str().unwrap().to_string()),
        ..Default::default()
    })
    .unwrap();
    let image = render("B", &fonts);
    assert!(image.pixels().any(|p| p.0[0] > 200));
}

/// The size limit is 256 MiB, large enough for the biggest real CJK collections.
#[test]
fn font_size_limit_is_256_mib() {
    assert_eq!(MAX_FONT_FILE_BYTES, 256 * 1024 * 1024);
}

/// A file just over the limit is refused before being read (a sparse file, so the test
/// does not actually write 256 MiB).
#[test]
fn oversized_font_is_refused() {
    let path = common::temp_dir().join("huge.ttf");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_FONT_FILE_BYTES + 1).unwrap();
    let (_, config) = {
        let config_path = write_config_with_defaults(
            &format!(r#"{{"fonts": {{"emoji": "{}"}}}}"#, path.display()),
            r#"{"on_start": {"actions": {}}}"#,
        );
        let config = load_config_from_path(config_path.to_str().unwrap());
        let _ = std::fs::remove_file(&config_path);
        ((), config)
    };
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("is larger than 256 MiB"), "{errors}");
    let _ = std::fs::remove_file(path);
}

/// The scan names the reason a glyph cannot be drawn: COLR layers, an SVG picture, or
/// nothing at all.
#[test]
fn scan_reports_reasons() {
    let glyphs = [('A', Glyph::Square), ('B', Glyph::Empty)];
    for (colour, reason) in [
        (
            ColourTables {
                colr: true,
                svg: false,
            },
            Undrawable::Colr,
        ),
        (
            ColourTables {
                colr: false,
                svg: true,
            },
            Undrawable::Svg,
        ),
        (ColourTables::default(), Undrawable::Empty),
    ] {
        let scan = scan_font(&build_font(&glyphs, colour), 0).unwrap();
        assert_eq!(scan.undrawable, vec![('B' as u32, reason)]);
    }
}

/// `-d fonts` lists every font in lookup order - configured ones with path, counts and
/// labelled undrawable ranges, embedded ones marked as built in - numbered from 1.
#[test]
fn debug_listing_shows_lookup_order_and_reasons() {
    let emoji = build_font(
        &[
            ('A', Glyph::Square),
            ('\u{1F600}', Glyph::Empty),
            ('\u{1F601}', Glyph::Empty),
        ],
        ColourTables {
            colr: true,
            svg: false,
        },
    );
    let (font, config) = load_emoji_font("listing.ttf", &emoji);
    let config = config.unwrap();
    let path = font.display();
    assert_eq!(
        config.font_details,
        vec![
            "lookup order (a character is drawn from the first font that has it; each style tries its own fonts, then for bold/italic the configured regular font, then the emoji fonts, then extra):".to_string(),
            "1 regular     embedded DejaVu Sans Mono (built in, not scanned)".to_string(),
            "2 bold        embedded DejaVu Sans Mono Bold (built in, not scanned)".to_string(),
            "3 italic      embedded DejaVu Sans Mono Oblique (built in, not scanned)".to_string(),
            "4 bold_italic embedded DejaVu Sans Mono Bold Oblique (built in, not scanned)".to_string(),
            format!("5 emoji       \"{path}\""),
            "    3 characters: 1 outline, 0 colour bitmap, 2 undrawable (drawn from embedded Noto Emoji instead)".to_string(),
            "    undrawable, COLR colour layers: U+1F600-1F601".to_string(),
            "6 emoji       embedded Noto Emoji (built in, not scanned)".to_string(),
        ]
    );
}

/// Without configured fonts there is nothing to list: `-d fonts` stays silent.
#[test]
fn debug_listing_empty_without_configured_fonts() {
    let path = write_config_with_defaults("{}", r#"{"on_start": {"actions": {}}}"#);
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(config.font_details.is_empty());
}
