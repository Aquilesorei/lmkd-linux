use std::io::{self, Write};

/// Print a formatted line atomically via stdout lock.
#[macro_export]
macro_rules! sync_print {
    ($($arg:tt)*) => {{
        $crate::output::locked_print(&format!($($arg)*));
    }};
}

pub fn locked_print(msg: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{msg}");
}

pub fn locked_eprint(msg: &str) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "{msg}");
}
