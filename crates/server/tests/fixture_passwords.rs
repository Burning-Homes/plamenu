//! Fixture password salts, default Argon2 strength and exhaustion stay real.

use std::collections::HashSet;

use plamenu::auth::{hash_password, password_hash_count, password_verify_count, verify_password};
use plamenu_test_support::fixture_password_hash;

#[test]
fn fixture_passwords_keep_distinct_salts_and_current_parameters_after_reserve_exhaustion() {
    let before = password_hash_count();
    // Exceed the 64-hash reserve without recycling a prior salt.
    let hashes: Vec<_> = (0..66).map(|_| fixture_password_hash()).collect();
    assert_eq!(password_hash_count() - before, 2);
    let fresh = hash_password("fixture-password").unwrap();
    let parameters: Vec<_> = fresh.split('$').take(4).collect();
    assert_eq!(parameters[1], "argon2id");
    let mut salts = HashSet::new();
    let verifications = password_verify_count();
    for hash in &hashes {
        assert_eq!(hash.split('$').take(4).collect::<Vec<_>>(), parameters);
        assert!(salts.insert(hash.split('$').nth(4).unwrap()));
        assert!(verify_password("fixture-password", hash));
    }
    for index in [0, 63, 64, 65] {
        assert!(!verify_password("a different password", &hashes[index]));
    }
    assert_eq!(salts.len(), 66);
    assert_eq!(password_verify_count() - verifications, 70);
    let mut owned = fixture_password_hash();
    owned.clear();
    let next = fixture_password_hash();
    assert!(verify_password("fixture-password", &next));
    assert!(!hashes.contains(&next));
}
