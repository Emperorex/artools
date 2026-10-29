//! `--owner` / `--group`: match entries by owning user and group, like
//! `find -user` / `-group`. Unix only.
//!
//! Each value is a name or a numeric id, optionally negated with a leading
//! `!`. Names are resolved once, when the command line is parsed, so the
//! search itself only compares numbers. Like GNU find, a value made of
//! digits is a name if such a user (group) exists, otherwise an id: `1000`
//! normally means uid 1000.
//!
//! Within one flag, plain values are OR'ed and `!` values exclude: an entry
//! matches `--owner alice --owner bob` if alice or bob owns it, and
//! `--owner '!root'` if root does not. `--owner` and `--group` are AND'ed.

/// One `--owner` / `--group` value, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdSpec {
    pub id: u32,
    /// A leading `!`: match entries *not* owned by `id`.
    pub negated: bool,
}

/// Which database a value is looked up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    User,
    Group,
}

impl IdKind {
    fn noun(self) -> &'static str {
        match self {
            IdKind::User => "user",
            IdKind::Group => "group",
        }
    }
}

/// Parses `NAME`, `ID`, `!NAME` or `!ID`; `lookup` resolves a name to an id.
/// Split from the clap parsers so it can be tested without real accounts.
pub fn parse_spec(
    s: &str,
    kind: IdKind,
    lookup: impl Fn(&str) -> Result<Option<u32>, String>,
) -> Result<IdSpec, String> {
    let (negated, name) = match s.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if name.is_empty() {
        return Err(format!("empty {} name", kind.noun()));
    }
    let id = match lookup(name)? {
        Some(id) => id,
        None => match name.parse::<u32>() {
            Ok(id) if name.bytes().all(|b| b.is_ascii_digit()) => id,
            _ => return Err(format!("unknown {} '{name}'", kind.noun())),
        },
    };
    Ok(IdSpec { id, negated })
}

/// clap value parser for `--owner`.
pub fn owner_parser(s: &str) -> Result<IdSpec, String> {
    parse_spec(s, IdKind::User, |name| lookup(name, IdKind::User))
}

/// clap value parser for `--group`.
pub fn group_parser(s: &str) -> Result<IdSpec, String> {
    parse_spec(s, IdKind::Group, |name| lookup(name, IdKind::Group))
}

/// Looks `name` up in the user or group database (`getpwnam_r` /
/// `getgrnam_r`, so NSS sources such as LDAP count too). `Ok(None)` if there
/// is no such entry.
#[cfg(unix)]
fn lookup(name: &str, kind: IdKind) -> Result<Option<u32>, String> {
    use std::{ffi::CString, mem::MaybeUninit, ptr};

    let cname = CString::new(name).map_err(|_| format!("invalid {} name", kind.noun()))?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let (rc, found, id) = match kind {
            IdKind::User => {
                let mut pwd = MaybeUninit::<libc::passwd>::uninit();
                let mut result: *mut libc::passwd = ptr::null_mut();
                // SAFETY: every pointer is valid for the call; `buf` is
                // writable for `buf.len()` bytes; `result` is either null or
                // points at `pwd`, which getpwnam_r has then filled in.
                let rc = unsafe {
                    libc::getpwnam_r(
                        cname.as_ptr(),
                        pwd.as_mut_ptr(),
                        buf.as_mut_ptr(),
                        buf.len(),
                        &mut result,
                    )
                };
                let id = (!result.is_null()).then(|| unsafe { (*result).pw_uid });
                (rc, !result.is_null(), id)
            }
            IdKind::Group => {
                let mut grp = MaybeUninit::<libc::group>::uninit();
                let mut result: *mut libc::group = ptr::null_mut();
                // SAFETY: as above, for getgrnam_r.
                let rc = unsafe {
                    libc::getgrnam_r(
                        cname.as_ptr(),
                        grp.as_mut_ptr(),
                        buf.as_mut_ptr(),
                        buf.len(),
                        &mut result,
                    )
                };
                let id = (!result.is_null()).then(|| unsafe { (*result).gr_gid });
                (rc, !result.is_null(), id)
            }
        };
        match rc {
            0 if found => return Ok(id),
            0 => return Ok(None),
            // The entry did not fit (large groups list many members): retry
            // with a bigger buffer, up to a sane bound.
            libc::ERANGE if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            // Some systems report "not found" as an errno instead of 0.
            libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM => return Ok(None),
            err => {
                return Err(format!(
                    "cannot look up {} '{name}': {}",
                    kind.noun(),
                    std::io::Error::from_raw_os_error(err)
                ));
            }
        }
    }
}

#[cfg(not(unix))]
fn lookup(_name: &str, kind: IdKind) -> Result<Option<u32>, String> {
    Err(format!(
        "--{} is only supported on Unix",
        match kind {
            IdKind::User => "owner",
            IdKind::Group => "group",
        }
    ))
}

