//! Opt-in check that the Rust keyring lookup reads entries written by Python's `keyring`.
//!
//! Ignored by default because it needs the real OS keyring. To run it, write two
//! throwaway entries with Python, run the test, then delete them:
//!
//! ```text
//! python -c "import keyring; [keyring.set_password('agent86-rs-keyring-test', a, v) for a, v in (('first', 'one'), ('second', 'two'))]"
//! cargo test --test os_keyring -- --ignored
//! python -c "import keyring; [keyring.delete_password('agent86-rs-keyring-test', a) for a in ('first', 'second')]"
//! ```
//!
//! Writing two accounts exercises Python's Windows layout, where the second
//! write moves the first account to `first@agent86-rs-keyring-test`.

use agent86::secrets::{OsKeyring, SecretStore};

const SERVICE: &str = "agent86-rs-keyring-test";

#[test]
#[ignore = "needs entries written to the real OS keyring by Python's keyring package"]
fn entries_written_by_python_keyring_are_readable() {
    let store = OsKeyring;
    assert_eq!(store.get_password(SERVICE, "first").as_deref(), Some("one"));
    assert_eq!(
        store.get_password(SERVICE, "second").as_deref(),
        Some("two")
    );
    assert_eq!(store.get_password(SERVICE, "third"), None);
}
