//! Tests for Chinese/Japanese/Korean text through configured fonts, using tiny in-memory
//! fonts (`tests/common/font_builder.rs`): the `extra` slot, its place at the end of the
//! lookup order (after the emoji fonts), fitting its glyphs into two columns, and bold or
//! italic text falling back to the configured regular font instead of a box.

mod common;

use common::font_builder::{build_font, solid_png, write_font, ColourTables, Glyph};
use dak::actions::load_config_from_path;
use dak::color::Color;
use dak::markup::{parse, Markup};
use dak::text::{render_lines, FontPaths, FontSet};
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

/// A stand-in CJK font: 日, 本 and the squared-CJK emoji 🈚 (U+1F21A) as square outlines.
fn cjk_font() -> Vec<u8> {
    build_font(
        &[
            ('\u{65E5}', Glyph::Square),
            ('\u{672C}', Glyph::Square),
            ('\u{1F21A}', Glyph::Square),
        ],
        ColourTables::default(),
    )
}

/// Loads a font set with the given slots set to fresh files holding `bytes`.
fn fonts_with(slots: &[(&str, &[u8])]) -> FontSet {
    let mut paths = FontPaths::default();
    for (key, bytes) in slots {
        let path = write_font(&format!("{key}.ttf"), bytes);
        *paths.slot_mut(key).unwrap() = Some(path.to_str().unwrap().to_string());
    }
    FontSet::load(&paths).expect("fonts load").0
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

/// Number of fully lit (white) pixels: the stand-in font's squares are solid, while the
/// embedded fonts' missing-glyph box is only an outline.
fn solid(image: &image::RgbImage) -> usize {
    image.pixels().filter(|p| p.0 == [255, 255, 255]).count()
}

/// Without a CJK font an ideograph is a thin missing-glyph box; with `extra` it is drawn
/// from that font.
#[test]
fn extra_draws_cjk() {
    let without = solid(&render("\u{65E5}", &FontSet::embedded()));
    let with = solid(&render("\u{65E5}", &fonts_with(&[("extra", &cjk_font())])));
    assert!(with > without * 3 + 200, "with {with}, without {without}");
}

/// An ideograph takes two columns, so a line holds three of them: a fourth is cut, and
/// the drawing matches the three-character line exactly.
#[test]
fn cjk_takes_two_columns() {
    let fonts = fonts_with(&[("extra", &cjk_font())]);
    assert_eq!(
        render("\u{65E5}\u{672C}\u{65E5}\u{672C}", &fonts),
        render("\u{65E5}\u{672C}\u{65E5}", &fonts)
    );
    // "ab" + two ideographs = 6 columns, all shown; "abc" + two = 7, the last is cut.
    assert_ne!(
        render("ab\u{65E5}\u{672C}", &fonts),
        render("ab\u{65E5}", &fonts)
    );
    assert_eq!(
        render("abc\u{65E5}\u{672C}", &fonts),
        render("abc\u{65E5}", &fonts)
    );
}

/// A character both an emoji font and `extra` have is drawn from the emoji font: here a
/// red colour-bitmap 🈚 wins over the extra font's white square.
#[test]
fn emoji_font_wins_over_extra() {
    let emoji = build_font(
        &[('\u{1F21A}', Glyph::Png(solid_png(32, [255, 0, 0, 255]), 32))],
        ColourTables::default(),
    );
    let fonts = fonts_with(&[("emoji", &emoji), ("extra", &cjk_font())]);
    let image = render("#[u=1F21A]", &fonts);
    assert!(image.pixels().filter(|p| p.0 == [255, 0, 0]).count() > 400);
    assert_eq!(solid(&image), 0);
}

/// The embedded Noto Emoji also comes before `extra`: 🈚 is drawn the same with or
/// without the extra font.
#[test]
fn embedded_emoji_wins_over_extra() {
    assert_eq!(
        render("#[u=1F21A]", &FontSet::embedded()),
        render("#[u=1F21A]", &fonts_with(&[("extra", &cjk_font())]))
    );
}

/// Latin text is untouched by `extra`: it stays in DejaVu.
#[test]
fn extra_leaves_latin_alone() {
    assert_eq!(
        render("abc", &FontSet::embedded()),
        render("abc", &fonts_with(&[("extra", &cjk_font())]))
    );
}

/// With a CJK font as `regular` only, bold and italic ideographs fall back to it rather
/// than to a missing-glyph box (DejaVu Bold/Oblique have no CJK).
#[test]
fn bold_and_italic_fall_back_to_configured_regular() {
    let fonts = fonts_with(&[("regular", &cjk_font())]);
    let regular = solid(&render("\u{65E5}", &fonts));
    for tag in ["#[bold]", "#[italics]", "#[bold,italics]"] {
        let styled = solid(&render(&format!("{tag}\u{65E5}"), &fonts));
        assert!(styled > regular / 2, "{tag}: {styled} vs {regular}");
    }
}

/// Bold CJK via `extra` is drawn (in the extra font's only weight).
#[test]
fn bold_cjk_via_extra() {
    let fonts = fonts_with(&[("extra", &cjk_font())]);
    assert!(solid(&render("#[bold]\u{65E5}", &fonts)) > 200);
}

/// `extra` loads through the config, and a missing file is reported under its name.
#[test]
fn extra_in_config() {
    let font = write_font("cjk.ttf", &cjk_font());
    let path = write_config_with_defaults(
        &format!(r#"{{"fonts": {{"extra": "{}"}}}}"#, font.display()),
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    assert_eq!(config.defaults.fonts.extra.as_deref(), font.to_str());

    let path = write_config_with_defaults(
        r#"{"fonts": {"extra": "/nonexistent/cjk.ttc#0"}}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let errors = error_texts(load_config_from_path(path.to_str().unwrap()).unwrap_err());
    let _ = std::fs::remove_file(&path);
    assert!(
        errors.contains("defaults.fonts.extra: \"/nonexistent/cjk.ttc\""),
        "{errors}"
    );
}
