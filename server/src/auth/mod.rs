//! Authentication: the single admin account, opaque bearer/cookie tokens, and
//! the policy that turns a presented credential into a [`Principal`].
//!
//! Mirrors the `memory` and `plugins` modules' store/service split and shares
//! the config store's `SqlitePool`.

mod store;
mod token;

pub use store::{AuthStore, TokenRow, UserRow};
pub use token::{GeneratedToken, Principal, TokenKind, generate, hash_secret, parse};
