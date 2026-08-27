use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::pkg::{self, Dep, Version};
use crate::{archive, db, elf, Error};

#[derive(Debug, Clone)]
pub struct Job {
    pub name: String,
    pub target: String,
    pub version: Version,
    pub depends: Vec<Dep>,
    pub archive: PathBuf,
    pub members: Vec<archive::Member>,
}

// every archive is read and checked before any of it is applied, so the order they
// were named in cannot decide whether the batch is valid
pub fn plan(root: &Path, archives: &[PathBuf], force: bool) -> Result<Vec<Job>, Error> {
    let mut jobs = Vec::new();
    for a in archives {
        let (name, target, version, depends) = meta(a)?;
        let members = archive::plan(root, a)?;
        jobs.push(Job {
            name,
            target,
            version,
            depends,
            archive: a.clone(),
            members,
        });
    }

    if !force {
        paths(root, &jobs)?;
        deps(root, &jobs)?;
    }
    Ok(jobs)
}

fn paths(root: &Path, jobs: &[Job]) -> Result<(), Error> {
    let replacing: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();

    let mut owner: HashMap<String, String> = HashMap::new();
    for j in jobs {
        for name in db::installed(root, &j.target)? {
            if replacing.contains(&name.as_str()) {
                continue;
            }
            for e in db::read(root, &j.target, &name)?.manifest {
                if !matches!(e.kind, db::Kind::Dir) {
                    owner.insert(e.path, name.clone());
                }
            }
        }
    }

    for j in jobs {
        for m in &j.members {
            if m.what == archive::What::Dir {
                continue;
            }
            if let Some(who) = owner.get(&m.path) {
                return Err(Error::Conflict {
                    path: m.path.clone(),
                    owner: who.clone(),
                });
            }
            owner.insert(m.path.clone(), j.name.clone());
        }
    }
    Ok(())
}

fn deps(root: &Path, jobs: &[Job]) -> Result<(), Error> {
    for j in jobs {
        let have = db::installed(root, &j.target)?;
        for d in &j.depends {
            if d.make {
                continue;
            }
            if have.contains(&d.name) || jobs.iter().any(|o| o.name == d.name) {
                continue;
            }
            return Err(Error::MissingDep {
                pkg: j.name.clone(),
                dep: d.name.clone(),
            });
        }
    }
    Ok(())
}

// TODO: a failure partway through leaves the earlier jobs applied
pub fn apply(root: &Path, jobs: &[Job]) -> Result<(), Error> {
    for j in jobs {
        let manifest = archive::extract(root, &j.archive)?;
        let provides = scan(root, &manifest)?
            .into_iter()
            .filter_map(|(path, s)| {
                let Seen::Elf(o) = s else { return None };
                Some(db::Provide {
                    soname: o.soname?,
                    versioned: o.versioned,
                    path,
                })
            })
            .collect::<Vec<_>>();
        db::write(
            root,
            &db::Installed {
                name: j.name.clone(),
                target: j.target.clone(),
                version: j.version.clone(),
                depends: j.depends.clone(),
                manifest,
            },
        )?;
        db::write_provides(root, &j.target, &j.name, &provides)?;
    }
    dispossess(root, jobs)
}

// --force lets a package take a path from whoever had it. the database has to stop
// claiming the loser owns it, or two records disagree about what lives there and
// whoever reads the older one rebuilds a shape that is no longer on disk
fn dispossess(root: &Path, jobs: &[Job]) -> Result<(), Error> {
    for j in jobs {
        let taken: HashSet<&str> = j
            .members
            .iter()
            .filter(|m| m.what != archive::What::Dir)
            .map(|m| m.path.as_str())
            .collect();

        for name in db::installed(root, &j.target)? {
            if name == j.name {
                continue;
            }
            let mut rec = db::read(root, &j.target, &name)?;
            let before = rec.manifest.len();
            rec.manifest
                .retain(|e| matches!(e.kind, db::Kind::Dir) || !taken.contains(e.path.as_str()));
            if rec.manifest.len() != before {
                db::write(root, &rec)?;
            }
        }
    }
    Ok(())
}

// what a manifest entry turned out to be, once opened. one pass, one open per file:
// an elf answers for its linkage, a shebang for its interpreter, and both are refusals
// the kernel makes before a single instruction runs
pub enum Seen {
    Elf(elf::Elf),
    Script(Vec<String>),
    Other,
    // listed by the manifest and will not open, or will not parse. doctor reports it
    // rather than skipping it, which is how a root missing half its binaries used to
    // come back clean
    Bad,
}

