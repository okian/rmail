//! Rendering a value as a TOML basic string.
//!
//! Not a dependency on `toml`/`toml_edit`: the two surfaces that need this
//! append a handful of scalar key/value pairs to the operator's own config
//! file, and that is the entire serialization surface either of them has. What
//! matters is the escaping, which is a property of TOML's grammar and therefore
//! belongs beside the config type it is written for rather than in whichever
//! client happened to need it first — `rmail-cli`'s `hook_cli` had the only copy
//! until the TUI needed the same block, and two copies of an escaper is two
//! chances to disagree about what a quote does.

/// `value` as a TOML basic-string literal — quoted, with the minimal escaping
/// TOML's basic-string grammar requires.
///
/// Control characters are escaped as `\uXXXX` rather than passed through: TOML
/// forbids a literal control character inside a basic string, so a value
/// carrying one would produce a file that does not parse — and the caller would
/// have written it into the operator's config before finding out.
#[must_use]
pub fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
