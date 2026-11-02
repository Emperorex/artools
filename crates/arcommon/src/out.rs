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
//!
//! # Buffering
//!
//! Rust's stdout flushes at every newline, even into a pipe or a file, so
//! printing many lines costs one `write` syscall each (argrep: 383,273 of
//! them for one dense search, against 1,066 for ripgrep). A tool can call
//! [`enable_buffering`] when line-by-line output is not needed, typically
//! when stdout is not a terminal: [`Out`] (and so `out!`/`outln!`) then
//! collects output in a 64 KiB buffer. The tool must then call [`flush`]
//! before it exits, since a buffer in a static is never dropped and
//! `process::exit` does not flush. Tools that never enable it write
//! exactly as before.

use std::{
    io::{self, BufWriter, Write},
    sync::{Mutex, PoisonError},
};

/// The block buffer, once [`enable_buffering`] was called.
static BUFFER: Mutex<Option<BufWriter<io::Stdout>>> = Mutex::new(None);

/// Size of the block buffer.
const BUFFER_SIZE: usize = 64 * 1024;

/// Collect stdout output in a buffer instead of writing every line at once.
/// The caller must [`flush`] before exiting.
pub fn enable_buffering() {
    let mut buffer = BUFFER.lock().unwrap_or_else(PoisonError::into_inner);
    if buffer.is_none() {
        *buffer = Some(BufWriter::with_capacity(BUFFER_SIZE, io::stdout()));
    }
}

/// Whether [`enable_buffering`] is in effect.
pub fn is_buffered() -> bool {
    BUFFER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

/// Writes out anything buffered, with the policy above for errors.
pub fn flush() {
    check(Out.flush());
}

/// Stdout as the tools write it: through the block buffer when it is
/// enabled, otherwise straight to the (line-buffered) standard stdout.
/// Each `write_fmt` call (one `out!`/`outln!`) holds the lock once, so a
/// line is never interleaved with another thread's.
pub struct Out;

impl Out {
    fn with<R>(f: impl FnOnce(&mut dyn Write) -> io::Result<R>) -> io::Result<R> {
        let mut buffer = BUFFER.lock().unwrap_or_else(PoisonError::into_inner);
        match buffer.as_mut() {
            Some(w) => f(w),
            None => f(&mut io::stdout().lock()),
        }
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Out::with(|w| w.write(buf))
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Out::with(|w| w.write_all(buf))
    }

    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        Out::with(|w| w.write_fmt(args))
    }

    fn flush(&mut self) -> io::Result<()> {
        Out::with(|w| w.flush())
    }
}

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
            ::std::write!($crate::out::Out, $($arg)*)
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
            ::std::writeln!($crate::out::Out)
        })
    };
    ($($arg:tt)*) => {
        $crate::out::check({
            use ::std::io::Write as _;
            ::std::writeln!($crate::out::Out, $($arg)*)
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
