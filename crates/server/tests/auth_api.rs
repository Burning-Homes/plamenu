//! Authentication tests share one default binary; individual targets remain selectable.

#[path = "invites.rs"]
mod invites;
#[path = "key_storage.rs"]
mod key_storage;
#[path = "multi_account.rs"]
mod multi_account;
#[path = "oauth_scopes.rs"]
mod oauth_scopes;
#[path = "oauth_token_admission.rs"]
mod oauth_token_admission;
#[path = "password_reset.rs"]
mod password_reset;
#[path = "registration.rs"]
mod registration;
