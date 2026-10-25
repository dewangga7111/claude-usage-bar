//! macOS: a menu bar item whose dropdown menu holds the details.

mod app;
mod sys;

pub use app::run;
pub use sys::{http_get, keychain_credentials};