pub fn scan(root: &Path, manifest: &[db::Entry]) -> Result<Vec<(String, Seen)>, Error> {
    let rootfd = rustix::fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| Error::Io(root.to_path_buf(), e.into()))?;

    let mut out = Vec::new();
    for e in manifest {
        if !matches!(e.kind, db::Kind::File(_)) {
            continue;
        }
        let (parent, leaf) = match e.path.rfind('/') {
            Some(i) => (&e.path[..i], &e.path[i + 1..]),
            None => ("", e.path.as_str()),
        };
        let Ok(pfd) = archive::beneath(&rootfd, if parent.is_empty() { "." } else { parent })
        else {
            out.push((e.path.clone(), Seen::Bad));
            continue;
        };
        let Ok(fd) =
            rustix::fs::openat(&pfd, leaf, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())
        else {
            out.push((e.path.clone(), Seen::Bad));
            continue;
        };

        // the kernel reads 256 bytes of a shebang line and no more, so neither does this
        let mut head = [0u8; 256];
        let mut f = std::fs::File::from(fd);
        let n = f.read(&mut head).unwrap_or(0);
        let head = &head[..n];

        if head.starts_with(&elf::MAGIC) {
            let mut bytes = head.to_vec();
            if f.read_to_end(&mut bytes).is_err() {
                out.push((e.path.clone(), Seen::Bad));
                continue;
            }
            out.push((
                e.path.clone(),
                match elf::parse(&bytes) {
                    Ok(o) => Seen::Elf(o),
                    Err(_) => Seen::Bad,
                },
            ));
            continue;
        }

        if e.mode & 0o111 != 0 && head.starts_with(b"#!") {
            let line = head[2..].split(|c| *c == b'\n').next().unwrap_or_default();
            let words: Vec<String> = String::from_utf8_lossy(line)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !words.is_empty() {
                out.push((e.path.clone(), Seen::Script(words)));
                continue;
            }
        }
        out.push((e.path.clone(), Seen::Other));
    }
    Ok(out)
}

// sidecar is <archive>.meta, appended rather than derived, since the archive name
// itself is a display name and is never parsed
fn meta(a: &Path) -> Result<(String, String, Version, Vec<Dep>), Error> {
    let d = PathBuf::from(format!("{}.meta", a.display()));
    let name = pkg::required(&d.join("name"))?.trim().to_string();
    let version = Version::parse(&pkg::required(&d.join("version"))?)?;
    let depends = pkg::depends_from(pkg::lines(&d.join("depends"))?);

    let targets = pkg::lines(&d.join("targets"))?;
    let target = match targets.len() {
        1 => targets[0].clone(),
        0 => return Err(Error::Empty(d.join("targets"))),
        _ => return Err(Error::Targets(d.join("targets"))),
    };

    Ok((name, target, version, depends))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::archive::{Member, What};
    use std::fs;
    use std::path::PathBuf;

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-in-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(name: &str, deps: &[&str], paths: &[&str]) -> Job {
        Job {
            name: name.to_string(),
            target: "x86_64-musl".to_string(),
            version: Version::parse("1.0 1").unwrap(),
            depends: deps
                .iter()
                .map(|d| Dep {
                    name: d.to_string(),
                    make: false,
                })
                .collect(),
            archive: PathBuf::from(format!("{name}.tar.zst")),
            members: paths
                .iter()
                .map(|p| Member {
                    what: What::File,
                    mode: 0o644,
                    path: p.to_string(),
                })
                .collect(),
        }
    }

    fn record(root: &Path, name: &str, paths: &[&str]) {
        db::write(
            root,
            &db::Installed {
                name: name.to_string(),
                target: "x86_64-musl".to_string(),
                version: Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: paths
                    .iter()
                    .map(|p| db::Entry {
                        mode: 0o644,
                        kind: db::Kind::File(SHA.to_string()),
                        path: p.to_string(),
                    })
                    .collect(),
            },
        )
        .unwrap();
    }

    #[test]
    fn a_path_another_package_owns_is_refused() {
        let root = scratch("owned");
        record(&root, "foo", &["usr/bin/x"]);

        match paths(&root, &[job("bar", &[], &["usr/bin/x"])]) {
            Err(Error::Conflict { path, owner }) => {
                assert_eq!(path, "usr/bin/x");
                assert_eq!(owner, "foo");
            }
            other => panic!("wanted Conflict, got {other:?}"),
        }
    }

    #[test]
    fn two_members_of_one_batch_cannot_claim_the_same_path() {
        let root = scratch("clash");
        let a = job("foo", &[], &["usr/bin/x"]);
        let b = job("bar", &[], &["usr/bin/x"]);

        assert!(matches!(paths(&root, &[a, b]), Err(Error::Conflict { .. })));
    }

    #[test]
    fn a_missing_dependency_stops_the_batch() {
        let root = scratch("dep");

        match deps(&root, &[job("foo", &["libbar"], &["usr/bin/foo"])]) {
            Err(Error::MissingDep { pkg, dep }) => {
                assert_eq!((pkg.as_str(), dep.as_str()), ("foo", "libbar"));
            }
            other => panic!("wanted MissingDep, got {other:?}"),
        }
    }

    #[test]
    fn order_in_the_batch_does_not_decide_anything() {
        let root = scratch("order");
        let dep = job("libbar", &[], &["usr/lib/libbar.so"]);
        let app = job("foo", &["libbar"], &["usr/bin/foo"]);

        assert!(deps(&root, &[app.clone(), dep.clone()]).is_ok());
        assert!(deps(&root, &[dep, app]).is_ok());
    }

    #[test]
    fn reinstalling_does_not_conflict_with_itself() {
        let root = scratch("again");
        record(&root, "foo", &["usr/bin/foo"]);

        assert!(paths(&root, &[job("foo", &[], &["usr/bin/foo"])]).is_ok());
    }

    #[test]
    fn a_sidecar_names_exactly_one_target_or_none_at_all() {
        let d = scratch("sidecar");
        let side = d.join("foo.tar.zst.meta");
        fs::create_dir_all(&side).unwrap();
        fs::write(side.join("name"), "foo\n").unwrap();
        fs::write(side.join("version"), "1.0 1\n").unwrap();

        fs::write(side.join("targets"), "").unwrap();
        assert!(matches!(meta(&d.join("foo.tar.zst")), Err(Error::Empty(_))));

        fs::write(side.join("targets"), "musl\ngnu\n").unwrap();
        assert!(matches!(
            meta(&d.join("foo.tar.zst")),
            Err(Error::Targets(_))
        ));
    }
}

