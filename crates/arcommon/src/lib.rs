//! Shared building blocks for the artools command-line tools (arfind,
//! ardisk, argrep).
//!
//! Each module covers one concern and has no knowledge of any particular
//! tool: functions return values or errors and leave printing, colours,
//! exit codes and the tool's name in messages to the caller. That keeps
//! every tool's user-visible behaviour (messages, exit statuses, CLI) its
//! own, while the logic underneath exists once.
//!
//! | Module        | What it provides                                              |
//! |---------------|---------------------------------------------------------------|
//! | [`term`]      | Escaping control characters before printing to a terminal     |
//! | [`walk`]      | The parallel work queue every tool's directory walk runs on    |
//! | [`ignore`]    | `.gitignore`/`.ignore` stacks, default ignored dirs, `--exclude` |
//! | [`jobs`]      | The `-j/--jobs` default and upper bound                        |
//! | [`age`]       | `--older-than` / `--newer-than` parsing and mtime filtering    |
//! | [`filetypes`] | The built-in `--type` table (`rust`, `python`, ...)            |
//! | [`fs`]        | Platform helpers such as the device id for `--one-file-system` |
//! | [`out`]       | `out!`/`outln!`: stdout writes that exit quietly on a closed pipe (the one module that ends the process) |

pub mod age;
pub mod filetypes;
pub mod fs;
pub mod ignore;
pub mod jobs;
pub mod out;
pub mod term;
pub mod walk;

// The terminal helpers predate the module split; keep them at the crate
// root too so existing `arcommon::display_path(...)` calls keep working.
pub use term::{display_path, sanitize_for_display, sanitize_name};
