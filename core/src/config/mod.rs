//! Configuration parsing, validation and model.

pub mod lexer;
pub mod model;
pub mod syntax;
pub mod units;
pub mod validate;

pub use lexer::Pos;
pub use model::{
    BodyLimit, Config, ConfigError, ConfigErrorKind, Limits, OnFileChange, ProxyRoute, Route,
    RouteAction, StripPrefix, VirtualHost,
};
pub use units::{parse_duration, parse_size};
pub use validate::load;
