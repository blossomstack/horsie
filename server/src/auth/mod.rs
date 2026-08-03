//! Authentication: the single admin account, opaque bearer/cookie tokens, and
//! the policy that turns a presented credential into a [`Principal`].
//!
//! Mirrors the `memory` and `plugins` modules' store/service split and shares
//! the config store's `SqlitePool`.

pub mod password;
mod service;
mod store;
mod throttle;
mod token;

pub use service::{
    ACCESS_TOKEN_TTL_SECS, ADMIN_USERNAME, AuthDeps, AuthService, DEVICE_CODE_TTL_SECS,
    DEVICE_POLL_INTERVAL_SECS, DeviceAuthorization, DeviceError, INITIAL_PASSWORD_FILE,
    IssuedTokens, LoginError, USER_CODE_ALPHABET, VerifiedToken,
};
pub use store::{AuthStore, DeviceCodeRow, RawTokenRow, TokenRow, UserRow};
pub use throttle::Throttle;
pub use token::{GeneratedToken, Principal, TokenKind, generate, hash_secret, parse};
