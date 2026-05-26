use std::io;

#[derive(Debug)]
pub enum MgdError {
    Io(io::Error),
    Parse(String),
}

impl From<io::Error> for MgdError {
    fn from(e: io::Error) -> Self { MgdError::Io(e) }
}

impl From<std::num::ParseFloatError> for MgdError {
    fn from(e: std::num::ParseFloatError) -> Self { MgdError::Parse(e.to_string()) }
}

impl std::fmt::Display for MgdError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            MgdError::Io(e) => write!(f, "{e}"),
            MgdError::Parse(s) => write!(f, "parse: {s}"),
        }
    }
}
