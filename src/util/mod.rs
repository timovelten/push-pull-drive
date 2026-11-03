use std::fmt::{Debug, Display};

mod async_read;
mod shell;

pub use async_read::InspectAsyncReader;
pub use shell::*;

pub struct WithoutNewlines<S: AsRef<str>> {
    original: S,
    replace: &'static str,
}

impl<S: AsRef<str>> WithoutNewlines<S> {
    pub fn new(s: S) -> Self {
        Self::new_with_replacement_char(s, "⏎")
    }

    pub fn new_with_replacement_char(s: S, replace: &'static str) -> Self {
        Self {
            original: s,
            replace,
        }
    }
}

impl<S: AsRef<str>> Display for WithoutNewlines<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for line in self.original.as_ref().lines() {
            if first {
                first = false;
            } else {
                write!(f, "{}", self.replace)?;
            }

            write!(f, "{}", line)?;
        }

        Ok(())
    }
}

pub struct DebugBytes<'a>(pub &'a [u8]);

impl<'a> Debug for DebugBytes<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chars = self
            .0
            .iter()
            .copied()
            .flat_map(core::ascii::escape_default);

        write!(f, "\"")?;

        for c in chars {
            write!(f, "{}", c as char)?;
        }

        write!(f, "\"")
    }
}

pub fn human_readable_bytes(bs: u64) -> (f64, &'static str) {
    static UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let i = if bs == 0 {
        0
    } else {
        (bs.ilog2() as usize / 10).min(UNITS.len() - 1)
    };
    let magnitude = 1 << (10 * i);

    (bs as f64 / magnitude as f64, UNITS[i])
}

#[track_caller]
pub fn assert_equal_when_sorted<T: Ord + core::fmt::Debug + Clone>(left: &[T], right: &[T]) {
    let mut a = Vec::from(left);
    a.sort();

    let mut b = Vec::from(right);
    b.sort();

    assert_eq!(a, b)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_human_readable_bytes() {
        assert_eq!(human_readable_bytes(42), (42f64, "B"));
        assert_eq!(human_readable_bytes(1024), (1f64, "KiB"));
        assert_eq!(human_readable_bytes(5 * 1024 * 1024), (5f64, "MiB"));
        assert_eq!(
            human_readable_bytes(1024 * 1024 * 1024 + 42),
            (
                (1024.0 * 1024.0 * 1024.0 + 42.0) / (1024.0 * 1024.0 * 1024.0),
                "GiB"
            )
        );
    }

    #[test]
    fn test_without_newlines() {
        use std::fmt::Write;

        let x = "A text\n with at least\n three lines";
        let mut out = String::new();

        write!(out, "{}", WithoutNewlines::new_with_replacement_char(x, "")).unwrap();

        assert_eq!(out, "A text with at least three lines");
    }
}
