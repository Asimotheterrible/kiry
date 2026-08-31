// a build sees its declared closure and nothing else. the guarantee is the kernel's:
// a header outside the closure is not hidden, it is not there

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::mount::{self, MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::process;
use rustix::thread::{self, UnshareFlags};

use kiry_core::db;
use kiry_core::pkg::Dep;

pub struct Member {
    pub name: String,
    // direct deps hand over their whole manifest, everything else only its libraries
    pub direct: bool,
}

// every build gets a shell and a compiler without asking, so depends says what goes on
// top of one rather than restating it. absent means absent: a build that needs a shell
// then fails saying so
pub fn toolchain(root: &Path) -> Result<Vec<Dep>, String> {
    let f = root.join("etc/kiry/toolchain");
    let text = match fs::read_to_string(&f) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", f.display())),
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| Dep {
            name: l.to_string(),
            make: true,
        })
        .collect())
}

// a build dep is needed to build, so its own build deps are not. only runtime edges
// are followed outward
pub fn closure(root: &Path, target: &str, deps: &[Dep]) -> Result<Vec<Member>, String> {
    let mut out: Vec<Member> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();

    for d in toolchain(root)?.iter().chain(deps) {
        if seen.insert(d.name.clone()) {
            out.push(Member {
                name: d.name.clone(),
                direct: true,
            });
            queue.push(d.name.clone());
        }
    }

    while let Some(name) = queue.pop() {
        let rec = db::read(root, target, &name).map_err(|e| e.to_string())?;
        for d in rec.depends.iter().filter(|d| !d.make) {
            if seen.insert(d.name.clone()) {
                out.push(Member {
                    name: d.name.clone(),
                    direct: false,
                });
                queue.push(d.name.clone());
            }
        }
    }
    Ok(out)
}

// provides already records which installed files are shared libraries, so a transitive
// dep needs no filename guessing and no dev subpackage split
pub fn assemble(root: &Path, target: &str, members: &[Member], into: &Path) -> Result<(), String> {
    // the sysroot is the root's shape or nothing in it starts: PT_INTERP says
    // /lib/ld-musl-x86_64.so.1 and the loader lives in /usr/lib
    for (link, to) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        // usr/lib is musl and usr/lib64 is gnu, so this is not a second name for lib
        ("lib64", "usr/lib64"),
    ] {
        let dst = into.join(link);
        fs::create_dir_all(into.join(to)).map_err(|e| format!("{}: {e}", dst.display()))?;
        if !dst.exists() {
            symlink(to, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
    }

    for m in members {
        let rec = db::read(root, target, &m.name).map_err(|e| e.to_string())?;
        for e in &rec.manifest {
            if !m.direct && builds_against(&e.path) {
                continue;
            }
            place(root, into, e)?;
        }
    }

    for d in ["proc", "dev", "tmp", "src", "dest"] {
        fs::create_dir_all(into.join(d)).map_err(|e| format!("{d}: {e}"))?;
    }
    Ok(())
}

// manifest paths are relative to the root and carry no leading slash, so an absolute
// link target just loses its slash and a relative one resolves against its own directory
// what a configure script reads to decide a feature is there. a transitive dependency
// keeps everything else -- autoconf is not autoconf without the perl that runs it, and
// perl is three levels of module tree, not one soname
fn builds_against(path: &str) -> bool {
    path.starts_with("usr/include/")
        || path.ends_with(".pc")
        || path.ends_with(".m4")
        || path.ends_with(".cmake")
}

fn place(root: &Path, into: &Path, e: &db::Entry) -> Result<(), String> {
    let dst = into.join(&e.path);
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).map_err(|x| format!("{}: {x}", p.display()))?;
    }
    match &e.kind {
        db::Kind::Dir => {
            fs::create_dir_all(&dst).map_err(|x| format!("{}: {x}", dst.display()))?;
        }
        db::Kind::Link(t) => {
            let _ = fs::remove_file(&dst);
            symlink(t, &dst).map_err(|x| format!("{}: {x}", dst.display()))?;
            return Ok(());
        }
        db::Kind::Hard(t) => {
            let _ = fs::remove_file(&dst);
            fs::hard_link(into.join(t), &dst).map_err(|x| format!("{}: {x}", dst.display()))?;
        }
        db::Kind::File(_) => clone(&root.join(&e.path), &dst)?,
    }
    fs::set_permissions(&dst, fs::Permissions::from_mode(e.mode))
        .map_err(|x| format!("{}: {x}", dst.display()))
}

