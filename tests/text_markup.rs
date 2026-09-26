//! Tests for the text-markup and font config: the `markup` setup field and
//! `defaults.markup`, `defaults.fonts` validation and loading, and how both show up in
//! the loaded config and the runtime variable state.

mod common;

use std::sync::Arc;

use dak::actions::load_config_from_path;
use dak::markup::Markup;
use dak::text::FontSet;
use dak::variables::Variables;

use crate::common::{
    assert_validation_error, error_texts, temp_dir, write_config_with_defaults,
    write_scenes_config, write_temp_config, SetHome, ENV_LOCK,
};

/// A scenes object with one `text_value` button showing `params`.
fn text_value_scene(params: &str) -> String {
    format!(
        r#"{{"on_start": {{"setup": {{"1b01": {{"type": "text_value", "params": "{params}"}}}}}}}}"#
    )
}

/// Loads a config with the given `defaults` and scenes, removing the temp file after.
fn load_with_defaults(
    defaults: &str,
    scenes: &str,
) -> Result<dak::actions::LoadedConfig, Vec<String>> {
    let path = write_config_with_defaults(defaults, scenes);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    config
}

/// Loads a config whose only non-trivial part is `defaults.fonts` = `fonts`.
fn load_fonts(fonts: &str) -> Result<dak::actions::LoadedConfig, Vec<String>> {
    load_with_defaults(
        &format!(r#"{{"fonts": {fonts}}}"#),
        r#"{"on_start": {"actions": {}}}"#,
    )
}

/// Copies the embedded DejaVu Bold face to a fresh temp dir, returning (dir, font path),
/// so tests exercise real font loading without shipping extra font files.
fn temp_font() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = temp_dir();
    let font = dir.join("custom.ttf");
    std::fs::copy("fonts/DejaVuSansMono-Bold.ttf", &font).unwrap();
    (dir, font)
}

/// Without `defaults.markup` the markup is tmux, and `"none"` switches it off.
#[test]
fn defaults_markup_loads() {
    let config = load_with_defaults("{}", &text_value_scene("x")).unwrap();
    assert_eq!(config.defaults.markup, Markup::Tmux);
    let config = load_with_defaults(r#"{"markup": "none"}"#, &text_value_scene("x")).unwrap();
    assert_eq!(config.defaults.markup, Markup::None);
}

/// An unknown or non-string `defaults.markup` is a config error.
#[test]
fn defaults_markup_rejects_bad_values() {
    let errors = error_texts(
        load_with_defaults(r#"{"markup": "bbcode"}"#, &text_value_scene("x")).unwrap_err(),
    );
    assert!(
        errors.contains("defaults.markup: unknown markup \"bbcode\""),
        "{errors}"
    );
    let errors =
        error_texts(load_with_defaults(r#"{"markup": 1}"#, &text_value_scene("x")).unwrap_err());
    assert!(errors.contains("defaults.markup must be"), "{errors}");
}

/// The `markup` field is accepted on every text type.
#[test]
fn entry_markup_accepted_on_text_types() {
    let path = write_scenes_config(
        r#"{"on_start": {"setup": {
            "1b01": {"type": "text_value", "params": "x", "markup": "none"},
            "1b02": {"type": "text_exec", "params": "echo x", "markup": "tmux"},
            "1b03": {"type": "text", "params": "/tmp", "markup": "none"}
        }}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{config:?}");
}

/// `markup` on a type that draws no text is rejected, as is an unknown value.
#[test]
fn entry_markup_rejected_on_other_types_and_bad_values() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/a", "markup": "none"}}}}"#,
        "markup cannot be used with type \"image\"",
    );
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_value", "params": "x", "markup": "html"}}}}"#,
        "markup: unknown markup \"html\"",
    );
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_value", "params": "x", "markup": true}}}}"#,
        "markup must be \"none\" or \"tmux\"",
    );
}

