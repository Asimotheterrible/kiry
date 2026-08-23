// resolving here is TOCTOU-able. RESOLVE_BENEATH in extract() is what actually
// keeps a member inside the root

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags, ResolveFlags};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::db;
use crate::{hex, Error};

// the kernel gives up at 40 as well
const MAX_HOPS: u32 = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    File,
    Dir,
    Link(String),
    Hard(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub what: What,
    pub mode: u32,
    // resolved, relative to the root
    pub path: String,
}

pub fn plan(root: &Path, archive: &Path) -> Result<Vec<Member>, Error> {
    let f = fs::File::open(archive).map_err(|e| Error::Io(archive.to_path_buf(), e))?;
    let z = ruzstd::decoding::StreamingDecoder::new(io::BufReader::new(f)).map_err(|_| {
        Error::Archive {
            path: archive.display().to_string(),
            why: "not a zstd stream",
        }
    })?;
    plan_reader(root, z).map_err(|e| match e {
        Error::Io(p, inner) if p.as_os_str().is_empty() => Error::Io(archive.to_path_buf(), inner),
        other => other,
    })
}

pub fn plan_reader<R: Read>(root: &Path, reader: R) -> Result<Vec<Member>, Error> {
    walk(root, reader, |_, _| Ok(()))
}

// one walk, two callers: plan collects, extract applies
fn walk<R: Read>(
    root: &Path,
    reader: R,
    mut each: impl FnMut(&mut tar::Entry<'_, R>, &Member) -> Result<(), Error>,
) -> Result<Vec<Member>, Error> {
    let mut tar = tar::Archive::new(reader);
    let mut out = Vec::new();
    // symlinks this archive makes, before they exist on disk
    let mut made: HashMap<String, String> = HashMap::new();

    for e in tar.entries().map_err(anon)? {
        let mut e = e.map_err(anon)?;

        let kind = e.header().entry_type();
        let raw = e.path().map_err(anon)?.display().to_string();
        let path = match tidy(&raw) {
            Ok(Some(p)) => p,
            // `tar c .` emits ./ for the tree root
            Ok(None) if kind == EntryType::Directory => continue,
            Ok(None) => {
                return Err(Error::Archive {
                    path: raw,
                    why: "empty path",
                })
            }
            Err(why) => return Err(Error::Archive { path: raw, why }),
        };

        // tar is built without the xattr feature
        if let Some(exts) = e.pax_extensions().map_err(anon)? {
            for ext in exts {
                let key = ext.map_err(anon)?.key().unwrap_or("").to_string();
                if key.starts_with("SCHILY.xattr.") || key.starts_with("LIBARCHIVE.xattr.") {
                    return Err(Error::Archive {
                        path,
                        why: "carries an xattr record",
                    });
                }
            }
        }

        let mode = e.header().mode().map_err(anon)?;

        let link = || -> Result<String, Error> {
            let t = e
                .link_name()
                .map_err(anon)?
                .ok_or_else(|| Error::Archive {
                    path: path.clone(),
                    why: "link with no target",
                })?
                .display()
                .to_string();
            Ok(t)
        };

        // a dir lands through a usr-merge symlink, a file replaces one
        let (what, follow_final) = match kind {
            EntryType::Regular | EntryType::Continuous => (What::File, false),
            EntryType::Directory => (What::Dir, true),
            EntryType::Symlink => (What::Link(link()?), false),
            EntryType::Link => {
                let t = link()?;
                let t = tidy(&t)
                    .map_err(|why| Error::Archive {
                        path: path.clone(),
                        why,
                    })?
                    .ok_or_else(|| Error::Archive {
                        path: path.clone(),
                        why: "hard link with an empty target",
                    })?;
                (What::Hard(resolve_or(root, &t, false, &made, &t)?), false)
            }
            EntryType::XHeader | EntryType::XGlobalHeader => continue,
            EntryType::GNULongName | EntryType::GNULongLink => continue,
            _ => {
                return Err(Error::Archive {
                    path,
                    why: "not a file, directory, symlink or hard link",
                })
            }
        };

        let landed = resolve_or(root, &path, follow_final, &made, &path)?;

        if let What::Link(t) = &what {
            made.insert(landed.clone(), t.clone());
        }

        let m = Member {
            what,
            mode,
            path: landed,
        };
        each(&mut e, &m)?;
        out.push(m);
    }

    Ok(out)
}

// mkdirat, symlinkat and linkat take no resolve flags, so the parent is opened with
// openat2 first and the last component created relative to that fd
pub fn extract(root: &Path, archive: &Path) -> Result<Vec<db::Entry>, Error> {
    let f = fs::File::open(archive).map_err(|e| Error::Io(archive.to_path_buf(), e))?;
    let z = ruzstd::decoding::StreamingDecoder::new(io::BufReader::new(f)).map_err(|_| {
        Error::Archive {
            path: archive.display().to_string(),
            why: "not a zstd stream",
        }
    })?;

    let rootfd = rustix::fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| Error::Io(root.to_path_buf(), e.into()))?;

    let mut manifest: Vec<db::Entry> = Vec::new();
    walk(root, z, |entry, m| apply(&rootfd, entry, m, &mut manifest)).map_err(|e| match e {
        Error::Io(p, inner) if p.as_os_str().is_empty() => Error::Io(archive.to_path_buf(), inner),
        other => other,
    })?;

    Ok(manifest)
}

