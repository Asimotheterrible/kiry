// a package is a directory of plain text files, one fact per file. the .meta/
// sidecar beside a cached artifact reuses these names on purpose, so this reads
// both and there is no second format to keep in sync

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub upstream: String,
    pub rev: u32,
}

impl Version {
    pub fn parse(s: &str) -> Version {
        let mut f = s.split_whitespace();
        let upstream = f.next().unwrap().to_string();
        let rev = f.next().unwrap().parse().unwrap();
        Version { upstream, rev }
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
    pub make: bool,
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
}

pub fn load(dir: &Path) -> Package {
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    let version = Version::parse(&fs::read_to_string(dir.join("version")).unwrap());

    let sources = lines(&dir.join("sources"));
    let checksums = lines(&dir.join("checksums"));
    // TODO: these two have to line up

    let mut depends = Vec::new();
    for l in lines(&dir.join("depends")) {
        // " make" on the end means build-only. splitting and looking at field two
        // beats matching the suffix, since the name can't contain a space anyway
        let mut f = l.split_whitespace();
        let name = f.next().unwrap().to_string();
        let make = f.next() == Some("make");
        depends.push(Dep { name, make });
    }

    // normally one line, space separated. nothing stops one per line though and
    // it would be annoying to care
    let mut targets = Vec::new();
    for l in lines(&dir.join("targets")) {
        targets.extend(l.split_whitespace().map(String::from));
    }

    Package {
        name,
        dir: dir.to_path_buf(),
        version,
        sources,
        checksums,
        depends,
        targets,
    }
}

// version and targets are the only two files every package has. the rest are
// optional and a missing one is just an empty list
fn lines(p: &Path) -> Vec<String> {
    // FIXME: an unreadable file looks identical to a missing one here
    let text = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // no tempfile crate, and CARGO_TARGET_TMPDIR is integration-tests only, so
    // this is what's left. i hate it but it works
    fn scratch(name: &str) -> PathBuf {
        // the leaf has to be the package name, load() takes it from the directory
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
        let v = Version::parse("7.1.1 1");
        assert_eq!(v.upstream, "7.1.1");
        assert_eq!(v.rev, 1);
        assert_eq!(v.to_string(), "7.1.1 1");
    }

    #[test]
    fn loads_a_whole_package() {
        let d = scratch("mesa");
        write(&d, "version", "25.2.0 1\n");
        write(&d, "targets", "x86_64-musl x86_64-gnu\n");
        write(&d, "sources", "https://example.invalid/mesa-25.2.0.tar.xz\n");
        write(&d, "checksums", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n");
        write(&d, "depends", "libdrm\nwayland\nmuon make\n");

        let p = load(&d);
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
    fn optional_files_can_just_not_be_there() {
        let d = scratch("nodeps");
        write(&d, "version", "1.0 1");
        write(&d, "targets", "x86_64-musl");

        let p = load(&d);
        assert!(p.depends.is_empty());
        assert!(p.sources.is_empty());
        assert_eq!(p.targets, ["x86_64-musl"]);
    }

    #[test]
    fn targets_one_per_line_works_too() {
        let d = scratch("perline");
        write(&d, "version", "2 3");
        write(&d, "targets", "x86_64-musl\nx86_64-gnu\n");

        assert_eq!(load(&d).targets, ["x86_64-musl", "x86_64-gnu"]);
    }
}
