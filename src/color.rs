//! Solid colours for button images: parsing the `background`/`text_color` config
//! values and compositing transparent images onto an opaque background.
//!
//! A config value is either a `#RRGGBB` literal or one of the CSS basic colour names
//! (plus `orange` and a few aliases). A [`Color`] keeps the exact text it was parsed
//! from alongside its channels, so `$defaults.background`/`text_color` read back the
//! author's value (a name stays a name) instead of a normalised form.

use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};

/// The colour names the config accepts, mapped to their channels.
///
/// The first sixteen are the CSS basic colours; `orange` and the `grey`/`magenta`/
/// `cyan` spellings are added for convenience. Names are matched case-insensitively.
const NAMED_COLOURS: &[(&str, [u8; 3])] = &[
    ("black", [0x00, 0x00, 0x00]),
    ("silver", [0xc0, 0xc0, 0xc0]),
    ("gray", [0x80, 0x80, 0x80]),
    ("grey", [0x80, 0x80, 0x80]),
    ("white", [0xff, 0xff, 0xff]),
    ("maroon", [0x80, 0x00, 0x00]),
    ("red", [0xff, 0x00, 0x00]),
    ("purple", [0x80, 0x00, 0x80]),
    ("fuchsia", [0xff, 0x00, 0xff]),
    ("magenta", [0xff, 0x00, 0xff]),
    ("green", [0x00, 0x80, 0x00]),
    ("lime", [0x00, 0xff, 0x00]),
    ("olive", [0x80, 0x80, 0x00]),
    ("yellow", [0xff, 0xff, 0x00]),
    ("navy", [0x00, 0x00, 0x80]),
    ("blue", [0x00, 0x00, 0xff]),
    ("teal", [0x00, 0x80, 0x80]),
    ("aqua", [0x00, 0xff, 0xff]),
    ("cyan", [0x00, 0xff, 0xff]),
    ("orange", [0xff, 0xa5, 0x00]),
];

/// A button colour: three 8-bit channels plus the text it was parsed from.
///
/// Equality compares channels only, so `Color::parse("red") == Color::parse("#ff0000")`
/// while [`Color::text`] still echoes whichever spelling the author used. The source
/// text is what makes `$defaults.background`/`text_color` reads round-trip.
#[derive(Debug, Clone)]
pub struct Color {
    /// Red, green and blue channels, in that order.
    channels: [u8; 3],
    /// The exact text this colour was parsed from.
    text: String,
}

impl Color {
    /// Builds a colour from raw channels, using the canonical lower-case `#RRGGBB`
    /// text as its source. Used for the built-in defaults.
    pub fn rgb(red: u8, green: u8, blue: u8) -> Self {
        let channels = [red, green, blue];
        Self {
            channels,
            text: format!("#{red:02x}{green:02x}{blue:02x}"),
        }
    }

    /// Parses a config colour: a `#RRGGBB` literal or a named colour.
    ///
    /// Only the full six-digit hex form is accepted (no `#RGB` shorthand and no alpha),
    /// and names are case-insensitive. The original (trimmed) text is kept for
    /// [`Color::text`].
    pub fn parse(value: &str) -> Result<Self, String> {
        let text = value.trim();
        if let Some(hex) = text.strip_prefix('#') {
            if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                let channels = [
                    u8::from_str_radix(&hex[0..2], 16).expect("two hex digits"),
                    u8::from_str_radix(&hex[2..4], 16).expect("two hex digits"),
                    u8::from_str_radix(&hex[4..6], 16).expect("two hex digits"),
                ];
                return Ok(Self {
                    channels,
                    text: text.to_string(),
                });
            }
            return Err(format!(
                "invalid colour \"{value}\": expected \"#RRGGBB\" or a colour name"
            ));
        }

        let lower = text.to_ascii_lowercase();
        match NAMED_COLOURS.iter().find(|(name, _)| *name == lower) {
            Some((_, channels)) => Ok(Self {
                channels: *channels,
                text: text.to_string(),
            }),
            None => Err(format!(
                "unknown colour \"{value}\": expected \"#RRGGBB\" or one of {}",
                colour_names()
            )),
        }
    }

    /// The red, green and blue channels, in that order.
    pub fn channels(&self) -> [u8; 3] {
        self.channels
    }

    /// The exact text this colour was parsed from, as `$defaults.*` reads return it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// An `image` RGB pixel for drawing this colour.
    pub fn to_rgb(&self) -> image::Rgb<u8> {
        image::Rgb(self.channels)
    }
}

impl PartialEq for Color {
    /// Two colours are equal when their channels match, regardless of spelling.
    fn eq(&self, other: &Self) -> bool {
        self.channels == other.channels
    }
}

impl Eq for Color {}

