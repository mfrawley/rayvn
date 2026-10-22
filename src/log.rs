//! Progress lines for work that may run side by side. When several modules build at once each of its lines starts
//! with `[module]`, so the interleaved output stays readable; otherwise lines are printed as written. The prefix
//! belongs to the thread running the module, so code just calls [`say!`] where it used to call `eprintln!`.

use std::cell::RefCell;
use std::io::Write;

thread_local! {
    static PREFIX: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// `text` as printed behind `prefix`: one prefixed line per line of text. Behind a prefix the indentation of a
/// progress line is dropped (`[core] compiling`), unless `keep_indent` (output of other programs, where it matters).
fn render_with(prefix: Option<&str>, text: &str, keep_indent: bool) -> String {
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    lines
        .into_iter()
        .map(|l| match prefix {
            Some(p) if keep_indent => format!("{p}{l}\n"),
            Some(p) => format!("{p}{}\n", l.trim_start()),
            None => format!("{l}\n"),
        })
        .collect()
}

#[cfg(test)]
fn render(prefix: Option<&str>, text: &str) -> String {
    render_with(prefix, text, false)
}

fn emit(text: &str, keep_indent: bool) {
    let out = PREFIX.with(|p| render_with(p.borrow().as_deref(), text, keep_indent));
    let _ = std::io::stderr().lock().write_all(out.as_bytes());
}

/// Prints `text` to stderr, each line in one write so that lines of different modules never mix.
pub fn line(text: &str) {
    emit(text, false);
}

/// Like [`line`] for output of another program (javac): indentation is kept, so its carets still line up, and
/// no output prints nothing.
pub fn verbatim(text: &str) {
    if !text.trim().is_empty() {
        emit(text, true);
    }
}

/// The prefix of the current thread, for code that hands output to other threads.
pub fn prefix() -> Option<String> {
    PREFIX.with(|p| p.borrow().clone())
}

/// Runs `f` with `prefix` on this thread's lines.
pub fn with_prefix<R>(prefix: Option<String>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PREFIX.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PREFIX.with(|p| p.replace(prefix)));
    f()
}

macro_rules! say {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}
pub(crate) use say;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_prefix_lines_are_printed_as_written() {
        assert_eq!(
            render(None, "  compiling 3 sources"),
            "  compiling 3 sources\n"
        );
        assert_eq!(render(None, "a\n  b"), "a\n  b\n");
        assert_eq!(render(None, ""), "\n");
    }

    #[test]
    fn with_a_prefix_every_line_carries_it_and_loses_its_indent() {
        assert_eq!(
            render(Some("[core] "), "  compiling\nNote: x"),
            "[core] compiling\n[core] Note: x\n"
        );
        assert_eq!(render(Some("[core] "), ""), "[core] \n");
    }

    #[test]
    fn output_of_other_programs_keeps_its_indentation() {
        assert_eq!(
            render_with(Some("[core] "), "A.java:3: error\n    int x;\n    ^", true),
            "[core] A.java:3: error\n[core]     int x;\n[core]     ^\n"
        );
    }

    #[test]
    fn the_prefix_is_per_thread_and_restored_afterwards() {
        assert_eq!(prefix(), None);
        with_prefix(Some("[a] ".into()), || {
            assert_eq!(prefix().as_deref(), Some("[a] "));
            with_prefix(Some("[b] ".into()), || {
                assert_eq!(prefix().as_deref(), Some("[b] "))
            });
            assert_eq!(prefix().as_deref(), Some("[a] "));
            std::thread::spawn(|| assert_eq!(prefix(), None))
                .join()
                .unwrap();
        });
        assert_eq!(prefix(), None);
    }
}
