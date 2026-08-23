// in the root subvolume, not /var: @var is shared between the A/B roots, so a db
// there would outlive a rollback of the files it describes

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::pkg::{self, Dep, Version};
use crate::Error;

const DB: &str = "usr/lib/kiry/db";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File(String),
    Dir,
    Link(String),
    Hard(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: u32,
    pub kind: Kind,
    pub path: String,
}

impl Entry {
    fn parse(line: &str, n: usize) -> Result<Entry, Error> {
        let bad = |why| Error::Manifest { line: n, why };

        // path last, so one with a space in it needs no quoting
        let mut f = line.splitn(4, ' ');
        let (Some(kind), Some(mode), Some(third), Some(path)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            return Err(bad("wanted four fields"));
        };

        let mode = u32::from_str_radix(mode, 8).map_err(|_| bad("mode is not octal"))?;

        let kind = match kind {
            "f" => {
                // a truncated hash later looks like a modified file
                if third.len() != 64 || !third.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(bad("not a sha256"));
                }
                Kind::File(third.to_string())
            }
            "d" => {
                if third != "-" {
                    return Err(bad("a directory takes - in field three"));
                }
                Kind::Dir
            }
            "l" => Kind::Link(third.to_string()),
            "h" => Kind::Hard(third.to_string()),
            _ => return Err(bad("kind is not one of f d l h")),
        };

        if path.is_empty() {
            return Err(bad("no path"));
        }
        if path.starts_with('/') {
            return Err(bad("path must be relative to the root"));
        }

        Ok(Entry {
            mode,
            kind,
            path: path.to_string(),
        })
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (k, third) = match &self.kind {
            Kind::File(h) => ('f', h.as_str()),
            Kind::Dir => ('d', "-"),
            Kind::Link(t) => ('l', t.as_str()),
            Kind::Hard(t) => ('h', t.as_str()),
        };
        write!(f, "{k} {:04o} {third} {}", self.mode, self.path)
    }
}

// no # comments: a file can be named #something
pub fn parse_manifest(text: &str) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        out.push(Entry::parse(line, i + 1)?);
    }
    Ok(out)
}

pub fn format_manifest(entries: &[Entry]) -> Result<String, Error> {
    let mut out = String::new();
    for e in entries {
        if e.path.is_empty() || e.path.starts_with('/') || e.path.contains('\n') {
            return Err(Error::BadPath(e.path.clone()));
        }
        // field three is positional too
        if let Kind::Link(t) | Kind::Hard(t) = &e.kind {
            if t.contains(' ') || t.contains('\n') {
                return Err(Error::BadPath(t.clone()));
            }
        }
        out.push_str(&e.to_string());
        out.push('\n');
    }
    Ok(out)
}

#[derive(Debug)]
pub struct Installed {
    pub name: String,
    pub target: String,
    pub version: Version,
    pub depends: Vec<Dep>,
    pub manifest: Vec<Entry>,
}

pub fn dir(root: &Path, target: &str, name: &str) -> PathBuf {
    root.join(DB).join("installed").join(target).join(name)
}

pub fn targets(root: &Path) -> Result<Vec<String>, Error> {
    names(&root.join(DB).join("installed"))
}

pub fn forget(root: &Path, target: &str, name: &str) -> Result<(), Error> {
    let d = dir(root, target, name);
    fs::remove_dir_all(&d).map_err(|e| Error::Io(d, e))
}

pub fn installed(root: &Path, target: &str) -> Result<Vec<String>, Error> {
    names(&root.join(DB).join("installed").join(target))
}