/// Lists the accepted colour names for an error message.
fn colour_names() -> String {
    NAMED_COLOURS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Composites `image` onto an opaque `background`, so a transparent pixel takes the
/// background colour instead of the RGB hidden under `alpha = 0`.
///
/// Returns the image untouched when it has no alpha channel (nothing to composite), and
/// otherwise an opaque RGB image of the same size. This is the fix for transparent icons
/// showing up white: the device's JPEG encoder drops alpha, so the RGB underneath must
/// already be the intended background.
pub fn flatten(image: DynamicImage, background: &Color) -> DynamicImage {
    if !image.has_alpha() {
        return image;
    }
    let [red, green, blue] = background.channels();
    let (width, height) = image.dimensions();
    let mut base = RgbaImage::from_pixel(width, height, Rgba([red, green, blue, 255]));
    image::imageops::overlay(&mut base, &image.to_rgba8(), 0, 0);
    DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(base).to_rgb8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#RRGGBB` parses regardless of case and keeps its original text.
    #[test]
    fn parse_hex_keeps_case_and_text() {
        let colour = Color::parse("#Ab12Cd").unwrap();
        assert_eq!(colour.channels(), [0xab, 0x12, 0xcd]);
        assert_eq!(colour.text(), "#Ab12Cd");
    }

    /// Colour names and their aliases parse, case-insensitively.
    #[test]
    fn parse_named_colours_and_aliases() {
        assert_eq!(Color::parse("red").unwrap().channels(), [0xff, 0x00, 0x00]);
        assert_eq!(Color::parse("RED").unwrap().channels(), [0xff, 0x00, 0x00]);
        assert_eq!(
            Color::parse("grey").unwrap().channels(),
            Color::parse("gray").unwrap().channels()
        );
        assert_eq!(
            Color::parse("magenta").unwrap().channels(),
            Color::parse("fuchsia").unwrap().channels()
        );
        assert_eq!(
            Color::parse("cyan").unwrap().channels(),
            Color::parse("aqua").unwrap().channels()
        );
        assert_eq!(
            Color::parse("orange").unwrap().channels(),
            [0xff, 0xa5, 0x00]
        );
    }

    /// A name is echoed verbatim by `text`, whitespace trimmed.
    #[test]
    fn parse_named_colour_keeps_original_text() {
        assert_eq!(Color::parse("  Red  ").unwrap().text(), "Red");
    }

    /// The `#RGB` shorthand and any other malformed hex are rejected.
    #[test]
    fn parse_rejects_short_and_malformed_hex() {
        for value in ["#fff", "#12345", "#1234567", "#gggggg", "#"] {
            assert!(Color::parse(value).is_err(), "{value} should be rejected");
        }
    }

    /// Unknown names and non-colour strings are rejected with a helpful message.
    #[test]
    fn parse_rejects_unknown_names() {
        let error = Color::parse("chartreuse").unwrap_err();
        assert!(error.contains("unknown colour"), "{error}");
        assert!(error.contains("orange"), "{error}");
        assert!(Color::parse("").is_err());
        assert!(Color::parse("rgb(0,0,0)").is_err());
    }

    /// The built-in constructor uses the canonical lower-case hex text.
    #[test]
    fn rgb_constructor_uses_canonical_text() {
        let colour = Color::rgb(0xff, 0x00, 0x10);
        assert_eq!(colour.channels(), [0xff, 0x00, 0x10]);
        assert_eq!(colour.text(), "#ff0010");
    }

    /// `to_rgb` returns the parsed channels as an `image` RGB pixel.
    #[test]
    fn to_rgb_returns_channels() {
        assert_eq!(Color::parse("red").unwrap().to_rgb().0, [0xff, 0x00, 0x00]);
        assert_eq!(Color::rgb(0x12, 0x34, 0x56).to_rgb().0, [0x12, 0x34, 0x56]);
    }

    /// Equality compares channels, not the spelling of the source text.
    #[test]
    fn equality_ignores_source_text() {
        assert_eq!(
            Color::parse("red").unwrap(),
            Color::parse("#ff0000").unwrap()
        );
        assert_eq!(
            Color::parse("#FF0000").unwrap(),
            Color::parse("#ff0000").unwrap()
        );
    }

    /// An image without an alpha channel is returned unchanged.
    #[test]
    fn flatten_leaves_opaque_images_alone() {
        let image =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])));
        let flattened = flatten(image, &Color::rgb(0, 0, 0));
        assert_eq!(flattened.color(), image::ColorType::Rgb8);
        assert_eq!(flattened.dimensions(), (2, 2));
        assert_eq!(flattened.to_rgb8().get_pixel(1, 1).0, [10, 20, 30]);
    }

    /// A fully transparent pixel becomes the background colour.
    #[test]
    fn flatten_replaces_transparent_pixels_with_background() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 0])));
        let flattened = flatten(image, &Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(flattened.color(), image::ColorType::Rgb8);
        assert_eq!(flattened.to_rgb8().get_pixel(0, 0).0, [0xff, 0x00, 0x00]);
    }

    /// An opaque pixel is preserved by compositing.
    #[test]
    fn flatten_preserves_opaque_pixels() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255])));
        let flattened = flatten(image, &Color::rgb(0xff, 0xff, 0xff));
        assert_eq!(flattened.to_rgb8().get_pixel(0, 0).0, [10, 20, 30]);
    }

    /// A half-transparent white pixel is blended halfway onto the background.
    #[test]
    fn flatten_blends_partial_alpha() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 128])));
        let flattened = flatten(image, &Color::rgb(0, 0, 0));
        let pixel = flattened.to_rgb8().get_pixel(0, 0).0;
        for channel in pixel {
            assert!(
                (channel as i32 - 127).abs() <= 2,
                "expected ~127, got {channel}"
            );
        }
    }
}