// compiled in rather than configured: a package legitimately owns usr/lib, the
// repair path has to work with no config present, and a config file that can
// delete /usr is not worth having
const PROTECTED: &[&str] = &[
    "",
    "usr",
    "etc",
    "var",
    "bin",
    "sbin",
    "lib",
    "lib64",
    "usr/bin",
    "usr/sbin",
    "usr/lib",
    "usr/lib64",
    "usr/local",
    "usr/share",
];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub gone: usize,
    pub missing: usize,
    pub kept: usize,
}

pub fn remove(root: &Path, target: &str, name: &str, force: bool) -> Result<Removed, Error> {
    let rec = db::read(root, target, name)?;

    if !force {
        for other in db::installed(root, target)? {
            if other == name {
                continue;
            }
            let o = db::read(root, target, &other)?;
            if o.depends.iter().any(|d| !d.make && d.name == name) {
                return Err(Error::Needed {
                    pkg: name.to_string(),
                    by: other,
                });
            }
        }
    }

    let rootfd = rustix::fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| Error::Io(root.to_path_buf(), e.into()))?;

    let mut out = Removed::default();
    for e in rec.manifest.iter().rev() {
        let (parent, leaf) = match e.path.rfind('/') {
            Some(i) => (&e.path[..i], &e.path[i + 1..]),
            None => ("", e.path.as_str()),
        };
        let pfd = match archive::beneath(&rootfd, if parent.is_empty() { "." } else { parent }) {
            Ok(fd) => fd,
            Err(_) => {
                out.missing += 1;
                continue;
            }
        };

        match &e.kind {
            db::Kind::Dir => {
                if !PROTECTED.contains(&e.path.as_str()) {
                    let _ = rustix::fs::unlinkat(&pfd, leaf, AtFlags::REMOVEDIR);
                }
            }
            db::Kind::Link(target) => match rustix::fs::readlinkat(&pfd, leaf, Vec::new()) {
                Ok(got) if got.to_bytes() == target.as_bytes() => {
                    let _ = rustix::fs::unlinkat(&pfd, leaf, AtFlags::empty());
                    out.gone += 1;
                }
                Ok(_) => out.kept += 1,
                Err(Errno::NOENT) => out.missing += 1,
                Err(_) => out.kept += 1,
            },
            // only ENOENT is gone. NOFOLLOW turns a symlink standing where the file
            // belongs into ELOOP, and that is modified, not missing
            db::Kind::File(want) | db::Kind::Hard(want) => match hash(&pfd, leaf) {
                Ok(got) if &got == want => {
                    let _ = rustix::fs::unlinkat(&pfd, leaf, AtFlags::empty());
                    out.gone += 1;
                }
                Ok(_) => out.kept += 1,
                Err(Errno::NOENT) => out.missing += 1,
                Err(_) => out.kept += 1,
            },
        }
    }

    db::forget(root, target, name)?;
    Ok(out)
}

fn hash(pfd: &OwnedFd, name: &str) -> Result<String, Errno> {
    let fd = rustix::fs::openat(pfd, name, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())?;
    crate::sha256(std::fs::File::from(fd)).map_err(|_| Errno::IO)
}
