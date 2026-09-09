// a .meta/ sidecar reuses these file names, so one parser reads both

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

#[derive(Debug)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    pub version: Version,
    pub sources: Vec<String>,
    pub checksums: Vec<String>,
    pub depends: Vec<Dep>,
    pub targets: Vec<String>,
    pub users: Vec<String>,
}

pub fn load(dir: &Path) -> Result<Package, Error> {
    if !dir.is_dir() {
        return Err(Error::NoPackage(dir.to_path_buf()));
    }

    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Name(dir.to_path_buf()))?
        .to_string();

    let version = Version::parse(&required(&dir.join("version"))?)?;

    let sources = lines(&dir.join("sources"))?;
    let checksums = lines(&dir.join("checksums"))?;
    // a hand written recipe can have sources and no checksums yet
    if !checksums.is_empty() && sources.len() != checksums.len() {
        return Err(Error::Counts {
            sources: sources.len(),
            checksums: checksums.len(),
        });
    }

    let depends = depends_from(lines(&dir.join("depends"))?);
    let users = lines(&dir.join("users"))?;

    // one line or one per line, either way
    let mut targets = Vec::new();
    for l in lines(&dir.join("targets"))? {
        targets.extend(l.split_whitespace().map(String::from));
    }
    if targets.is_empty() {
        return Err(Error::Empty(dir.join("targets")));
    }

    Ok(Package {
        name,
        dir: dir.to_path_buf(),
        version,
        sources,
        checksums,
        depends,
        targets,
        users,
    })
}

// only version and targets are required. absent means empty, unreadable is an error
pub(crate) fn lines(p: &Path) -> Result<Vec<String>, Error> {
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
pub(crate) fn depends_from(ls: Vec<String>) -> Vec<Dep> {
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

pub(crate) fn required(p: &Path) -> Result<String, Error> {
    fs::read_to_string(p).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::Required(p.to_path_buf()),
        _ => Error::Io(p.to_path_buf(), e),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // no tempfile, and CARGO_TARGET_TMPDIR is integration-tests only
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-t-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, file: &str, body: &str) {
        fs::write(dir.join(file), body).unwrap();
    }

    #[test]
    fn version_round_trips() {
        let v = Version::parse("7.1.1 1").unwrap();
        assert_eq!(v.upstream, "7.1.1");
        assert_eq!(v.rev, 1);
        assert_eq!(v.to_string(), "7.1.1 1");
    }

    #[test]
    fn loads_a_whole_package() {
        let d = scratch("mesa");
        write(&d, "version", "25.2.0 1\n");
        write(&d, "targets", "x86_64-musl x86_64-gnu\n");
        write(
            &d,
            "sources",
            "https://example.invalid/mesa-25.2.0.tar.xz\n",
        );
        write(&d, "checksums", &format!("{}\n", "e3b0c442".repeat(8)));
        write(&d, "depends", "libdrm\nwayland\nmuon make\n");

        let p = load(&d).unwrap();
        assert_eq!(p.name, "mesa");
        assert_eq!(p.version.to_string(), "25.2.0 1");
        assert_eq!(p.targets, ["x86_64-musl", "x86_64-gnu"]);
        assert_eq!(p.sources.len(), 1);
        assert_eq!(p.checksums.len(), 1);

        assert_eq!(p.depends.len(), 3);
        assert!(!p.depends[0].make);
        assert_eq!(p.depends[2].name, "muon");
        assert!(p.depends[2].make);
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
    fn optional_files_can_just_not_be_there() {
        let d = scratch("nodeps");
        write(&d, "version", "1.0 1");
        write(&d, "targets", "x86_64-musl");

        let p = load(&d).unwrap();
        assert!(p.depends.is_empty());
        assert!(p.sources.is_empty());
        assert_eq!(p.targets, ["x86_64-musl"]);
    }

    #[test]
    fn targets_one_per_line_works_too() {
        let d = scratch("perline");
        write(&d, "version", "2 3");
        write(&d, "targets", "x86_64-musl\nx86_64-gnu\n");

        assert_eq!(load(&d).unwrap().targets, ["x86_64-musl", "x86_64-gnu"]);
    }

    #[test]
    fn a_directory_that_isnt_there_says_so() {
        let d = scratch("gone");
        fs::remove_dir_all(&d).unwrap();
        assert!(matches!(load(&d), Err(Error::NoPackage(_))));
    }

    #[test]
    fn version_has_to_have_both_fields() {
        assert!(matches!(Version::parse("7.1.1"), Err(Error::Version(_))));
        assert!(matches!(Version::parse(""), Err(Error::Version(_))));
        assert!(matches!(Version::parse("7.1.1 x"), Err(Error::Version(_))));
    }

    #[test]
    fn a_missing_version_names_the_file() {
        let d = scratch("noversion");
        write(&d, "targets", "x86_64-musl");

        match load(&d) {
            Err(Error::Required(p)) => assert!(p.ends_with("version")),
            other => panic!("wanted Required, got {other:?}"),
        }
    }

    #[test]
    fn targets_cannot_be_blank() {
        let d = scratch("notargets");
        write(&d, "version", "1 1");
        write(&d, "targets", "\n\n# only a comment\n");

        assert!(matches!(load(&d), Err(Error::Empty(_))));
    }

    #[test]
    fn checksums_have_to_pair_up_with_sources() {
        let d = scratch("shortsums");
        write(&d, "version", "1 1");
        write(&d, "targets", "x86_64-musl");
        write(&d, "sources", "a\nb\n");
        write(&d, "checksums", "aa\n");

        match load(&d) {
            Err(Error::Counts { sources, checksums }) => {
                assert_eq!((sources, checksums), (2, 1));
            }
            other => panic!("wanted Counts, got {other:?}"),
        }
    }

    #[test]
    fn a_file_that_will_not_read_is_not_an_empty_file() {
        let d = scratch("weird");
        write(&d, "version", "1 1");
        write(&d, "targets", "x86_64-musl");
        fs::create_dir(d.join("sources")).unwrap();

        assert!(matches!(load(&d), Err(Error::Io(_, _))));
    }
}