/// A malformed tag in a literal `text_value` is a load-time warning (it still loads and
/// would be drawn literally); with `"markup": "none"` there is nothing to warn about.
#[test]
fn literal_text_value_markup_errors_are_warnings() {
    let config = load_with_defaults("{}", &text_value_scene("#[blink]x")).unwrap();
    let warnings = config.warnings.join("\n");
    assert!(
        warnings.contains("unknown attribute \"blink\""),
        "{warnings}"
    );

    let config =
        load_with_defaults(r#"{"markup": "none"}"#, &text_value_scene("#[blink]x")).unwrap();
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
}

/// A `text_value` built from a reference is only known at runtime, so it is not checked.
#[test]
fn dynamic_text_value_markup_is_not_checked() {
    let path = write_temp_config(
        r##"{"variables": {"v": {"type": "str", "value": "x"}},
            "scenes": {"on_start": {"setup": {"1b01": {"type": "text_value", "params": "#[blink]$v"}}}},
            "devices": {}}"##,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.unwrap().warnings.is_empty());
}

/// Without `defaults.fonts` the loaded font set is the shared embedded one.
#[test]
fn no_fonts_uses_embedded_set() {
    let config = load_with_defaults("{}", &text_value_scene("x")).unwrap();
    assert!(Arc::ptr_eq(&config.fonts, &FontSet::embedded()));
}

/// A readable font file in every slot loads into a custom font set.
#[test]
fn fonts_load_from_files() {
    let (dir, font) = temp_font();
    let font = font.to_str().unwrap();
    let config = load_fonts(&format!(
        r#"{{"regular": "{font}", "bold": "{font}", "italic": "{font}", "bold_italic": "{font}", "emoji": "{font}"}}"#
    ))
    .unwrap();
    assert!(!Arc::ptr_eq(&config.fonts, &FontSet::embedded()));
    assert_eq!(config.defaults.fonts.regular.as_deref(), Some(font));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing font file stops the config from loading, naming the slot and the path.
#[test]
fn fonts_missing_file_is_an_error() {
    let errors = error_texts(load_fonts(r#"{"bold": "/nonexistent/dak-font.ttf"}"#).unwrap_err());
    assert!(
        errors.contains("defaults.fonts.bold: \"/nonexistent/dak-font.ttf\""),
        "{errors}"
    );
}

/// A file that is not a font, or a collection index a plain font lacks, is an error.
#[test]
fn fonts_invalid_file_and_index_are_errors() {
    let dir = temp_dir();
    let garbage = dir.join("garbage.ttf");
    std::fs::write(&garbage, b"not a font at all").unwrap();
    let errors =
        error_texts(load_fonts(&format!(r#"{{"regular": "{}"}}"#, garbage.display())).unwrap_err());
    assert!(
        errors.contains("is not a TrueType/OpenType font"),
        "{errors}"
    );

    let (font_dir, font) = temp_font();
    let errors =
        error_texts(load_fonts(&format!(r#"{{"emoji": "{}#3"}}"#, font.display())).unwrap_err());
    assert!(errors.contains("defaults.fonts.emoji"), "{errors}");
    assert!(errors.contains("face #3"), "{errors}");
    // Face 0 of a plain font is the font itself.
    assert!(load_fonts(&format!(r#"{{"emoji": "{}#0"}}"#, font.display())).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&font_dir);
}

/// A directory is not accepted as a font file.
#[test]
fn fonts_directory_is_an_error() {
    let dir = temp_dir();
    let errors =
        error_texts(load_fonts(&format!(r#"{{"regular": "{}"}}"#, dir.display())).unwrap_err());
    assert!(errors.contains("is not a regular file"), "{errors}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `defaults.fonts` must be an object of known keys with non-empty string paths.
#[test]
fn fonts_shape_errors() {
    let errors = error_texts(load_fonts(r#""/a.ttf""#).unwrap_err());
    assert!(
        errors.contains("defaults.fonts must be an object"),
        "{errors}"
    );
    let errors = error_texts(load_fonts(r#"{"mono": "/a.ttf"}"#).unwrap_err());
    assert!(
        errors.contains("defaults.fonts: unknown key \"mono\""),
        "{errors}"
    );
    let errors = error_texts(load_fonts(r#"{"bold": ""}"#).unwrap_err());
    assert!(
        errors.contains("defaults.fonts.bold must be a font file path"),
        "{errors}"
    );
    let errors = error_texts(load_fonts(r#"{"bold": 5}"#).unwrap_err());
    assert!(
        errors.contains("defaults.fonts.bold must be a font file path"),
        "{errors}"
    );
}

/// Font paths expand a leading `~` to `$HOME`.
#[test]
fn fonts_expand_tilde() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let (dir, _font) = temp_font();
    let _home = SetHome::new(&dir);
    assert!(load_fonts(r#"{"regular": "~/custom.ttf"}"#).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Font paths expand `$` references from the variables' initial values, and an
/// undefined reference is an error.
#[test]
fn fonts_expand_variables() {
    let (dir, font) = temp_font();
    let config = format!(
        r#"{{"variables": {{"font_dir": {{"type": "str", "value": "{}"}}}},
            "defaults": {{"fonts": {{"bold": "$font_dir/custom.ttf"}}}},
            "scenes": {{"on_start": {{"actions": {{}}}}}}, "devices": {{}}}}"#,
        dir.display()
    );
    let path = write_temp_config(&config);
    let loaded = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(loaded.is_ok(), "{loaded:?} for {}", font.display());

    let errors = error_texts(load_fonts(r#"{"bold": "$nope/custom.ttf"}"#).unwrap_err());
    assert!(errors.contains("defaults.fonts.bold"), "{errors}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `$defaults.markup` reads the configured markup as text.
#[test]
fn defaults_markup_is_readable() {
    let config = load_with_defaults(r#"{"markup": "none"}"#, &text_value_scene("x")).unwrap();
    let variables = Variables::new(config.variables.clone(), &config.defaults);
    assert_eq!(variables.expand("$defaults.markup").unwrap(), "none");
}

/// `markup` and `fonts` are loaded once at startup, so assigning them is rejected as
/// read-only rather than as an unknown parameter.
#[test]
fn markup_and_fonts_are_read_only() {
    for (target, expected_path) in [
        ("$defaults.markup := 1", "defaults.markup"),
        ("$defaults.fonts := 1", "defaults.fonts"),
        ("$defaults.fonts.bold := 1", "defaults.fonts.bold"),
    ] {
        assert_validation_error(
            &format!(r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{target}"}}}}}}}}"#),
            &format!("tries to set read-only parameter \"{expected_path}\""),
        );
    }
}