// FICLONE is a metadata operation, so a closure of twenty thousand files costs no data
// movement at all. anything that is not btrfs falls back to reading the bytes
fn clone(src: &Path, dst: &Path) -> Result<(), String> {
    let from = fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let to = fs::File::create(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    if rustix::fs::ioctl_ficlone(&to, &from).is_ok() {
        return Ok(());
    }
    drop(to);
    fs::copy(src, dst)
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", dst.display()))
}

// a freshly exec'd process has exactly one thread, and CLONE_NEWUSER refuses to unshare
// from a process that has more than one. that is why the build enters through a new kiry
// rather than through this one, which is free to grow threads later
pub fn init() -> Result<(), String> {
    let work = PathBuf::from(
        std::env::var("KIRY_SANDBOX").map_err(|_| "KIRY_SANDBOX is not set".to_string())?,
    );
    let root = work.join("sysroot");

    let (uid, gid) = (process::getuid().as_raw(), process::getgid().as_raw());
    // SAFETY: rustix documents one hazard, UnshareFlags::FILES leaving a thread holding
    // descriptors from a table it no longer shares. these three flags are not that one,
    // and a process this new has no second thread to hand a descriptor to anyway
    unsafe {
        thread::unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWNET)
    }
    .map_err(|e| format!("unshare: {e}"))?;
    write("/proc/self/setgroups", "deny")?;
    write("/proc/self/gid_map", &format!("0 {gid} 1"))?;
    write("/proc/self/uid_map", &format!("0 {uid} 1"))?;

    // ours alone, so nothing below leaks back out to the host
    mount::mount_change(
        "/",
        MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )
    .map_err(|e| format!("private /: {e}"))?;
    mount::mount_bind(&root, &root).map_err(|e| format!("bind sysroot: {e}"))?;

    mount::mount_bind(work.join("src"), root.join("src")).map_err(|e| format!("src: {e}"))?;
    mount::mount_bind(work.join("dest"), root.join("dest")).map_err(|e| format!("dest: {e}"))?;
    // a fresh procfs wants CAP_SYS_ADMIN in the user namespace owning the pid namespace,
    // and there is none here, so the host's is bound in. recursive because a user
    // namespace locks the mounts it inherited and refuses to bind one on its own
    mount::mount_bind_recursive("/proc", root.join("proc"))
        .map_err(|e| format!("bind proc: {e}"))?;
    mount::mount(
        "tmpfs",
        root.join("tmp"),
        "tmpfs",
        MountFlags::empty(),
        None,
    )
    .map_err(|e| format!("mount tmp: {e}"))?;
    devices(&root)?;

    // the closure is readable and nothing more. src, dest and tmp are mounts on top of
    // it, so they stay writable
    mount::mount_remount(&root, MountFlags::BIND | MountFlags::RDONLY, "")
        .map_err(|e| format!("seal sysroot: {e}"))?;

    process::chdir(&root).map_err(|e| format!("chdir sysroot: {e}"))?;
    process::pivot_root(".", ".").map_err(|e| format!("pivot_root: {e}"))?;
    mount::unmount(".", UnmountFlags::DETACH).map_err(|e| format!("detach old root: {e}"))?;

    process::setrlimit(
        process::Resource::Nofile,
        process::Rlimit {
            current: Some(4096),
            maximum: Some(4096),
        },
    )
    .map_err(|e| format!("rlimit: {e}"))?;
    // a build that allocates without bound takes the machine down rather than itself,
    // and the process the oom killer picks is whatever else was running. binutils'
    // configure probe for ada handed clang a .adb and it ran until there was no memory
    // left. RLIMIT_DATA rather than AS: since 4.7 it covers anonymous mmap, which is
    // what a runaway allocator uses, and it does not count address space a linker only
    // reserves
    if let Some(cap) = memcap() {
        process::setrlimit(
            process::Resource::Data,
            process::Rlimit {
                current: Some(cap),
                maximum: Some(cap),
            },
        )
        .map_err(|e| format!("rlimit: {e}"))?;
    }

    let cwd = crate::unpacked(Path::new("/src"));
    process::chdir(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;

    Err(format!(
        "exec sh: {}",
        Command::new("sh").arg("-e").arg("/build").exec()
    ))
}

// enough for a build to run and nothing that reads or writes hardware
fn devices(root: &Path) -> Result<(), String> {
    let dev = root.join("dev");
    mount::mount(
        "tmpfs",
        &dev,
        "tmpfs",
        MountFlags::empty(),
        Some(c"mode=755"),
    )
    .map_err(|e| format!("mount dev: {e}"))?;
    for n in ["null", "zero", "full", "random", "urandom"] {
        let at = dev.join(n);
        fs::write(&at, "").map_err(|e| format!("{}: {e}", at.display()))?;
        mount::mount_bind(format!("/dev/{n}"), &at).map_err(|e| format!("bind {n}: {e}"))?;
    }
    Ok(())
}

// half of what the machine has, per process. KIRY_MEM is megabytes and 0 turns it off
fn memcap() -> Option<u64> {
    if let Ok(v) = std::env::var("KIRY_MEM") {
        return match v.parse::<u64>() {
            Ok(0) | Err(_) => None,
            Ok(mb) => Some(mb * 1024 * 1024),
        };
    }
    let info = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = info
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches(" kB")
        .parse::<u64>()
        .ok()?;
    Some(kb * 1024 / 2)
}

fn write(path: &str, body: &str) -> Result<(), String> {
    fs::write(path, body).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use kiry_core::pkg::Version;
    use std::path::PathBuf;

    const T: &str = "x86_64-gnu";
    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-sb-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn dep(name: &str, make: bool) -> Dep {
        Dep {
            name: name.to_string(),
            make,
        }
    }

    // a package with a header, a pkg-config file and a library, all really on disk
    fn install(root: &Path, name: &str, deps: &[Dep]) {
        let lib = format!("usr/lib64/lib{name}.so.1");
        let paths = [
            format!("usr/include/{name}.h"),
            format!("usr/lib64/pkgconfig/{name}.pc"),
            lib.clone(),
        ];
        for p in &paths {
            let f = root.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(&f, format!("{name} {p}\n")).unwrap();
        }
        db::write(
            root,
            &db::Installed {
                name: name.to_string(),
                target: T.to_string(),
                version: Version::parse("1.0 1").unwrap(),
                depends: deps.to_vec(),
                manifest: paths
                    .iter()
                    .map(|p| db::Entry {
                        mode: 0o644,
                        kind: db::Kind::File(SHA.to_string()),
                        path: p.clone(),
                    })
                    .collect(),
            },
        )
        .unwrap();
        db::write_provides(
            root,
            T,
            name,
            &[db::Provide {
                soname: format!("lib{name}.so.1"),
                versioned: false,
                path: lib,
            }],
        )
        .unwrap();
    }

    #[test]
    fn the_toolchain_is_in_every_closure_without_a_recipe_naming_it() {
        let root = scratch("toolchain");
        install(&root, "busybox", &[]);
        install(&root, "zlib", &[]);
        fs::create_dir_all(root.join("etc/kiry")).unwrap();
        fs::write(
            root.join("etc/kiry/toolchain"),
            "# a shell, at least\nbusybox\n",
        )
        .unwrap();

        let c = closure(&root, T, &[dep("zlib", false)]).unwrap();
        let names: Vec<&str> = c.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"busybox"),
            "no shell in the sandbox: {names:?}"
        );
        assert!(names.contains(&"zlib"), "{names:?}");
    }

    #[test]
    fn no_toolchain_file_means_no_toolchain() {
        let root = scratch("notoolchain");
        install(&root, "zlib", &[]);
        assert_eq!(closure(&root, T, &[dep("zlib", false)]).unwrap().len(), 1);
    }

    #[test]
    fn a_build_dependency_does_not_drag_its_own_build_dependencies_in() {
        let root = scratch("closure");
        install(&root, "gcc", &[dep("gccdep", true)]);
        install(&root, "gccdep", &[]);
        install(&root, "zlib", &[]);

        let c = closure(&root, T, &[dep("gcc", true), dep("zlib", false)]).unwrap();
        let names: Vec<&str> = c.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"gcc") && names.contains(&"zlib"),
            "{names:?}"
        );
        assert!(
            !names.contains(&"gccdep"),
            "a make dep's make deps came along"
        );
    }

    #[test]
    fn a_cycle_in_the_recorded_depends_does_not_hang() {
        let root = scratch("cycle");
        install(&root, "freetype", &[dep("harfbuzz", false)]);
        install(&root, "harfbuzz", &[dep("freetype", false)]);

        let c = closure(&root, T, &[dep("freetype", false)]).unwrap();
        assert_eq!(c.len(), 2);
    }

    // the two layer rule: headers come from what the recipe named, and nothing else
    #[test]
    fn a_transitive_dependency_hands_over_libraries_and_no_headers() {
        let root = scratch("layers");
        install(&root, "png", &[dep("zlib", false)]);
        install(&root, "zlib", &[]);

        let c = closure(&root, T, &[dep("png", false)]).unwrap();
        let into = root.join("sysroot");
        assemble(&root, T, &c, &into).unwrap();

        assert!(
            into.join("usr/include/png.h").exists(),
            "direct dep lost its header"
        );
        assert!(into.join("usr/lib64/pkgconfig/png.pc").exists());
        assert!(into.join("usr/lib64/libpng.so.1").exists());

        assert!(
            into.join("usr/lib64/libzlib.so.1").exists(),
            "closure lost a library"
        );
        assert!(
            !into.join("usr/include/zlib.h").exists(),
            "a transitive dependency's header reached the build, which is what makes a \
             feature turn itself on that nobody asked for"
        );
        assert!(!into.join("usr/lib64/pkgconfig/zlib.pc").exists());
    }

    #[test]
    fn nothing_outside_the_closure_is_placed_at_all() {
        let root = scratch("outside");
        install(&root, "png", &[]);
        install(&root, "openssl", &[]);

        let c = closure(&root, T, &[dep("png", false)]).unwrap();
        let into = root.join("sysroot");
        assemble(&root, T, &c, &into).unwrap();

        assert!(into.join("usr/lib64/libpng.so.1").exists());
        assert!(
            !into.join("usr/lib64/libopenssl.so.1").exists(),
            "an installed package nobody declared turned up in the sysroot"
        );
    }
}
