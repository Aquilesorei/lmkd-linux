use std::io::{self, Write};

/// Print `msg` atomically to stdout via its lock.
pub fn locked_print(msg: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{msg}");
}

/// Print `msg` atomically to stderr via its lock.
pub fn locked_eprint(msg: &str) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "{msg}");
}

/// Thread-safe formatted print to stdout (no intermediate String allocation).
#[macro_export]
macro_rules! sync_print {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let mut out = ::std::io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
    }};
}

/// Thread-safe formatted print to stderr (no intermediate String allocation).
#[macro_export]
macro_rules! sync_eprint {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let mut out = ::std::io::stderr().lock();
        let _ = writeln!(out, $($arg)*);
    }};
}
