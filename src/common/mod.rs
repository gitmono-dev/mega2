pub mod canonical_json;
pub mod errors;
pub mod utils;

pub type MonoError = errors::MegaError;
pub type MonoResult<T> = Result<T, errors::MegaError>;
