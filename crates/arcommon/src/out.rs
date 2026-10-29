//! Writing to stdout when the reader may go away.
//!
//! `print!`/`println!` panic when stdout fails, so `argrep TODO . | head`
//! ends in a Rust backtrace once `head` has read its line and closed the
//! pipe. The [`out!`](crate::out!) and [`outln!`](crate::outln!) macros
//! write the same way, but hand any error to [`check`]:
//!
//! - **Broken pipe**: the reader is gone, so there is nobody left to tell
//!   anything. The process exits at once with status 0, quietly, as ripgrep
//!   does: `cmd | head` is a normal way to stop a search early. Exiting
//!   right away also stops the walk instead of searching the rest of the
//!   tree for nobody.
//! - **Any other error** (disk full, I/O error on a redirected file): output
//!   was lost, so it is reported on stderr and the process exits with 1.
//!
//! This is the one place in arcommon that ends the process itself: the
//! policy is the same for every tool and every output path, including
//! writes from worker or printer threads, where returning an error would
//! not stop the search.

use std::io;

/// Applies the policy above to the result of a write to stdout.
pub fn check(result: io::Result<()>) {
    if let Err(err) = result {
        exit_on_write_error(&err);
    }
}

/// Exits after a failed write to stdout: quietly with 0 for a broken pipe,
/// otherwise with a message and 1.
pub fn exit_on_write_error(err: &io::Error) -> ! {
    if err.kind() == io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("error: cannot write output: {err}");
    std::process::exit(1);
}

/// Like `print!`, but exits instead of panicking when stdout fails (see
/// [`check`]).
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::check({
            use ::std::io::Write as _;
            ::std::write!(::std::io::stdout().lock(), $($arg)*)
        })
    };
}

/// Like `println!`, but exits instead of panicking when stdout fails (see
/// [`check`]).
#[macro_export]
macro_rules! outln {
    () => {
        $crate::out::check({
            use ::std::io::Write as _;
            ::std::writeln!(::std::io::stdout().lock())
        })
    };
    ($($arg:tt)*) => {
        $crate::out::check({
            use ::std::io::Write as _;
            ::std::writeln!(::std::io::stdout().lock(), $($arg)*)
        })
    };
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn success_passes_through() {
        check(Ok(()));
        // The macros expand and write in every form.
        crate::out!("");
        crate::outln!();
        crate::outln!("{} {}", "a", 1);
    }
}
