//! `--color`: whether output is colored, decided separately for stdout and
//! stderr.
//!
//! Each stream gets its own answer because they often go to different
//! places. With `argrep foo . 2>err.log`, results go to a terminal but
//! errors go to a file; with `argrep foo . | less`, it is the other way
//! round. One shared decision either writes escape codes into `err.log` or
//! strips the color from errors shown on the terminal.
//!
//! The decision for each stream, first match wins (`anstream`'s rules):
//!
//! 1. `--color always` / `--color never`
//! 2. `NO_COLOR` set to anything but an empty string: no color
//! 3. `CLICOLOR_FORCE` set to anything but `0`: color
//! 4. `CLICOLOR=0`: no color
//! 5. Color when the stream is a terminal (and `TERM` is set and not
//!    `dumb`), otherwise none.
//!
//! How it is applied:
//! - **stdout** is styled with the `colored` crate, as before: [`init`]
//!   sets its global switch to the stdout decision.
//! - **stderr** messages are styled with [`ErrPaint`] (`.err_red()` and
//!   so on), which follows the stderr decision. `colored` must not be used for stderr, since it
//!   follows stdout.
//!
//! Machine-readable output (`-0`, `--json`) is never colored, whatever
//! `--color` says; that is up to the code writing it.

use std::fmt::{self, Display};
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};

use anstyle::{AnsiColor, Style};

/// The `--color WHEN` values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color a stream only when it is a terminal (honors `NO_COLOR`,
    /// `CLICOLOR` and `CLICOLOR_FORCE`).
    #[default]
    Auto,
    /// Always color, even into a pipe or file (e.g. for `less -R`).
    Always,
    /// Never color.
    Never,
}

/// The decision for each stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub stdout: bool,
    pub stderr: bool,
}

const UNSET: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

/// The stderr decision; [`UNSET`] until [`init`] or first use.
static STDERR: AtomicU8 = AtomicU8::new(UNSET);

/// Decides colors for both streams from `--color` and the environment,
/// and applies them: `colored` (stdout) and [`ErrPaint`] (stderr).
/// Called once by each tool's `main`, right after parsing arguments.
pub fn init(choice: ColorChoice) -> Colors {
    anstream::ColorChoice::write_global(match choice {
        ColorChoice::Auto => anstream::ColorChoice::Auto,
        ColorChoice::Always => anstream::ColorChoice::AlwaysAnsi,
        ColorChoice::Never => anstream::ColorChoice::Never,
    });
    let colors = Colors {
        stdout: wants_color(&io::stdout()),
        stderr: wants_color(&io::stderr()),
    };
    colored::control::set_override(colors.stdout);
    STDERR.store(if colors.stderr { ON } else { OFF }, Ordering::Relaxed);
    colors
}

/// Whether `stream` should be colored, by the rules in the module docs.
fn wants_color<S: anstream::stream::RawStream>(stream: &S) -> bool {
    anstream::AutoStream::choice(stream) != anstream::ColorChoice::Never
}

/// Whether stderr is colored. Before [`init`] (library code under test),
/// decided from the environment as for `--color auto`.
pub fn stderr_enabled() -> bool {
    match STDERR.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let on = wants_color(&io::stderr());
            STDERR.store(if on { ON } else { OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// A value to print on stderr, in a color that follows the stderr
/// decision (see [`ErrPaint`]).
#[derive(Debug, Clone, Copy)]
pub struct Painted<T> {
    value: T,
    style: Style,
}

impl<T> Painted<T> {
    /// The same color, bold.
    pub fn bold(mut self) -> Self {
        self.style = self.style.bold();
        self
    }
}

impl<T: Display> Display for Painted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if stderr_enabled() {
            write!(
                f,
                "{}{}{}",
                self.style.render(),
                self.value,
                self.style.render_reset()
            )
        } else {
            self.value.fmt(f)
        }
    }
}

/// Colors for stderr messages, following the stderr decision:
/// `eprintln!("{}", format!("error: {e}").err_red())`. Use these, not
/// `colored`'s `.red()` (which follows stdout), for anything written to
/// stderr. Chain [`Painted::bold`] for bold.
pub trait ErrPaint: Sized {
    /// Errors.
    fn err_red(self) -> Painted<Self> {
        paint(self, AnsiColor::Red)
    }

    /// Warnings, and `--stats` headings.
    fn err_yellow(self) -> Painted<Self> {
        paint(self, AnsiColor::Yellow)
    }

    /// `--stats` totals.
    fn err_green(self) -> Painted<Self> {
        paint(self, AnsiColor::Green)
    }

    /// `--stats` values.
    fn err_cyan(self) -> Painted<Self> {
        paint(self, AnsiColor::Cyan)
    }
}

impl<T: Display> ErrPaint for T {}

fn paint<T>(value: T, color: AnsiColor) -> Painted<T> {
    Painted {
        value,
        style: Style::new().fg_color(Some(color.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn painted_follows_the_stderr_decision() {
        STDERR.store(ON, Ordering::Relaxed);
        assert_eq!("x".err_red().to_string(), "\x1b[31mx\x1b[0m");
        assert_eq!(
            "x".err_yellow().bold().to_string(),
            "\x1b[1m\x1b[33mx\x1b[0m"
        );
        assert_eq!(
            format!("e: {}", 1).err_cyan().to_string(),
            "\x1b[36me: 1\x1b[0m"
        );
        STDERR.store(OFF, Ordering::Relaxed);
        assert_eq!("x".err_red().to_string(), "x");
        assert_eq!(42.err_green().bold().to_string(), "42");
    }
}
