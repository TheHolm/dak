//! Runtime variables: declaration, validation, storage and substitution.
//!
//! Variables are declared in the config's top-level `variables` object. They are global:
//! one set shared by every device and every scene, not copied per device or reset on a
//! scene switch. A declaration fixes a variable's type ([`VarType::Int`] or
//! [`VarType::Str`]), its constraints (an inclusive range for an `int`, a maximum
//! character count for a `str`) and the value it starts the program with. Every part of
//! a declaration is validated at config-load time, so a malformed or out-of-range
//! declaration is a hard config error rather than a runtime surprise.
//!
//! [`VariableStore`] holds the declarations alongside each variable's *current* value.
//! Reading a variable (`$name`, see later commits' expansion) always returns the current
//! value, while the constraints still come from the declaration.

use serde_json::Value;
use std::collections::BTreeMap;

use crate::actions::value_type;
use crate::press::Defaults;

/// The default `max_length` of a `str` variable when the declaration omits it.
pub const DEFAULT_MAX_LENGTH: usize = 255;

/// The largest `max_length` a `str` variable may declare (a hard cap, even though a
/// `str` value is otherwise "variable length").
pub const MAX_ALLOWED_LENGTH: usize = 65535;

/// The value type a variable was declared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    /// A 32-bit signed integer.
    Int,
    /// A variable-length unicode string.
    Str,
}

/// A variable's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarValue {
    /// An integer value.
    Int(i32),
    /// A string value.
    Str(String),
}

impl VarValue {
    /// The [`VarType`] this value belongs to.
    pub fn kind(&self) -> VarType {
        match self {
            VarValue::Int(_) => VarType::Int,
            VarValue::Str(_) => VarType::Str,
        }
    }

    /// The value as an `i32`, or `None` when it is a string.
    pub fn as_int(&self) -> Option<i32> {
        match self {
            VarValue::Int(number) => Some(*number),
            VarValue::Str(_) => None,
        }
    }

    /// The value as a string slice, or `None` when it is an integer.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            VarValue::Int(_) => None,
            VarValue::Str(text) => Some(text),
        }
    }

    /// The value rendered for text substitution: an integer as its decimal form, a
    /// string as-is (no surrounding quotes added).
    pub fn to_text(&self) -> String {
        match self {
            VarValue::Int(number) => number.to_string(),
            VarValue::Str(text) => text.clone(),
        }
    }
}

/// A validated variable declaration: its type, constraints and initial value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDef {
    /// The declared type. Kept explicit even though [`VarDef::initial`] also implies it,
    /// so range/length constraints can be interpreted without matching the value.
    pub kind: VarType,
    /// Inclusive lower bound of an `int` variable (unused for `str`).
    pub min: i32,
    /// Inclusive upper bound of an `int` variable (unused for `str`).
    pub max: i32,
    /// Maximum number of characters of a `str` variable (unused for `int`).
    pub max_length: usize,
    /// The value the variable starts the program with.
    pub initial: VarValue,
}

impl VarDef {
    /// An `int` variable with inclusive bounds `min..=max` starting at `initial`.
    pub fn int(min: i32, max: i32, initial: i32) -> Self {
        Self {
            kind: VarType::Int,
            min,
            max,
            max_length: 0,
            initial: VarValue::Int(initial),
        }
    }

    /// A `str` variable of at most `max_length` characters starting at `initial`.
    pub fn string(max_length: usize, initial: String) -> Self {
        Self {
            kind: VarType::Str,
            min: 0,
            max: 0,
            max_length,
            initial: VarValue::Str(initial),
        }
    }
}

/// The declared variables and each one's current value.
#[derive(Debug, Clone)]
pub struct VariableStore {
    /// Validated declarations, keyed by variable name.
    defs: BTreeMap<String, VarDef>,
    /// Current values, keyed by variable name. Always has exactly the same keys as
    /// `defs`: every declared variable starts at its initial value and assignments only
    /// replace existing entries.
    values: BTreeMap<String, VarValue>,
}

impl VariableStore {
    /// Builds a store from validated declarations, with every variable at its initial
    /// value.
    pub fn new(defs: BTreeMap<String, VarDef>) -> Self {
        let values = defs
            .iter()
            .map(|(name, def)| (name.clone(), def.initial.clone()))
            .collect();
        Self { defs, values }
    }