fn names(d: &Path) -> Result<Vec<String>, Error> {
    let rd = match fs::read_dir(d) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(d.to_path_buf(), e)),
    };

    let mut out = Vec::new();
    for e in rd {
        let e = e.map_err(|e| Error::Io(d.to_path_buf(), e))?;
        if let Some(n) = e.file_name().to_str() {
            out.push(n.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub fn read(root: &Path, target: &str, name: &str) -> Result<Installed, Error> {
    let d = dir(root, target, name);
    if !d.is_dir() {
        return Err(Error::NoPackage(d));
    }

    Ok(Installed {
        name: name.to_string(),
        target: target.to_string(),
        version: Version::parse(&pkg::required(&d.join("version"))?)?,
        depends: pkg::depends_from(pkg::lines(&d.join("depends"))?),
        manifest: parse_manifest(&pkg::required(&d.join("manifest"))?)?,
    })
}

pub fn write(root: &Path, rec: &Installed) -> Result<(), Error> {
    // before anything is created, so a bad path leaves no half record
    let manifest = format_manifest(&rec.manifest)?;

    let mut depends = String::new();
    for d in &rec.depends {
        depends.push_str(&d.name);
        if d.make {
            depends.push_str(" make");
        }
        depends.push('\n');
    }

    let d = dir(root, &rec.target, &rec.name);
    fs::create_dir_all(&d).map_err(|e| Error::Io(d.clone(), e))?;

    // TODO: wants to land atomically, the install path will care about that
    put(&d.join("version"), &format!("{}\n", rec.version))?;
    put(&d.join("depends"), &depends)?;
    put(&d.join("manifest"), &manifest)
}

fn put(p: &Path, body: &str) -> Result<(), Error> {
    fs::write(p, body).map_err(|e| Error::Io(p.to_path_buf(), e))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-db-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> String {
        format!(
            "f 0755 {SHA} usr/bin/foo\n\
             d 0755 - usr/share/foo\n\
             l 0777 ../bar usr/bin/baz\n\
             h 0644 usr/bin/foo usr/bin/foo-alias\n"
        )
    }

    #[test]
    fn a_manifest_round_trips_byte_for_byte() {
        let text = sample();
        let entries = parse_manifest(&text).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(format_manifest(&entries).unwrap(), text);
    }

    #[test]
    fn each_kind_lands_where_it_should() {
        let e = parse_manifest(&sample()).unwrap();
        assert_eq!(e[0].kind, Kind::File(SHA.to_string()));
        assert_eq!(e[0].mode, 0o755);
        assert_eq!(e[1].kind, Kind::Dir);
        assert_eq!(e[2].kind, Kind::Link("../bar".to_string()));
        assert_eq!(e[3].kind, Kind::Hard("usr/bin/foo".to_string()));
        assert_eq!(e[3].path, "usr/bin/foo-alias");
    }

    #[test]
    fn a_path_with_spaces_needs_no_quoting() {
        let line = format!("f 0644 {SHA} usr/share/some app/read me.txt\n");
        let e = parse_manifest(&line).unwrap();
        assert_eq!(e[0].path, "usr/share/some app/read me.txt");
        assert_eq!(format_manifest(&e).unwrap(), line);
    }

    #[test]
    fn junk_lines_name_their_line_number() {
        let cases = [
            ("f 0644 {SHA}", "wanted four fields"),
            ("f 0644 {SHA} ", "no path"),
            ("x 0644 {SHA} usr/bin/foo", "kind is not one of f d l h"),
            ("f 08x9 {SHA} usr/bin/foo", "mode is not octal"),
            ("f 0644 deadbeef usr/bin/foo", "not a sha256"),
            ("d 0755 nope usr/share/foo", "a directory takes - in field three"),
            ("f 0644 {SHA} /usr/bin/foo", "path must be relative to the root"),
        ];
        for (raw, want) in cases {
            let line = raw.replace("{SHA}", SHA);
            let text = format!("d 0755 - usr\n{line}\n");
            match parse_manifest(&text) {
                Err(Error::Manifest { line, why }) => {
                    assert_eq!(line, 2, "wrong line for {raw:?}");
                    assert_eq!(why, want, "wrong reason for {raw:?}");
                }
                other => panic!("{raw:?} should not have parsed, got {other:?}"),
            }
        }
    }

    #[test]
    fn unrepresentable_paths_are_refused_before_writing() {
        let mut e = Entry {
            mode: 0o644,
            kind: Kind::File(SHA.to_string()),
            path: "usr/bin/with\nnewline".to_string(),
        };
        assert!(matches!(format_manifest(&[e.clone()]), Err(Error::BadPath(_))));

        e.path = "/absolute".to_string();
        assert!(matches!(format_manifest(&[e.clone()]), Err(Error::BadPath(_))));

        e.path = "usr/bin/baz".to_string();
        e.kind = Kind::Link("../some dir/bar".to_string());
        assert!(matches!(format_manifest(&[e]), Err(Error::BadPath(_))));
    }

    #[test]
    fn a_record_survives_a_write_and_a_read() {
        let root = scratch("root");
        let rec = Installed {
            name: "mesa".to_string(),
            target: "x86_64-musl".to_string(),
            version: Version::parse("25.2.0 1").unwrap(),
            depends: pkg::depends_from(vec![
                "libdrm".to_string(),
                "muon make".to_string(),
            ]),
            manifest: parse_manifest(&sample()).unwrap(),
        };

        write(&root, &rec).unwrap();
        let back = read(&root, "x86_64-musl", "mesa").unwrap();

        assert_eq!(back.name, "mesa");
        assert_eq!(back.version.to_string(), "25.2.0 1");
        assert_eq!(back.depends.len(), 2);
        assert!(!back.depends[0].make);
        assert!(back.depends[1].make);
        assert_eq!(back.manifest, rec.manifest);

        let d = root.join("usr/lib/kiry/db/installed/x86_64-musl/mesa");
        assert!(d.join("manifest").is_file());
        assert_eq!(fs::read_to_string(d.join("version")).unwrap(), "25.2.0 1\n");
    }

    #[test]
    fn reading_something_not_installed_says_so() {
        let root = scratch("empty");
        assert!(matches!(
            read(&root, "x86_64-musl", "nothing"),
            Err(Error::NoPackage(_))
        ));
    }

    #[test]
    fn a_bad_path_leaves_no_half_written_record() {
        let root = scratch("halfway");
        let rec = Installed {
            name: "bad".to_string(),
            target: "x86_64-musl".to_string(),
            version: Version::parse("1 1").unwrap(),
            depends: Vec::new(),
            manifest: vec![Entry {
                mode: 0o644,
                kind: Kind::Dir,
                path: "/nope".to_string(),
            }],
        };

        assert!(matches!(write(&root, &rec), Err(Error::BadPath(_))));
        assert!(!dir(&root, "x86_64-musl", "bad").exists());
    }
}
