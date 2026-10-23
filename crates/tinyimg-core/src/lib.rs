//! Offline, quality-targeted image optimization with conservative file safety.
pub mod config;
pub mod imaging;
pub mod paths;
pub mod pipeline;
pub mod scanner;
pub mod writer;
pub use pipeline::*;