fn apply<R: Read>(
    rootfd: &OwnedFd,
    entry: &mut tar::Entry<'_, R>,
    m: &Member,
    manifest: &mut Vec<db::Entry>,
) -> Result<(), Error> {
    let (parent, name) = split(&m.path);
    let pfd = parent_fd(rootfd, parent, manifest)?;

    // /etc/kiry/setuid does not exist yet
    let mode = m.mode & 0o1777;

    let kind = match &m.what {
        What::Dir => {
            mkdir(&pfd, name, mode, &m.path)?;
            db::Kind::Dir
        }
        What::File => {
            clear(&pfd, name);
            let fd = rustix::fs::openat(
                &pfd,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                Mode::from_bits_truncate(mode),
            )
            .map_err(|e| Error::Io(m.path.clone().into(), e.into()))?;

            let mut out = fs::File::from(fd);
            let mut h = Sha256::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = entry.read(&mut buf).map_err(anon)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| Error::Io(m.path.clone().into(), e))?;
            }
            db::Kind::File(hex(&h.finalize()))
        }
        What::Link(target) => {
            clear(&pfd, name);
            rustix::fs::symlinkat(target.as_str(), &pfd, name)
                .map_err(|e| Error::Io(m.path.clone().into(), e.into()))?;
            db::Kind::Link(target.clone())
        }
        What::Hard(target) => {
            clear(&pfd, name);
            rustix::fs::linkat(
                rootfd,
                target.as_str(),
                &pfd,
                name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(|e| Error::Io(m.path.clone().into(), e.into()))?;
            db::Kind::Hard(target.clone())
        }
    };

    manifest.push(db::Entry {
        mode,
        kind,
        path: m.path.clone(),
    });
    Ok(())
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

fn clear(pfd: &OwnedFd, name: &str) {
    let _ = rustix::fs::unlinkat(pfd, name, rustix::fs::AtFlags::empty());
}

fn mkdir(pfd: &OwnedFd, name: &str, mode: u32, blame: &str) -> Result<(), Error> {
    match rustix::fs::mkdirat(pfd, name, Mode::from_bits_truncate(mode)) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(e) => Err(Error::Io(blame.into(), e.into())),
    }
}

pub(crate) fn beneath(rootfd: &OwnedFd, path: &str) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat2(
        rootfd,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    )
}

fn parent_fd(
    rootfd: &OwnedFd,
    parent: &str,
    manifest: &mut Vec<db::Entry>,
) -> Result<OwnedFd, Error> {
    if parent.is_empty() {
        return rootfd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| Error::Io(std::path::PathBuf::new(), e));
    }

    let open = |p: &str| beneath(rootfd, p);

    if let Ok(fd) = open(parent) {
        return Ok(fd);
    }

    // archive named a file without its directory
    let mut walked = String::new();
    for c in parent.split('/') {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(c);
        if open(&walked).is_ok() {
            continue;
        }
        let (up, name) = split(&walked);
        let upfd = if up.is_empty() {
            rootfd
                .as_fd()
                .try_clone_to_owned()
                .map_err(|e| Error::Io(std::path::PathBuf::new(), e))?
        } else {
            open(up).map_err(|e| Error::Io(up.into(), e.into()))?
        };
        mkdir(&upfd, name, 0o755, &walked)?;
        manifest.push(db::Entry {
            mode: 0o755,
            kind: db::Kind::Dir,
            path: walked.clone(),
        });
    }

    open(parent).map_err(|e| Error::Io(parent.into(), e.into()))
}

fn anon(e: io::Error) -> Error {
    Error::Io(std::path::PathBuf::new(), e)
}