    /// The declaration of `name`, or `None` when it is not declared.
    pub fn def(&self, name: &str) -> Option<&VarDef> {
        self.defs.get(name)
    }

    /// The current value of `name`, or `None` when it is not declared.
    pub fn get(&self, name: &str) -> Option<&VarValue> {
        self.values.get(name)
    }

    /// Whether `name` is declared.
    pub fn contains(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    /// Replaces the current value of `name`. Callers are responsible for having
    /// validated/clamped the value; setting an undeclared name is a no-op so a bug
    /// cannot silently introduce a variable.
    pub fn set(&mut self, name: &str, value: VarValue) {
        if let Some(slot) = self.values.get_mut(name) {
            *slot = value;
        }
    }

    /// The names and declarations of every variable, in ascending name order.
    pub fn defs(&self) -> &BTreeMap<String, VarDef> {
        &self.defs
    }
}

/// Whether `name` is a valid variable name: a nonempty sequence starting with an ASCII
/// letter, followed by ASCII letters, digits or underscores.
///
/// JSON object keys are arbitrary strings, so this is the gate that rejects e.g. `1a`,
/// `first-name` or an empty name before it can be referenced by a `$` substitution.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Validates the optional top-level `variables` section and returns the declarations.
///
/// A missing section yields an empty map; a malformed one contributes errors and yields
/// only the declarations that parsed (the caller rejects the whole config when any errors
/// were collected, so a partial map is never used).
pub(crate) fn check_variables(
    variables: &Value,
    errors: &mut Vec<String>,
) -> BTreeMap<String, VarDef> {
    let mut defs = BTreeMap::new();

    let Some(map) = variables.as_object() else {
        errors.push(format!(
            "config \"variables\" must be an object of variable declarations, got {}",
            value_type(variables)
        ));
        return defs;
    };

    for (name, declaration) in map {
        if let Some(def) = check_variable(name, declaration, errors) {
            defs.insert(name.clone(), def);
        }
    }
    defs
}

/// Validates a single variable declaration: `{ "type": "int"|"str", ... }` with only the
/// keys the chosen type allows.
fn check_variable(name: &str, declaration: &Value, errors: &mut Vec<String>) -> Option<VarDef> {
    if !is_valid_name(name) {
        errors.push(format!(
            "variables: invalid variable name \"{name}\", expected a name starting with a letter and containing only letters, digits and underscores"
        ));
        return None;
    }

    let Some(map) = declaration.as_object() else {
        errors.push(format!(
            "variables.{name} must be an object with a \"type\" key, got {}",
            value_type(declaration)
        ));
        return None;
    };

    const KNOWN_KEYS: &[&str] = &["type", "min", "max", "max_length", "value"];
    let mut unknown_key = false;
    for key in map.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            errors.push(format!(
                "variables.{name}: unknown key \"{key}\", expected one of {}",
                KNOWN_KEYS
                    .iter()
                    .map(|key| format!("\"{key}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            unknown_key = true;
        }
    }
    if unknown_key {
        return None;
    }

    let Some(kind) = map.get("type").and_then(Value::as_str) else {
        errors.push(format!(
            "variables.{name}: \"type\" must be a string, either \"int\" or \"str\""
        ));
        return None;
    };

    match kind {
        "int" => check_int_variable(name, map, errors),
        "str" => check_str_variable(name, map, errors),
        other => {
            errors.push(format!(
                "variables.{name}: unknown type \"{other}\", expected \"int\" or \"str\""
            ));
            None
        }
    }
}

/// Validates an `int` declaration: optional 32-bit `min`/`max` (inclusive, defaulting to
/// the full `i32` range) and an optional in-range `value` (defaulting to `0`).
fn check_int_variable(
    name: &str,
    map: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) -> Option<VarDef> {
    if map.contains_key("max_length") {
        errors.push(format!(
            "variables.{name}: \"max_length\" is only valid for a \"str\" variable"
        ));
        return None;
    }

    let min = match map.get("min") {
        Some(value) => match json_i32(value) {
            Some(number) => number,
            None => {
                errors.push(format!(
                    "variables.{name}: \"min\" must be a 32-bit integer, got {}",
                    value_type(value)
                ));
                return None;
            }
        },
        None => i32::MIN,
    };
    let max = match map.get("max") {
        Some(value) => match json_i32(value) {
            Some(number) => number,
            None => {
                errors.push(format!(
                    "variables.{name}: \"max\" must be a 32-bit integer, got {}",
                    value_type(value)
                ));
                return None;
            }
        },
        None => i32::MAX,
    };
    if min > max {
        errors.push(format!(
            "variables.{name}: \"min\" {min} is greater than \"max\" {max}"
        ));
        return None;
    }

    let initial = match map.get("value") {
        Some(value) => match json_i32(value) {
            Some(number) => number,
            None => {
                errors.push(format!(
                    "variables.{name}: \"value\" must be a 32-bit integer, got {}",
                    value_type(value)
                ));
                return None;
            }
        },
        None => 0,
    };
    if initial < min || initial > max {
        errors.push(format!(
            "variables.{name}: \"value\" {initial} is outside the declared range {min}..={max}"
        ));
        return None;
    }

    Some(VarDef::int(min, max, initial))
}

/// Validates a `str` declaration: optional `max_length` (1..=65535, default
/// [`DEFAULT_MAX_LENGTH`]) and an optional `value` no longer than that limit.
fn check_str_variable(
    name: &str,
    map: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) -> Option<VarDef> {
    if map.contains_key("min") || map.contains_key("max") {
        errors.push(format!(
            "variables.{name}: \"min\"/\"max\" are only valid for an \"int\" variable"
        ));
        return None;
    }

    let max_length = match map.get("max_length") {
        Some(value) => match value.as_u64() {
            Some(number) if (1..=MAX_ALLOWED_LENGTH as u64).contains(&number) => number as usize,
            Some(number) => {
                errors.push(format!(
                    "variables.{name}: \"max_length\" must be between 1 and {MAX_ALLOWED_LENGTH}, got {number}"
                ));
                return None;
            }
            None => {
                errors.push(format!(
                    "variables.{name}: \"max_length\" must be a positive integer, got {}",
                    value_type(value)
                ));
                return None;
            }
        },
        None => DEFAULT_MAX_LENGTH,
    };

    let initial = match map.get("value") {
        Some(value) => match value.as_str() {
            Some(text) => text.to_string(),
            None => {
                errors.push(format!(
                    "variables.{name}: \"value\" must be a string, got {}",
                    value_type(value)
                ));
                return None;
            }
        },
        None => String::new(),
    };
    let length = initial.chars().count();
    if length > max_length {
        errors.push(format!(
            "variables.{name}: \"value\" is {length} characters, longer than \"max_length\" {max_length}"
        ));
        return None;
    }

    Some(VarDef::string(max_length, initial))
}

/// Parses a JSON value as an `i32`, accepting only integers (not floats) that fit in the
/// 32-bit signed range.
fn json_i32(value: &Value) -> Option<i32> {
    value.as_i64().and_then(|number| i32::try_from(number).ok())
}

/// The address space a `$` reference's name is resolved against.
///
/// A bare `$name` is shorthand for the variables scope (`$var.name`); other scopes are
/// written explicitly. More scopes (e.g. `env`) are expected in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The user-declared variables (the default scope).
    Var,
    /// The built-in `defaults` parameters, which behave like variables but also drive
    /// the device (see [`Variables`]).
    Defaults,
}

impl Scope {
    /// Parses a scope name, or `None` when it is not a known scope.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "var" => Some(Scope::Var),
            "defaults" => Some(Scope::Defaults),
            _ => None,
        }
    }
}

