//! `--color auto|always|never`, decided separately for stdout and stderr
//! (see arcommon::color). The terminal cases run the tool on a
//! pseudo-terminal, since a test's own streams are pipes.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");
const ESC: u8 = 0x1b;

/// arfind in `dir` with a clean color environment, plus `env`.
fn command(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("TERM", "xterm-256color");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

fn arfind(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    command(dir, args, env)
        .output()
        .expect("failed to run arfind")
}

fn colored(bytes: &[u8]) -> bool {
    bytes.contains(&ESC)
}

/// A directory, so stdout has a colored (bold blue) entry.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/f"), "").unwrap();
    dir
}

/// Results on stdout, and an invalid glob reported on stderr.
const LIST: &[&str] = &["d"];
const BAD_GLOB: &[&str] = &["d", "--exclude-dir", "["];

#[test]
fn auto_is_plain_into_pipes() {
    let d = tree();
    assert!(!colored(&arfind(d.path(), LIST, &[]).stdout));
    assert!(!colored(&arfind(d.path(), BAD_GLOB, &[]).stderr));
}

#[test]
fn always_colors_both_streams_into_pipes() {
    let d = tree();
    assert!(colored(
        &arfind(d.path(), &["d", "--color", "always"], &[]).stdout
    ));
    let out = arfind(
        d.path(),
        &["d", "--exclude-dir", "[", "--color=always"],
        &[],
    );
    assert!(
        colored(&out.stderr),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn never_wins_over_clicolor_force() {
    let d = tree();
    let force = [("CLICOLOR_FORCE", "1")];
    assert!(colored(&arfind(d.path(), LIST, &force).stdout));
    assert!(!colored(
        &arfind(d.path(), &["d", "--color", "never"], &force).stdout
    ));
    let out = arfind(
        d.path(),
        &["d", "--exclude-dir", "[", "--color", "never"],
        &force,
    );
    assert!(!colored(&out.stderr));
}

#[test]
fn always_wins_over_no_color() {
    let d = tree();
    let out = arfind(d.path(), &["d", "--colour", "always"], &[("NO_COLOR", "1")]);
    assert!(colored(&out.stdout));
}

#[test]
fn nul_lists_are_never_colored() {
    let d = tree();
    let out = arfind(d.path(), &["d", "-0", "--color", "always"], &[]);
    assert!(out.status.success());
    assert!(!colored(&out.stdout));
}

#[cfg(unix)]
mod terminal {
    use super::*;
    use std::{
        fs::File,
        io::{ErrorKind, Read},
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        process::Stdio,
    };

    /// Which of the child's streams goes to the terminal.
    #[derive(Clone, Copy)]
    enum On {
        Stdout,
        Stderr,
    }

    /// Runs `cmd` with one stream on a pseudo-terminal and the other piped.
    /// Returns what was written to the terminal, and the process output
    /// (whose piped stream is the other one).
    fn run_on_terminal(mut cmd: Command, on: On) -> (Vec<u8>, Output) {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills in two new descriptors that we then own.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let child_end = Stdio::from(slave.try_clone().unwrap());
        match on {
            On::Stdout => cmd.stdout(child_end).stderr(Stdio::piped()),
            On::Stderr => cmd.stdout(Stdio::piped()).stderr(child_end),
        };
        let out = cmd.output().expect("failed to run arfind");

        // Read only now, with our own `slave` still open: macOS discards
        // unread terminal output once the last slave descriptor closes,
        // so reading after the child exits would find nothing. Holding
        // it open keeps the output, and non-blocking reads end at
        // WouldBlock once everything written has been read.
        // SAFETY: plain fcntl calls on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        let mut file = File::from(master);
        let mut written = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => written.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("reading the terminal: {e}"),
            }
        }
        drop(slave);
        (written, out)
    }

    #[test]
    fn stderr_into_a_file_gets_no_codes_while_stdout_is_a_terminal() {
        // The original bug: `arfind ... 2>err.log` on a terminal wrote
        // color codes into err.log, because stderr followed stdout.
        let d = tree();
        let (_, out) = run_on_terminal(command(d.path(), BAD_GLOB, &[]), On::Stdout);
        assert!(!out.stderr.is_empty());
        assert!(
            !colored(&out.stderr),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        // ...while results on that terminal are colored.
        let (tty, _) = run_on_terminal(command(d.path(), LIST, &[]), On::Stdout);
        assert!(colored(&tty), "{}", String::from_utf8_lossy(&tty));
    }

    #[test]
    fn stderr_on_a_terminal_is_colored_while_stdout_is_a_pipe() {
        let d = tree();
        let (tty, _) = run_on_terminal(command(d.path(), BAD_GLOB, &[]), On::Stderr);
        assert!(colored(&tty), "{}", String::from_utf8_lossy(&tty));

        // ...and results into the pipe are plain.
        let (_, out) = run_on_terminal(command(d.path(), LIST, &[]), On::Stderr);
        assert!(!out.stdout.is_empty());
        assert!(!colored(&out.stdout));
    }

    #[test]
    fn never_turns_off_a_terminal() {
        let d = tree();
        let (tty, _) = run_on_terminal(
            command(d.path(), &["d", "--color", "never"], &[]),
            On::Stdout,
        );
        assert!(!tty.is_empty());
        assert!(!colored(&tty), "{}", String::from_utf8_lossy(&tty));
    }
}
