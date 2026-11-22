//! A2UI's naming rule (the specification's "Catalog Entity Naming Rules"):
//! component names, function names, property names, and component instance ids
//! are Unicode identifiers in the sense of UAX #31.

use regex::Regex;
use std::sync::OnceLock;

/// Whether `name` is a UAX #31 identifier as A2UI defines one: an `XID_Start`
/// character or `_`, then `XID_Continue` characters, with no `Pattern_Syntax` or
/// `Pattern_White_Space` character other than `_`. (`XID_Continue` already
/// excludes both; the second check keeps the rule explicit, as the specification
/// states it.) A leading digit is refused because digits are not `XID_Start`.
pub fn is_identifier(name: &str) -> bool {
    static SHAPE: OnceLock<Regex> = OnceLock::new();
    static FORBIDDEN: OnceLock<Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        Regex::new(r"^[\p{XID_Start}_]\p{XID_Continue}*$").expect("identifier pattern")
    });
    let forbidden = FORBIDDEN.get_or_init(|| {
        Regex::new(r"[[\p{Pattern_Syntax}\p{Pattern_White_Space}]--[_]]").expect("symbol pattern")
    });
    shape.is_match(name) && !forbidden.is_match(name)
}

#[cfg(test)]
mod tests {
    use super::is_identifier;

    #[test]
    fn follows_the_specifications_examples() {
        // The specification's own examples: `user_card` and `userCard` are valid,
        // `user-card` and `1st_item` are not.
        assert!(is_identifier("user_card"));
        assert!(is_identifier("userCard"));
        assert!(!is_identifier("user-card"));
        assert!(!is_identifier("1st_item"));
    }

    #[test]
    fn accepts_unicode_letters_and_a_leading_underscore() {
        assert!(is_identifier("_private"));
        assert!(is_identifier("名前"));
        assert!(is_identifier("café2"));
    }

    #[test]
    fn refuses_empty_whitespace_and_symbols() {
        assert!(!is_identifier(""));
        assert!(!is_identifier("a b"));
        assert!(!is_identifier("a.b"));
        assert!(!is_identifier("@path"));
        assert!(!is_identifier("a$"));
    }
}