/// A fully parsed `$` reference: a scope and a name within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarRef {
    /// The scope the name is resolved in.
    pub scope: Scope,
    /// The variable/parameter name.
    pub name: String,
}

impl std::fmt::Display for VarRef {
    /// Renders the reference in its source form: `$name` for the default scope, or
    /// `$scope.name` otherwise.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.scope {
            Scope::Var => write!(f, "${}", self.name),
            Scope::Defaults => write!(f, "$defaults.{}", self.name),
        }
    }
}

/// Reads a variable name starting at `index`: an ASCII letter followed by ASCII letters,
/// digits or underscores. Advances `index` past the name.
fn parse_name(chars: &[char], index: &mut usize) -> Option<String> {
    let start = *index;
    match chars.get(*index) {
        Some(c) if c.is_ascii_alphabetic() => *index += 1,
        _ => return None,
    }
    while let Some(c) = chars.get(*index) {
        if c.is_ascii_alphanumeric() || *c == '_' {
            *index += 1;
        } else {
            break;
        }
    }
    Some(chars[start..*index].iter().collect())
}

/// Expands every `$` reference in `text`, using `resolve` to turn a reference into its
/// replacement text.
///
/// Reference syntax is `$name` (the variables scope) or `$scope.name`; names are greedy
/// over letters/digits/underscores. Backslash is the escape character: `\$` produces a
/// literal `$` and `\\` a literal backslash. A backslash immediately *after* a reference
/// terminates its name and escapes the next character (`$name\kun` is `<value>kun`);
/// every other backslash is passed through untouched so the downstream command tokenizer
/// still sees it. A `$` not followed by a variable name, an unknown scope, or a reference
/// `resolve` rejects is an error describing the first problem found.
pub fn expand_with<F>(text: &str, mut resolve: F) -> Result<String, String>
where
    F: FnMut(&VarRef) -> Result<String, String>,
{
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c == '\\' {
            match chars.get(index + 1) {
                Some(next @ ('$' | '\\')) => {
                    out.push(*next);
                    index += 2;
                }
                _ => {
                    out.push('\\');
                    index += 1;
                }
            }
        } else if c == '$' {
            index += 1;
            let Some(first) = parse_name(&chars, &mut index) else {
                return Err(format!(
                    "\"$\" in \"{text}\" must be followed by a variable name"
                ));
            };
            // A dot only separates a scope from a name when the leading token is a known
            // scope keyword (`var`, `defaults`, ...). Otherwise it is literal text, so
            // `$name.png` is the variable `name` followed by `.png`, not scope `name`.
            // Scope keywords are therefore reserved and cannot also name a variable.
            let scope = Scope::parse(&first);
            let reference = if scope.is_some() && chars.get(index) == Some(&'.') {
                let scope = scope.expect("checked just above");
                index += 1;
                let Some(name) = parse_name(&chars, &mut index) else {
                    return Err(format!(
                        "\"$\" in \"{text}\" must name a variable after \"{first}.\""
                    ));
                };
                VarRef { scope, name }
            } else {
                VarRef {
                    scope: Scope::Var,
                    name: first,
                }
            };
            out.push_str(&resolve(&reference)?);
            // A backslash directly after a reference ends its name: consume it and emit
            // the escaped character, so it cannot leak into the downstream tokenizer.
            if chars.get(index) == Some(&'\\') {
                if let Some(next) = chars.get(index + 1) {
                    out.push(*next);
                    index += 2;
                } else {
                    out.push('\\');
                    index += 1;
                }
            }
        } else {
            out.push(c);
            index += 1;
        }
    }
    Ok(out)
}

