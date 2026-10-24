//! Immutable RSA fixtures for integration-test actors.

use std::sync::atomic::{AtomicUsize, Ordering};

use plamenu_ap::keys::{self, KeyPairPem};

#[path = "rsa_fixture_data.rs"]
mod data;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Issue each pre-generated RSA-2048 pair once, then generate fresh pairs.
/// Both local helpers and remote actors share this allocator, so even actors
/// with the same name receive independent keys. Never recycle an exhausted
/// reserve: large fixtures must retain their distinct identities too.
pub(super) fn rsa_keypair() -> KeyPairPem {
    match NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |index| {
        (index < data::PAIRS.len()).then_some(index + 1)
    }) {
        Ok(index) => {
            let (private_pem, public_pem) = data::PAIRS[index];
            KeyPairPem {
                private_pem: private_pem.to_owned(),
                public_pem: public_pem.to_owned(),
            }
        }
        Err(_) => keys::generate_keypair().unwrap(),
    }
}
