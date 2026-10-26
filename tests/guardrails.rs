//! The guardrail scanners must match Python's patterns and results exactly.

mod support;

use agent86::guardrails::{
    redact, scan_injection, scan_pii, scan_secrets, Finding, INJECTION_PATTERNS, PII_PATTERNS,
    SECRET_PATTERNS,
};
use serde_json::Value;
use support::fixture;

fn golden() -> Value {
    serde_json::from_str(&fixture("guardrails/scanners.json")).unwrap()
}

fn summary(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|finding| format!("{}:{}", finding.category, finding.label))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn patterns_are_pythons_verbatim() {
    let golden = golden();
    for (name, ours, ignore_case) in [
        ("_INJECTION", INJECTION_PATTERNS, true),
        ("_SECRETS", SECRET_PATTERNS, false),
        ("_PII", PII_PATTERNS, false),
    ] {
        let theirs = golden["patterns"][name].as_array().unwrap();
        assert_eq!(ours.len(), theirs.len(), "{name}");
        for ((pattern, label), expected) in ours.iter().zip(theirs) {
            assert_eq!(
                *pattern,
                expected["pattern"].as_str().unwrap(),
                "{name} {label}"
            );
            assert_eq!(*label, expected["label"].as_str().unwrap(), "{name}");
            assert_eq!(
                ignore_case,
                expected["ignore_case"].as_bool().unwrap(),
                "{name} {label}"
            );
        }
    }
}

#[test]
fn scans_and_redactions_match_cpython() {
    for sample in golden()["samples"].as_array().unwrap() {
        let text = sample["text"].as_str().unwrap();
        assert_eq!(
            summary(&scan_injection(text)),
            sample["injection"],
            "injection: {text:?}"
        );
        assert_eq!(
            summary(&scan_secrets(text)),
            sample["secrets"],
            "secrets: {text:?}"
        );
        assert_eq!(summary(&scan_pii(text)), sample["pii"], "pii: {text:?}");
        let (redacted, findings) = redact(text);
        assert_eq!(
            redacted,
            sample["redacted"].as_str().unwrap(),
            "redacted: {text:?}"
        );
        assert_eq!(
            summary(&findings),
            sample["redact_summary"],
            "redact summary: {text:?}"
        );
    }
}