/// Syntax-only scan: returns every `$` reference in `text` in source order, erroring on
/// the same malformed syntax [`expand_with`] rejects. Values are not resolved, so this is
/// what config validation uses to check references against the declarations.
pub fn references_in(text: &str) -> Result<Vec<VarRef>, String> {
    let mut refs = Vec::new();
    expand_with(text, |reference| {
        refs.push(reference.clone());
        Ok(String::new())
    })?;
    Ok(refs)
}

/// The current state of every variable and `defaults` parameter.
///
/// `defaults` entries live in the same namespace as user variables but are fixed (their
/// names are known) and, for the writable ones, also change device behaviour when
/// assigned - see the assignment code in `actions.rs`. Variable reads always return the
/// current value here, never the declaration's initial value.
#[derive(Debug, Clone)]
pub struct Variables {
    /// User-declared variables and their current values.
    store: VariableStore,
    /// Current button/screen LCD brightness (0-100).
    button_brightness: i32,
    /// Current encoder LED-ring brightness (0-100).
    encoder_brightness: i32,
    /// Current short-press threshold in milliseconds.
    short_press_duration_ms: i64,
    /// Current double-click gap in milliseconds.
    double_click_gap_ms: i64,
}

impl Variables {
    /// Builds the runtime state from validated declarations and the loaded `defaults`.
    pub fn new(defs: BTreeMap<String, VarDef>, defaults: &Defaults) -> Self {
        Self {
            store: VariableStore::new(defs),
            button_brightness: defaults.button_brightness as i32,
            encoder_brightness: defaults.encoder_brightness as i32,
            short_press_duration_ms: duration_millis(defaults.short_press_duration),
            double_click_gap_ms: duration_millis(defaults.double_click_gap),
        }
    }

