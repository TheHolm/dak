//! Text rendering for button LCDs using a font embedded into the binary.

use std::error::Error;

use ab_glyph::{point, Font, FontRef, ScaleFont};
use image::{Rgb, RgbImage};
use mirajazz::types::ImageFormat;

use crate::color::Color;

/// Maximum number of characters shown per line on a button.
///
/// Matches the config documentation for the `text` and `text_exec` commands.
pub const MAX_LINE_CHARS: usize = 6;

/// Maximum number of lines shown per button.
///
/// Matches the config documentation for the `text` and `text_exec` commands.
pub const MAX_LINES: usize = 3;

/// DejaVu Sans Mono embedded into the binary, so text rendering works without
/// any font files installed on the host. Free to redistribute, see `fonts/LICENSE.txt`.
static FONT_BYTES: &[u8] = include_bytes!("../fonts/DejaVuSansMono.ttf");

/// The default text colour (white), used by [`render_text`].
fn default_text_color() -> Color {
    Color::rgb(0xff, 0xff, 0xff)
}

/// The default background colour (black), used by [`render_text`] and [`render_error_image`].
fn default_background() -> Color {
    Color::rgb(0x00, 0x00, 0x00)
}

/// Extracts up to three lines of up to six characters from `text`, as shown on a button.
///
/// Lines are split on newline characters and each line is truncated to
/// [`MAX_LINE_CHARS`] characters; only the first [`MAX_LINES`] lines are kept.
pub fn button_text(text: &str) -> Vec<String> {
    text.lines()
        .take(MAX_LINES)
        .map(|line| line.chars().take(MAX_LINE_CHARS).collect())
        .collect()
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
    render_image(
        lines,
        &default_text_color(),
        &default_background(),
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
    render_image(lines, text_color, background, image_format)
}

/// Renders the label "Error" in red, centered, used when an async command fails,
/// times out or is interrupted. Deliberately unaffected by the configured colours.
pub fn render_error_image(
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    render_image(
        &["Error".to_string()],
        &Color::rgb(0xff, 0x00, 0x00),
        &default_background(),
        image_format,
    )
}

/// Renders the text of `lines` in `color` onto a `background`-filled button-sized image.
///
/// Shared by [`render_text_colored`] and [`render_error_image`]; see [`render_text`] for
/// an explanation of the geometry.
fn render_image(
    lines: &[String],
    color: &Color,
    background: &Color,
    image_format: ImageFormat,
) -> Result<image::DynamicImage, Box<dyn Error>> {
    let (width, height) = (image_format.size.0 as u32, image_format.size.1 as u32);
    if width == 0 || height == 0 {
        return Err("render_text: image format size must be non-zero".into());
    }

    let font: FontRef = FontRef::try_from_slice(FONT_BYTES)?;
    let mut image = RgbImage::from_pixel(width, height, background.to_rgb());
    let foreground = color.channels();
    let backdrop = background.channels();

    // Reference metrics from a 1px scale; every metric scales linearly, so the font
    // size can be solved from the width and height budgets below.
    let reference = font.as_scaled(1.0);
    let advance = reference.h_advance(reference.glyph_id('M'));
    let line_height = reference.ascent() - reference.descent();
    let line_gap = reference.line_gap();

    let line_count = lines.len().max(1) as f32;
    let char_count = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    // Largest uniform scale that keeps every column and all lines inside the button.
    let scale_x = width as f32 / (char_count * advance);
    let scale_y = height as f32 / (line_count * line_height + (line_count - 1.0) * line_gap);
    let scale = scale_x.min(scale_y);

    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_height = ascent - descent;
    let gap = scaled.line_gap();
    let total_height = line_count * line_height + (line_count - 1.0) * gap;
    let top = (height as f32 - total_height) / 2.0;

    for (index, line) in lines.iter().enumerate() {
        // Baseline of each line: `index` full lines above it.
        let baseline = top + ascent + index as f32 * (line_height + gap);
        let line_width: f32 = line
            .chars()
            .map(|ch| scaled.h_advance(scaled.glyph_id(ch)))
            .sum();
        let mut x = (width as f32 - line_width) / 2.0;

        for ch in line.chars() {
            let mut glyph = scaled.scaled_glyph(ch);
            glyph.position = point(x, baseline);
            if let Some(outline) = scaled.outline_glyph(glyph) {
                // draw() reports glyph-local pixel coordinates, so translate them by
                // the glyph's pixel bounds origin to reach image coordinates.
                let (offset_x, offset_y) = (
                    outline.px_bounds().min.x as i32,
                    outline.px_bounds().min.y as i32,
                );
                outline.draw(|px, py, coverage| {
                    let (px, py) = (px as i32 + offset_x, py as i32 + offset_y);
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        // Blend the glyph's colour over the background by its coverage,
                        // so partially covered edge pixels mix rather than replace.
                        let alpha = coverage.clamp(0.0, 1.0);
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
            x += scaled.h_advance(scaled.glyph_id(ch));
        }
    }

    Ok(image::DynamicImage::ImageRgb8(image))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Rendering writes the text into a button-sized image with visible, grayscale pixels.
    #[test]
    fn render_text_draws_glyphs() {
        let image = render_text(
            &["Hi".to_string()],
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (60, 60),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
        )
        .expect("render")
        .to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
        let has_pixels = image.pixels().any(|p| p.0 != [0, 0, 0]);
        assert!(has_pixels, "expected some non-black pixels");
    }

    /// Rendering scales the text so two full-width lines still fit vertically.
    #[test]
    fn render_text_fits_two_lines() {
        let image = render_text(
            &["abcdef".to_string(), "ghijkl".to_string()],
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (60, 60),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
        )
        .expect("render")
        .to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
    }

    /// A zero-sized image format is rejected because no glyphs can be rendered.
    #[test]
    fn render_image_rejects_zero_size() {
        let error = render_text(
            &["x".to_string()],
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (0, 0),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
        )
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
        let image = render_text(
            &["line1".to_string(), "line2 ".to_string()],
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (60, 60),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
        )
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
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (60, 60),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
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
            ImageFormat {
                mode: mirajazz::types::ImageMode::None,
                size: (60, 60),
                rotation: mirajazz::types::ImageRotation::Rot0,
                mirror: mirajazz::types::ImageMirroring::None,
            },
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
        let image = render_error_image(ImageFormat {
            mode: mirajazz::types::ImageMode::None,
            size: (60, 60),
            rotation: mirajazz::types::ImageRotation::Rot0,
            mirror: mirajazz::types::ImageMirroring::None,
        })
        .expect("render")
        .to_rgb8();
        assert_eq!(image.dimensions(), (60, 60));
        assert!(
            image
                .pixels()
                .any(|p| p.0[0] > 100 && p.0[1] == 0 && p.0[2] == 0),
            "expected red pixels"
        );
    }
}