fn resolve_or(
    root: &Path,
    path: &str,
    follow_final: bool,
    made: &HashMap<String, String>,
    blame: &str,
) -> Result<String, Error> {
    resolve(root, path, follow_final, made).map_err(|why| Error::Archive {
        path: blame.to_string(),
        why,
    })
}

fn tidy(p: &str) -> Result<Option<String>, &'static str> {
    let s = p.trim_end_matches('/');

    if s.starts_with('/') {
        return Err("absolute path");
    }
    if s.contains('\n') {
        return Err("newline in the path");
    }
    if s.is_empty() || s == "." {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

fn resolve(
    root: &Path,
    path: &str,
    follow_final: bool,
    made: &HashMap<String, String>,
) -> Result<String, &'static str> {
    let mut out: Vec<String> = Vec::new();
    // reversed, so pop() gives the next component
    let mut queue: Vec<String> = path.split('/').rev().map(String::from).collect();
    let mut hops = 0u32;

    while let Some(c) = queue.pop() {
        // "." covers the ./ prefix tar puts on every member
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            if out.pop().is_none() {
                return Err("would climb out of the root");
            }
            continue;
        }

        let mut cand = out.join("/");
        if !cand.is_empty() {
            cand.push('/');
        }
        cand.push_str(&c);

        if queue.is_empty() && !follow_final {
            out.push(c);
            break;
        }

        let target = match made.get(&cand) {
            Some(t) => Some(t.clone()),
            None => fs::read_link(root.join(&cand))
                .ok()
                .and_then(|t| t.to_str().map(String::from)),
        };

        match target {
            Some(t) => {
                hops += 1;
                if hops > MAX_HOPS {
                    return Err("too many symlinks on the way");
                }
                // absolute means inside the root
                if t.starts_with('/') {
                    out.clear();
                }
                for part in t.split('/').rev() {
                    queue.push(part.to_string());
                }
            }
            None => out.push(c),
        }
    }

    Ok(out.join("/"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tar(tar::Builder<Vec<u8>>);

    impl Tar {
        fn new() -> Tar {
            Tar(tar::Builder::new(Vec::new()))
        }

        fn raw(&mut self, path: &str, kind: EntryType, mode: u32, link: Option<&str>) {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(mode);
            h.set_entry_type(kind);
            h.set_path(path).unwrap();
            if let Some(t) = link {
                h.set_link_name(t).unwrap();
            }
            h.set_cksum();
            self.0.append(&h, &[][..]).unwrap();
        }

        // set_path normalises ./ away and refuses absolute and .. paths
        fn verbatim(mut self, p: &str, kind: EntryType) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(if kind == EntryType::Directory {
                0o755
            } else {
                0o644
            });
            h.set_entry_type(kind);
            h.set_path("placeholder").unwrap();
            let b = h.as_mut_bytes();
            b[..100].fill(0);
            b[..p.len()].copy_from_slice(p.as_bytes());
            h.set_cksum();
            self.0.append(&h, &[][..]).unwrap();
            self
        }

        fn evil(self, p: &str) -> Self {
            self.verbatim(p, EntryType::Regular)
        }

        fn file(mut self, p: &str) -> Self {
            self.raw(p, EntryType::Regular, 0o644, None);
            self
        }
        fn dir(mut self, p: &str) -> Self {
            self.raw(p, EntryType::Directory, 0o755, None);
            self
        }
        fn link(mut self, p: &str, t: &str) -> Self {
            self.raw(p, EntryType::Symlink, 0o777, Some(t));
            self
        }
        fn done(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-ar-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn paths(m: &[Member]) -> Vec<&str> {
        m.iter().map(|e| e.path.as_str()).collect()
    }

    fn why(r: Result<Vec<Member>, Error>) -> String {
        match r {
            Err(Error::Archive { why, .. }) => why.to_string(),
            other => panic!("wanted a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_leading_dot_slash_is_stripped() {
        let r = root("dotslash");
        let t = Tar::new()
            .verbatim("./usr/bin", EntryType::Directory)
            .verbatim("./usr/bin/foo", EntryType::Regular)
            .verbatim("././usr/bin/bar", EntryType::Regular)
            .done();

        let m = plan_reader(&r, &t[..]).unwrap();
        assert_eq!(paths(&m), ["usr/bin", "usr/bin/foo", "usr/bin/bar"]);
    }

    #[test]
    fn a_directory_lands_through_a_usr_merge_symlink() {
        let r = root("usrmerge");
        fs::create_dir_all(r.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", r.join("lib")).unwrap();

        let m = plan_reader(&r, &Tar::new().dir("lib").dir("lib/pkgconfig").done()[..]).unwrap();
        assert_eq!(paths(&m), ["usr/lib", "usr/lib/pkgconfig"]);
    }

    #[test]
    fn a_file_replaces_a_symlink_instead_of_writing_through_it() {
        let r = root("replace");
        fs::create_dir_all(r.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/real", r.join("usr/bin/foo")).unwrap();

        let m = plan_reader(&r, &Tar::new().file("usr/bin/foo").done()[..]).unwrap();
        assert_eq!(paths(&m), ["usr/bin/foo"]);
    }

    #[test]
    fn climbing_out_with_dot_dot_is_refused() {
        let r = root("climb");
        assert_eq!(
            why(plan_reader(
                &r,
                &Tar::new().evil("../etc/passwd").done()[..]
            )),
            "would climb out of the root"
        );
        assert_eq!(
            why(plan_reader(
                &r,
                &Tar::new().evil("usr/../../etc/passwd").done()[..]
            )),
            "would climb out of the root"
        );
    }

    #[test]
    fn an_absolute_member_path_is_refused() {
        let r = root("abs");
        assert_eq!(
            why(plan_reader(&r, &Tar::new().evil("/etc/passwd").done()[..])),
            "absolute path"
        );
    }

    #[test]
    fn a_symlink_the_archive_makes_cannot_be_used_to_escape() {
        let r = root("selfmade");
        let t = Tar::new()
            .link("evil", "../..")
            .file("evil/etc/passwd")
            .done();
        assert_eq!(why(plan_reader(&r, &t[..])), "would climb out of the root");
    }

    #[test]
    fn a_chain_of_archive_symlinks_is_followed_all_the_way() {
        let r = root("chain");
        let t = Tar::new()
            .link("a", "b")
            .link("b", "../../../")
            .file("a/etc/passwd")
            .done();
        assert_eq!(why(plan_reader(&r, &t[..])), "would climb out of the root");
    }

    #[test]
    fn an_absolute_symlink_target_stays_inside_the_root() {
        let r = root("absLink");
        let t = Tar::new().link("evil", "/etc").file("evil/passwd").done();
        let m = plan_reader(&r, &t[..]).unwrap();
        assert_eq!(paths(&m), ["evil", "etc/passwd"]);
    }

    #[test]
    fn a_newline_in_a_path_is_refused() {
        let r = root("newline");
        assert_eq!(
            why(plan_reader(
                &r,
                &Tar::new().evil("usr/bin/two\nlines").done()[..]
            )),
            "newline in the path"
        );
    }

    #[test]
    fn an_xattr_record_is_refused() {
        let r = root("xattr");
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("SCHILY.xattr.security.capability", &b"cap"[..])])
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o755);
        h.set_entry_type(EntryType::Regular);
        h.set_path("usr/bin/ping").unwrap();
        h.set_cksum();
        b.append(&h, &[][..]).unwrap();

        assert_eq!(
            why(plan_reader(&r, &b.into_inner().unwrap()[..])),
            "carries an xattr record"
        );
    }

    #[test]
    fn layer_two_refuses_to_leave_the_root() {
        let r = root("beneath");
        fs::create_dir_all(r.join("usr")).unwrap();
        let rootfd =
            rustix::fs::open(&r, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        let mut junk = Vec::new();

        assert!(
            parent_fd(&rootfd, "usr", &mut junk).is_ok(),
            "a real dir should open"
        );
        for bad in ["..", "../..", "/etc", "usr/../.."] {
            assert!(
                parent_fd(&rootfd, bad, &mut junk).is_err(),
                "openat2 let {bad:?} through, RESOLVE_BENEATH is not doing its job"
            );
        }
    }

    #[test]
    fn a_zstd_stream_is_required() {
        let r = root("notzst");
        let junk = r.join("junk.tar.zst");
        fs::write(&junk, b"this is not zstd").unwrap();
        assert_eq!(why(plan(&r, &junk)), "not a zstd stream");
    }

    #[test]
    fn a_symlink_loop_gives_up() {
        let r = root("loop");
        let t = Tar::new().link("a", "b").link("b", "a").file("a/x").done();
        assert_eq!(why(plan_reader(&r, &t[..])), "too many symlinks on the way");
    }

    #[test]
    fn a_device_node_is_refused() {
        let r = root("dev");
        let mut t = Tar::new();
        t.raw("dev/null", EntryType::Char, 0o666, None);
        assert_eq!(
            why(plan_reader(&r, &t.done()[..])),
            "not a file, directory, symlink or hard link"
        );
    }
}
