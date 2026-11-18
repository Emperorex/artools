//! The manual page of a tool, generated from its command-line definition.
//!
//! Every tool has a hidden `--generate-man` flag that prints its page in
//! roff format (`arfind --generate-man > arfind.1`). The options come from
//! the same clap definition as `--help`, so the page can't fall behind the
//! flags; the tool adds what clap doesn't know: examples, exit statuses and
//! related pages (see [`Page`]). The release workflow runs the flag once
//! and publishes the page next to the binaries.

use clap::Command;
use clap_mangen::Man;
use roff::{Roff, bold, roman};
use std::io::{self, Write};

/// Where the long-form documentation lives; named at the end of each page.
const HOMEPAGE: &str = "https://github.com/Emperorex/artools";

/// What a tool adds to the page generated from its options.
pub struct Page<'a> {
    /// `(what it does, command line)`, in the order they are shown.
    pub examples: &'a [(&'a str, &'a str)],
    /// `(status, meaning)`.
    pub exit_status: &'a [(&'a str, &'a str)],
    /// Related pages, e.g. `"find(1)"`.
    pub see_also: &'a [&'a str],
}

/// Writes the manual page of `cmd` to `w`, in roff format.
pub fn render(cmd: Command, page: &Page, w: &mut dyn Write) -> io::Result<()> {
    let cmd = prepare(cmd);
    let version = cmd.get_version().unwrap_or_default().to_string();
    let name = cmd.get_name().to_string();
    let man = Man::new(cmd);

    // The title line is written here: the page has no date (it would
    // differ from build to build), and a date left out rather than given
    // as "" shifts the version into its place.
    let preamble = Roff::new().render();
    let mut out = preamble.clone().into_bytes();
    writeln!(
        out,
        r#".TH {} 1 "" "{name} {version}" "User Commands""#,
        name.to_uppercase()
    )?;
    let mut section = |write: &dyn Fn(&mut Vec<u8>) -> io::Result<()>| -> io::Result<()> {
        let mut buf = Vec::new();
        write(&mut buf)?;
        out.extend_from_slice(buf.strip_prefix(preamble.as_bytes()).unwrap_or(&buf));
        Ok(())
    };
    section(&|w| man.render_name_section(w))?;
    section(&|w| man.render_synopsis_section(w))?;
    section(&|w| man.render_description_section(w))?;
    section(&|w| man.render_options_section(w))?;
    section(&|w| examples(page).to_writer(w))?;
    section(&|w| exit_status(page).to_writer(w))?;
    section(&|w| see_also(page).to_writer(w))?;
    section(&|w| man.render_version_section(w))?;
    section(&|w| man.render_authors_section(w))?;
    w.write_all(&out)
}

/// The command as the page should describe it.
fn prepare(mut cmd: Command) -> Command {
    // One line, as in `--help`, instead of every option spelled out.
    let usage = cmd.render_usage().to_string();
    let usage = usage.strip_prefix("Usage: ").unwrap_or(&usage).to_string();
    cmd = cmd.override_usage(usage);

    // The -j default depends on the machine the page is generated on, so
    // the page says how it is chosen instead of naming a number.
    let jobs_help = cmd
        .get_arguments()
        .find(|arg| arg.get_id() == "jobs")
        .and_then(|arg| arg.get_long_help().or_else(|| arg.get_help()))
        .map(|help| help.to_string());
    if let Some(help) = jobs_help {
        let max = crate::jobs::DEFAULT_JOBS_MAX;
        cmd = cmd.mut_arg("jobs", |arg| {
            arg.hide_default_value(true).long_help(format!(
                "{help}. The default is half of the available CPUs, at most {max}"
            ))
        });
    }
    cmd
}

fn examples(page: &Page) -> Roff {
    let mut roff = Roff::new();
    roff.control("SH", ["EXAMPLES"]);
    for (what, command) in page.examples {
        roff.control("TP", []);
        roff.text([roman(format!("{what}:"))]);
        roff.text([bold(*command)]);
    }
    roff
}