    /// The user-variable store, for direct access by callers that need definitions or
    /// raw values (e.g. assignment).
    pub fn store(&self) -> &VariableStore {
        &self.store
    }

    /// Mutable access to the user-variable store.
    pub fn store_mut(&mut self) -> &mut VariableStore {
        &mut self.store
    }

    /// Resolves `reference` to its current text: an integer as decimal, a string as-is.
    pub fn read(&self, reference: &VarRef) -> Result<String, String> {
        match reference.scope {
            Scope::Var => match self.store.get(&reference.name) {
                Some(value) => Ok(value.to_text()),
                None => Err(format!("undefined variable \"{reference}\"")),
            },
            Scope::Defaults => match reference.name.as_str() {
                "button_brightness" => Ok(self.button_brightness.to_string()),
                "encoder_brightness" => Ok(self.encoder_brightness.to_string()),
                "short_press_duration" => Ok(self.short_press_duration_ms.to_string()),
                "double_click_gap" => Ok(self.double_click_gap_ms.to_string()),
                _ => Err(format!("undefined variable \"{reference}\"")),
            },
        }
    }

    /// The declared type of `reference`, or `None` when it names nothing.
    pub fn kind_of(&self, reference: &VarRef) -> Option<VarType> {
        match reference.scope {
            Scope::Var => self.store.def(&reference.name).map(|def| def.kind),
            Scope::Defaults => match reference.name.as_str() {
                "button_brightness"
                | "encoder_brightness"
                | "short_press_duration"
                | "double_click_gap" => Some(VarType::Int),
                _ => None,
            },
        }
    }

    /// Expands every `$` reference in `text` from the current state.
    pub fn expand(&self, text: &str) -> Result<String, String> {
        expand_with(text, |reference| self.read(reference))
    }

    /// The current button/screen brightness (0-100).
    pub fn button_brightness(&self) -> i32 {
        self.button_brightness
    }

    /// The current encoder LED-ring brightness (0-100).
    pub fn encoder_brightness(&self) -> i32 {
        self.encoder_brightness
    }

    /// Sets the button/screen brightness (the caller clamps it to 0-100 first).
    pub fn set_button_brightness(&mut self, value: i32) {
        self.button_brightness = value;
    }

    /// Sets the encoder LED-ring brightness (the caller clamps it to 0-100 first).
    pub fn set_encoder_brightness(&mut self, value: i32) {
        self.encoder_brightness = value;
    }
}

