// a .meta/ sidecar reuses these file names, so one parser reads both

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub upstream: String,
    pub rev: u32,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version, Error> {
        let mut f = s.split_whitespace();
        let (Some(upstream), Some(rev)) = (f.next(), f.next()) else {
            return Err(Error::Version(s.trim().to_string()));
        };
        let rev = rev
            .parse()
            .map_err(|_| Error::Version(s.trim().to_string()))?;

        Ok(Version {
            upstream: upstream.to_string(),
            rev,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.upstream, self.rev)
    }
}

#[derive(Debug, Clone)]
pub struct Dep {
    pub name: String,
    // build-time only: not a runtime edge, so nothing follows it outward and removing
    // the named package is not blocked by this one
    pub make: bool,
    // resolved under KIRY_HOST rather than the target being built
    pub host: bool,
    // a target suffix the line is restricted to, matched the way a triple ends. musl
    // needs fts and obstack and argp as separate packages and glibc has all three
    // built in, so on gnu those names must not resolve to anything at all
    pub only: Option<String>,
}

impl Dep {
    pub fn applies(&self, target: &str) -> bool {
        self.only.as_deref().is_none_or(|o| target.ends_with(o))
    }
}

// the line this came from, so a record written back out reads like the recipe did
impl fmt::Display for Dep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if self.make {
            write!(f, "{}", if self.host { " make" } else { " build" })?;
        }
        match &self.only {
            Some(o) => write!(f, " {o}"),
            None => Ok(()),
        }
    }
}

// only version and targets are required. absent means empty, unreadable is an error
pub fn lines(p: &Path) -> Result<Vec<String>, Error> {
    let text = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(p.to_path_buf(), e)),
    };

    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

// both suffixes mean build-only and they differ in which target answers. " make" is a
// tool that runs during the build, so the host's copy of it is the right one -- bison,
// perl. " build" is something compiled against, so it has to be the target's --
// xorgproto, wayland-protocols, and every other package that is only headers. one word
// meant both until a gnu build looked for X11/Xfuncproto.h and found musl's
pub fn depends_from(ls: Vec<String>) -> Vec<Dep> {
    let mut out = Vec::new();
    for l in ls {
        let mut f = l.split_whitespace();
        let Some(name) = f.next() else { continue };
        // the two words after the name are told apart by which they are rather than by
        // position, so neither order can be got wrong by hand
        let (mut kind, mut only) = (None, None);
        for w in f {
            match w {
                "make" | "build" => kind = Some(w),
                _ => only = Some(w.to_string()),
            }
        }
        out.push(Dep {
            name: name.to_string(),
            make: kind == Some("make") || kind == Some("build"),
            host: kind == Some("make"),
            only,
        });
    }
    out
}

pub fn required(p: &Path) -> Result<String, Error> {
    fs::read_to_string(p).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::Required(p.to_path_buf()),
        _ => Error::Io(p.to_path_buf(), e),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn version_round_trips() {
        let v = Version::parse("7.1.1 1").unwrap();
        assert_eq!(v.upstream, "7.1.1");
        assert_eq!(v.rev, 1);
        assert_eq!(v.to_string(), "7.1.1 1");
    }

    #[test]
    fn a_dep_can_be_restricted_to_one_target() {
        let d = depends_from(
            [
                "musl-fts musl",
                "bsd-compat-headers build musl",
                // the other order, because a hand-written line gets both
                "argp-standalone musl build",
                "zlib",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );

        assert_eq!(d[0].only.as_deref(), Some("musl"));
        assert!(!d[0].make, "the target word was read as a kind");
        assert!(d[1].make && !d[1].host);
        assert!(d[2].make && !d[2].host && d[2].only.as_deref() == Some("musl"));

        for x in &d[..3] {
            assert!(x.applies("x86_64-musl"), "{x} is musl's");
            assert!(!x.applies("x86_64-gnu"), "{x} reached the gnu build");
        }
        assert!(d[3].applies("x86_64-gnu"), "an unrestricted dep got dropped");
        assert_eq!(d[1].to_string(), "bsd-compat-headers build musl");
    }

    #[test]
    fn version_has_to_have_both_fields() {
        assert!(matches!(Version::parse("7.1.1"), Err(Error::Version(_))));
        assert!(matches!(Version::parse(""), Err(Error::Version(_))));
        assert!(matches!(Version::parse("7.1.1 x"), Err(Error::Version(_))));
    }
}
