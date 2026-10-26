//! The `difflib` port must reproduce CPython's diffs exactly.

mod support;

use agent86::difflib::{splitlines, unified_diff};
use serde_json::Value;
use support::fixture;

fn golden() -> Value {
    serde_json::from_str(&fixture("tools/write_tools.json")).unwrap()
}

#[test]
fn unified_diffs_match_cpython() {
    let golden = golden();
    for case in golden["diffs"].as_array().unwrap() {
        let old = splitlines(case["old"].as_str().unwrap());
        let new = splitlines(case["new"].as_str().unwrap());
        let diff = unified_diff(&old, &new, "a/f.txt", "b/f.txt", 3);
        assert_eq!(
            diff,
            case["diff"].as_str().unwrap(),
            "case {}",
            case["name"]
        );
    }
}

#[test]
fn splitlines_matches_cpython() {
    let golden = golden();
    let text = golden["splitlines"]["text"].as_str().unwrap();
    let expected: Vec<&str> = golden["splitlines"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    assert_eq!(splitlines(text), expected);
    assert!(splitlines("").is_empty());
    assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
}