/// The ids one flag accepts and rejects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdFilter {
    include: Vec<u32>,
    exclude: Vec<u32>,
}

impl IdFilter {
    pub fn new(specs: &[IdSpec]) -> Self {
        let mut f = IdFilter::default();
        for s in specs {
            if s.negated {
                f.exclude.push(s.id);
            } else {
                f.include.push(s.id);
            }
        }
        f
    }

    pub fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    /// Any plain value matches (OR), and no `!` value does.
    pub fn matches(&self, id: u32) -> bool {
        (self.include.is_empty() || self.include.contains(&id)) && !self.exclude.contains(&id)
    }
}

/// `--owner` and `--group` together (AND'ed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ownership {
    pub owner: IdFilter,
    pub group: IdFilter,
}

impl Ownership {
    pub fn new(owners: &[IdSpec], groups: &[IdSpec]) -> Self {
        Ownership {
            owner: IdFilter::new(owners),
            group: IdFilter::new(groups),
        }
    }

    /// Whether any filter is set, i.e. whether metadata is needed.
    pub fn is_active(&self) -> bool {
        self.owner.is_active() || self.group.is_active()
    }

    /// Whether an entry with this owner and group passes.
    pub fn matches_ids(&self, uid: u32, gid: u32) -> bool {
        self.owner.matches(uid) && self.group.matches(gid)
    }

    /// Whether an entry passes, given its own metadata (not a symlink's
    /// target, as with --size). An entry whose metadata could not be read
    /// does not match an active filter.
    pub fn matches(&self, metadata: Option<&std::fs::Metadata>) -> bool {
        if !self.is_active() {
            return true;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.is_some_and(|m| self.matches_ids(m.uid(), m.gid()))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(name: &str) -> Result<Option<u32>, String> {
        Ok(match name {
            "root" => Some(0),
            "alice" => Some(1000),
            // A user whose name is a number, as GNU find has to handle.
            "123" => Some(4242),
            _ => None,
        })
    }

    fn parse(s: &str) -> Result<IdSpec, String> {
        parse_spec(s, IdKind::User, users)
    }

    #[test]
    fn names_ids_and_negation() {
        assert_eq!(
            parse("alice"),
            Ok(IdSpec {
                id: 1000,
                negated: false
            })
        );
        assert_eq!(
            parse("0"),
            Ok(IdSpec {
                id: 0,
                negated: false
            })
        );
        assert_eq!(
            parse("!root"),
            Ok(IdSpec {
                id: 0,
                negated: true
            })
        );
        assert_eq!(
            parse("!501"),
            Ok(IdSpec {
                id: 501,
                negated: true
            })
        );
        // An existing name wins over the number it spells, like GNU find.
        assert_eq!(parse("123").unwrap().id, 4242);
        assert_eq!(parse("124").unwrap().id, 124);
    }

    #[test]
    fn unknown_or_malformed_values_are_errors() {
        assert_eq!(parse("bob"), Err("unknown user 'bob'".to_string()));
        assert!(parse("!").is_err());
        assert!(parse("").is_err());
        assert!(parse("-1").is_err());
        assert!(parse("+5").is_err(), "u32::from_str would accept +5");
        assert!(parse("99999999999").is_err());
        let err = parse_spec("nogrp", IdKind::Group, |_| Ok(None)).unwrap_err();
        assert_eq!(err, "unknown group 'nogrp'");
    }

    #[test]
    fn plain_values_are_ored_negated_ones_exclude() {
        let p = |v: &[&str]| -> Vec<IdSpec> { v.iter().map(|s| parse(s).unwrap()).collect() };
        let either = IdFilter::new(&p(&["root", "alice"]));
        assert!(either.matches(0) && either.matches(1000) && !either.matches(7));
        let not_root = IdFilter::new(&p(&["!root"]));
        assert!(!not_root.matches(0) && not_root.matches(1000));
        let none = IdFilter::default();
        assert!(!none.is_active() && none.matches(12345));
    }

    #[test]
    fn owner_and_group_are_anded() {
        let o = Ownership::new(
            &[IdSpec {
                id: 0,
                negated: false,
            }],
            &[IdSpec {
                id: 50,
                negated: false,
            }],
        );
        assert!(o.matches_ids(0, 50));
        assert!(!o.matches_ids(0, 0));
        assert!(!o.matches_ids(1000, 50));
        assert!(
            !o.matches(None),
            "unknown metadata never matches an active filter"
        );
        assert!(Ownership::default().matches(None));
    }

    #[cfg(unix)]
    #[test]
    fn real_lookups() {
        // root (uid 0) exists everywhere; its primary group is root on
        // Linux and wheel on macOS, both gid 0.
        assert_eq!(owner_parser("root").unwrap().id, 0);
        assert_eq!(group_parser("0").unwrap().id, 0);
        assert!(owner_parser("no-such-user-arfind-test").is_err());
        assert!(group_parser("no-such-group-arfind-test").is_err());
    }
}
