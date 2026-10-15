macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), "tamotsu: {}", format_args!($($arg)*));
    }};
}

pub(crate) use note;