fn exit_status(page: &Page) -> Roff {
    let mut roff = Roff::new();
    roff.control("SH", ["EXIT STATUS"]);
    for (status, meaning) in page.exit_status {
        roff.control("TP", []);
        roff.text([bold(*status)]);
        roff.text([roman(*meaning)]);
    }
    roff
}

fn see_also(page: &Page) -> Roff {
    let mut roff = Roff::new();
    roff.control("SH", ["SEE ALSO"]);
    if !page.see_also.is_empty() {
        roff.text([roman(page.see_also.join(", "))]);
        roff.control("PP", []);
    }
    roff.text([roman(format!("Full documentation: {HOMEPAGE}"))]);
    roff
}

/// Prints the manual page of `cmd` on stdout.
pub fn print(cmd: Command, page: &Page) {
    let mut out = Vec::new();
    render(cmd, page, &mut out).expect("writing to memory cannot fail");
    crate::out!("{}", String::from_utf8_lossy(&out));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction};

    fn page_of(cmd: Command) -> String {
        let page = Page {
            examples: &[("List it", "tool -a .")],
            exit_status: &[("0", "Success"), ("1", "It's broken")],
            see_also: &["find(1)"],
        };
        let mut out = Vec::new();
        render(cmd, &page, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn tool() -> Command {
        Command::new("tool")
            .version("1.2.3")
            .author("Somebody")
            .about("Does a thing")
            .arg(
                Arg::new("all")
                    .short('a')
                    .long("all")
                    .action(ArgAction::SetTrue)
                    .help("Everything"),
            )
            .arg(
                Arg::new("secret")
                    .long("secret")
                    .hide(true)
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("jobs")
                    .short('j')
                    .long("jobs")
                    .default_value("7")
                    .help("Worker threads"),
            )
            .arg(Arg::new("path").value_name("PATH").num_args(0..))
    }

    #[test]
    fn the_page_has_every_section_in_order() {
        let page = page_of(tool());
        assert!(
            page.starts_with(concat!(
                ".ie \\n(.g .ds Aq \\(aq\n.el .ds Aq '\n",
                ".TH TOOL 1 \"\" \"tool 1.2.3\" \"User Commands\"\n"
            )),
            "{page}"
        );
        let mut from = 0;
        for title in [
            "NAME",
            "SYNOPSIS",
            "DESCRIPTION",
            "OPTIONS",
            "EXAMPLES",
            "EXIT STATUS",
            "SEE ALSO",
            "VERSION",
            "AUTHORS",
        ] {
            let heading = if title.contains(' ') {
                format!(".SH \"{title}\"\n")
            } else {
                format!(".SH {title}\n")
            };
            let at = page[from..]
                .find(&heading)
                .unwrap_or_else(|| panic!("{title}:\n{page}"));
            from += at;
        }
        // The preamble is written once, not before every section.
        assert_eq!(page.matches(".ds Aq").count(), 2, "{page}");
    }

    #[test]
    fn options_come_from_the_command_and_hidden_ones_stay_out() {
        let page = page_of(tool());
        assert!(page.contains("\\-\\-all"), "{page}");
        assert!(page.contains("Everything"), "{page}");
        assert!(!page.contains("secret"), "{page}");
    }

    #[test]
    fn the_synopsis_is_one_usage_line() {
        let page = page_of(tool());
        assert!(page.contains("[OPTIONS]"), "{page}");
    }

    #[test]
    fn the_jobs_default_is_described_not_numbered() {
        let page = page_of(tool());
        assert!(page.contains("half of the available CPUs"), "{page}");
        assert!(!page.contains("default: 7"), "{page}");
    }

    #[test]
    fn the_extra_sections_are_written() {
        let page = page_of(tool());
        assert!(page.contains("List it:"), "{page}");
        assert!(page.contains("tool \\-a ."), "{page}");
        assert!(page.contains("It\\*(Aqs broken"), "{page}");
        assert!(page.contains("find(1)"), "{page}");
        assert!(page.contains("tool 1.2.3"), "{page}");
    }
}