/// A duration in whole milliseconds as an `i64`, saturating rather than wrapping on an
/// absurdly large configured value.
fn duration_millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `is_valid_name` accepts names starting with a letter and rejects empty names,
    /// leading digits, hyphens and other punctuation.
    #[test]
    fn valid_names_start_with_a_letter() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("A"));
        assert!(is_valid_name("counter_1"));
        assert!(is_valid_name("x9_"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("1a"));
        assert!(!is_valid_name("_a"));
        assert!(!is_valid_name("first-name"));
        assert!(!is_valid_name("a.b"));
        assert!(!is_valid_name("a b"));
    }

    /// A well-formed `int` and `str` declaration parse into their declared constraints
    /// and initial values.
    #[test]
    fn check_variables_accepts_valid_declarations() {
        let mut errors = Vec::new();
        let defs = check_variables(
            &json!({
                "count": { "type": "int", "min": 0, "max": 10, "value": 3 },
                "name": { "type": "str", "max_length": 5, "value": "Bob" },
                "plain": { "type": "int" }
            }),
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(defs["count"], VarDef::int(0, 10, 3));
        assert_eq!(defs["name"], VarDef::string(5, "Bob".to_string()));
        assert_eq!(defs["plain"], VarDef::int(i32::MIN, i32::MAX, 0));
    }

    /// `str` defaults to [`DEFAULT_MAX_LENGTH`] and an empty value when omitted, and
    /// `int` defaults to the full 32-bit range.
    #[test]
    fn check_variables_applies_declaration_defaults() {
        let mut errors = Vec::new();
        let defs = check_variables(
            &json!({ "s": { "type": "str" }, "i": { "type": "int" } }),
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(defs["s"], VarDef::string(DEFAULT_MAX_LENGTH, String::new()));
        assert_eq!(defs["i"], VarDef::int(i32::MIN, i32::MAX, 0));
    }

    /// A non-object `variables` section is rejected as a whole.
    #[test]
    fn check_variables_rejects_non_object() {
        let mut errors = Vec::new();
        let defs = check_variables(&json!([]), &mut errors);
        assert!(defs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must be an object"));
    }

    /// Every malformed declaration shape contributes an error and is omitted.
    #[test]
    fn check_variables_reports_each_rejection() {
        let cases = [
            (json!({ "1a": { "type": "int" } }), "invalid variable name"),
            (json!({ "a": 5 }), "must be an object"),
            (json!({ "a": { "value": 1 } }), "\"type\" must be a string"),
            (json!({ "a": { "type": "bool" } }), "unknown type"),
            (json!({ "a": { "type": "int", "bogus": 1 } }), "unknown key"),
            (
                json!({ "a": { "type": "int", "max_length": 3 } }),
                "only valid for a \"str\"",
            ),
            (
                json!({ "a": { "type": "str", "min": 1 } }),
                "only valid for an \"int\"",
            ),
            (
                json!({ "a": { "type": "int", "min": 5, "max": 1 } }),
                "greater than",
            ),
            (
                json!({ "a": { "type": "int", "min": 0, "max": 10, "value": 11 } }),
                "outside the declared range",
            ),
            (
                json!({ "a": { "type": "int", "value": "x" } }),
                "\"value\" must be a 32-bit integer",
            ),
            (
                json!({ "a": { "type": "str", "value": 5 } }),
                "\"value\" must be a string",
            ),
            (
                json!({ "a": { "type": "str", "max_length": 0 } }),
                "between 1 and",
            ),
            (
                json!({ "a": { "type": "str", "max_length": 70000 } }),
                "between 1 and",
            ),
            (
                json!({ "a": { "type": "str", "max_length": 2, "value": "abc" } }),
                "longer than",
            ),
        ];
        for (value, needle) in cases {
            let mut errors = Vec::new();
            let defs = check_variables(&value, &mut errors);
            assert!(
                errors.iter().any(|error| error.contains(needle)),
                "expected an error containing {needle:?}, got {errors:?}"
            );
            assert!(defs.is_empty(), "rejected declaration leaked: {defs:?}");
        }
    }

    /// The store starts every declared variable at its initial value, looks declarations
    /// and values up by name, and `set` only touches declared variables.
    #[test]
    fn store_initialises_and_updates_declared_values() {
        let mut defs = BTreeMap::new();
        defs.insert("count".to_string(), VarDef::int(0, 10, 3));
        defs.insert("name".to_string(), VarDef::string(5, "Bob".to_string()));
        let mut store = VariableStore::new(defs);

        assert_eq!(store.get("count"), Some(&VarValue::Int(3)));
        assert_eq!(store.get("name"), Some(&VarValue::Str("Bob".to_string())));
        assert!(store.def("count").is_some());
        assert!(store.contains("name"));
        assert!(!store.contains("missing"));

        store.set("count", VarValue::Int(7));
        assert_eq!(store.get("count"), Some(&VarValue::Int(7)));
        store.set("missing", VarValue::Int(1));
        assert!(!store.contains("missing"));
    }

    /// `VarValue::to_text` stringifies integers decimally and returns strings verbatim.
    #[test]
    fn var_value_renders_for_substitution() {
        assert_eq!(VarValue::Int(-12).to_text(), "-12");
        assert_eq!(
            VarValue::Str("he said \"hi\"".to_string()).to_text(),
            "he said \"hi\""
        );
    }

    /// Builds a runtime state for expansion tests: an int `count`, a str `name` and the
    /// built-in [`Defaults`].
    fn test_variables() -> Variables {
        let mut defs = BTreeMap::new();
        defs.insert("count".to_string(), VarDef::int(0, 100, 7));
        defs.insert("name".to_string(), VarDef::string(50, "Bob".to_string()));
        Variables::new(defs, &Defaults::default())
    }

    /// A bare `$name` and the explicit `$var.name` form resolve identically, ints render
    /// decimally and `$defaults.*` reads return the current defaults values.
    #[test]
    fn expand_resolves_variables_and_defaults() {
        let variables = test_variables();
        assert_eq!(variables.expand("hello $name").unwrap(), "hello Bob");
        assert_eq!(variables.expand("$var.name").unwrap(), "Bob");
        assert_eq!(variables.expand("$count").unwrap(), "7");
        assert_eq!(
            variables.expand("$defaults.button_brightness").unwrap(),
            "50"
        );
        assert_eq!(
            variables.expand("$defaults.short_press_duration").unwrap(),
            "300"
        );
        // A dot after a non-scope name is literal text, not a scope separator.
        assert_eq!(variables.expand("$name.png").unwrap(), "Bob.png");
        assert_eq!(variables.expand("$var.name.png").unwrap(), "Bob.png");
        assert_eq!(variables.expand("no reference").unwrap(), "no reference");
    }

    /// Backslash escapes a literal `$`/`\`, terminates a reference name, and is otherwise
    /// left in place for the downstream command tokenizer.
    #[test]
    fn expand_handles_escapes_and_delimiters() {
        let variables = test_variables();
        assert_eq!(variables.expand(r"$name-kun").unwrap(), "Bob-kun");
        assert_eq!(variables.expand(r"$name\kun").unwrap(), "Bobkun");
        assert_eq!(variables.expand(r"\$name").unwrap(), "$name");
        assert_eq!(variables.expand(r"a\\b").unwrap(), r"a\b");
        assert_eq!(variables.expand(r"$count$count").unwrap(), "77");
        assert_eq!(
            variables.expand(r"/bin/echo a\ b").unwrap(),
            r"/bin/echo a\ b"
        );
    }

    /// Malformed or unresolvable references are rejected with a descriptive error.
    #[test]
    fn expand_rejects_bad_references() {
        let variables = test_variables();
        for (input, needle) in [
            ("$", "must be followed by a variable name"),
            ("$ x", "must be followed by a variable name"),
            ("$1abc", "must be followed by a variable name"),
            ("$var.", "must name a variable"),
            ("$env.HOME", "undefined variable"),
            ("$missing", "undefined variable"),
            ("$defaults.nope", "undefined variable"),
        ] {
            let error = variables.expand(input).unwrap_err();
            assert!(
                error.contains(needle),
                "expected {input:?} to fail with {needle:?}, got {error:?}"
            );
        }
    }

    /// `references_in` lists references in source order without resolving them, and
    /// ignores escaped ones.
    #[test]
    fn references_in_collects_unresolved_references() {
        assert_eq!(
            references_in("a $x then $var.y and $defaults.button_brightness").unwrap(),
            vec![
                VarRef {
                    scope: Scope::Var,
                    name: "x".to_string()
                },
                VarRef {
                    scope: Scope::Var,
                    name: "y".to_string()
                },
                VarRef {
                    scope: Scope::Defaults,
                    name: "button_brightness".to_string()
                },
            ]
        );
        assert!(references_in(r"\$x and a\\b").unwrap().is_empty());
        assert!(references_in("plain").unwrap().is_empty());
    }

    /// `kind_of` reports declared types (defaults are all ints) and `None` for unknown
    /// names.
    #[test]
    fn kind_of_reports_declared_types() {
        let variables = test_variables();
        assert_eq!(
            variables.kind_of(&VarRef {
                scope: Scope::Var,
                name: "count".to_string()
            }),
            Some(VarType::Int)
        );
        assert_eq!(
            variables.kind_of(&VarRef {
                scope: Scope::Var,
                name: "name".to_string()
            }),
            Some(VarType::Str)
        );
        assert_eq!(
            variables.kind_of(&VarRef {
                scope: Scope::Defaults,
                name: "button_brightness".to_string()
            }),
            Some(VarType::Int)
        );
        assert_eq!(
            variables.kind_of(&VarRef {
                scope: Scope::Var,
                name: "missing".to_string()
            }),
            None
        );
    }
}
