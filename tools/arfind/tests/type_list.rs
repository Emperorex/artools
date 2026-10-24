//! End-to-end tests for --type-list.

use arcommon::filetypes::{TYPE_TABLE, type_list};
use std::process::{Command, Output};

fn arfind(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arfind"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

#[test]
fn prints_every_type_with_its_globs_and_exits_0() {
    let out = arfind(&["--type-list"]);
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Exactly the shared list argrep prints, in table order.
    assert_eq!(stdout, type_list());
    assert_eq!(stdout.lines().count(), TYPE_TABLE.len());
    assert!(stdout.starts_with("rust: *.rs\n"), "{stdout}");
    assert!(stdout.contains("\npython: *.py, *.pyi\n"), "{stdout}");
}

#[test]
fn does_not_search() {
    // Run from an empty directory: any search output would show up here.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("x.rs"), "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_arfind"))
        .current_dir(dir.path())
        .arg("--type-list")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains("x.rs"), "{stdout}");
}

#[test]
fn refuses_a_path_or_other_options() {
    for args in [
        &["--type-list", "."][..],
        &[".", "--type-list"],
        &["--type-list", "--type", "rust"],
        &["--type-list", "-0"],
    ] {
        let out = arfind(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn unknown_type_points_to_type_list() {
    let out = arfind(&[".", "--type", "cobol"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown type 'cobol'"), "{stderr}");
    assert!(stderr.contains("arfind --type-list"), "{stderr}");
}

/// The README's type table is written by hand; keep it in step with the
/// table the tools actually use.
#[test]
fn readme_type_table_matches_the_built_in_types() {
    let readme = include_str!("../README.md");
    let section = readme
        .split("## File types")
        .nth(1)
        .expect("README has a File types section");
    let rows: Vec<(String, String)> = section
        .lines()
        .skip_while(|l| !l.starts_with("| Type"))
        .skip(2) // header and separator
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (cells[1].replace('`', ""), cells[2].replace('`', ""))
        })
        .collect();
    let expected: Vec<(String, String)> = TYPE_TABLE
        .iter()
        .map(|(name, globs)| (name.to_string(), globs.join(", ")))
        .collect();
    assert_eq!(rows, expected, "update the table in tools/arfind/README.md");
}
