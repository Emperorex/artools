//! `--generate-man` prints the manual page in roff format (see
//! arcommon::man). The release workflow runs it to publish `ardisk.1`.

use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");

fn ardisk(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run ardisk")
}

fn page() -> String {
    let out = ardisk(&["--generate-man"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn it_is_a_manual_page_for_this_version() {
    let page = page();
    let title = concat!(
        ".TH ARDISK 1 \"\" \"ardisk ",
        env!("CARGO_PKG_VERSION"),
        "\" \"User Commands\"\n"
    );
    assert!(page.contains(title), "{page}");
    for section in [
        ".SH NAME",
        ".SH SYNOPSIS",
        ".SH OPTIONS",
        ".SH EXAMPLES",
        ".SH \"EXIT STATUS\"",
        ".SH \"SEE ALSO\"",
    ] {
        assert!(page.contains(section), "{section} is missing:\n{page}");
    }
}

#[test]
fn it_lists_the_options_but_not_the_hidden_ones() {
    let page = page();
    for flag in [
        "\\-\\-top",
        "\\-\\-largest\\-files",
        "\\-\\-json",
        "\\-\\-color",
    ] {
        assert!(page.contains(flag), "{flag} is missing");
    }
    assert!(!page.contains("generate"), "{page}");
}

#[test]
fn every_option_is_described() {
    // An option without a doc comment leaves an empty entry in the page:
    // its `.TP` header line would be followed directly by the next `.TP`.
    let page = page();
    let lines: Vec<&str> = page.lines().collect();
    for pair in lines.windows(3) {
        if pair[0] == ".TP" && pair[2] == ".TP" {
            panic!("no description for: {}", pair[1]);
        }
    }
}

#[test]
fn the_page_is_the_same_on_every_machine() {
    // The -j default depends on the CPU count; the page describes it.
    let page = page();
    assert!(page.contains("half of the available CPUs"), "{page}");
    assert!(!page.contains("JOBS\\fR [default"), "{page}");
}

#[test]
fn it_is_not_in_help_and_takes_nothing_else() {
    let help = ardisk(&["--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("generate-man"));
    let out = ardisk(&["--generate-man", "-s"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
