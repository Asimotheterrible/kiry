mod convert;
mod sandbox;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use kiry_core::pkg::{Dep, Package};
use kiry_core::{db, elf, install, pkg};

macro_rules! say {
    ($($a:tt)*) => {
        if writeln!(std::io::stdout(), $($a)*).is_err() {
            std::process::exit(0);
        }
    };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => usage(),
        Some("--version") => say!("kiry {}", env!("CARGO_PKG_VERSION")),
        Some("b") => build_cmd(&args[1..]),
        Some("i") => install_cmd(&args[1..]),
        Some("r") => remove_cmd(&args[1..]),
        Some("l") => list_cmd(&args[1..]),
        Some("doctor") => doctor_cmd(&args[1..]),
        Some("owns") => owns_cmd(&args[1..]),
        Some("rebuild") => rebuild_cmd(&args[1..]),
        Some("convert") => convert_cmd(&args[1..]),
        Some("sandbox") => {
            if let Err(e) = sandbox::init() {
                die(e);
            }
        }
        Some(dir) => show(dir),
        None => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    say!("usage: kiry b [--root DIR] [--target T] [-v] <package dir>...");
    say!("       kiry i [--root DIR] [--force] <archive>...");
    say!("       kiry r [--root DIR] [--force] <pkg>...");
    say!("       kiry l [--root DIR]");
    say!("       kiry doctor [--root DIR] [--files] [--orphans]");
    say!("       kiry owns [--root DIR] <path>...");
    say!("       kiry rebuild [--root DIR] [-n]");
    say!("       kiry convert [-n] <APKBUILD>... <into DIR>");
    say!("       kiry sandbox                    internal: build inside its closure");
    say!("       kiry <package dir>");
}

fn die(msg: String) -> ! {
    eprintln!("kiry: {msg}");
    std::process::exit(1);
}

fn opts(args: &[String]) -> (PathBuf, bool, Vec<String>) {
    let mut root = std::env::var("KIRY_ROOT").unwrap_or_else(|_| "/".into());
    let mut force = false;
    let mut rest = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" => force = true,
            "--root" => match it.next() {
                Some(r) => root = r.clone(),
                None => die("--root wants a path".into()),
            },
            _ => rest.push(a.clone()),
        }
    }
    (PathBuf::from(root), force, rest)
}

// --root defaults to / and most of these commands write. checked only where one
// mutates, so l and doctor still answer for the running system
//
// what is refused is a / that no kiry installed anything into, which is the machine
// this gets built on. on the installed system / is the root it owns and there is
// nothing to guard against
fn writes(root: &Path) {
    let at = root.canonicalize();
    let at = at.as_deref().unwrap_or(root);
    if at == Path::new("/")
        && !Path::new("/usr/lib/kiry/db/installed").is_dir()
        && std::env::var_os("KIRY_ROOT_REALLY").is_none()
    {
        die("refusing to write to /, which no kiry owns. set KIRY_ROOT_REALLY=1 to mean it".into());
    }
}

fn build_cmd(args: &[String]) {
    let mut want = None;
    let mut verbose = false;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" => verbose = true,
            "--target" => match it.next() {
                Some(t) => want = Some(t.clone()),
                None => die("--target wants a name".into()),
            },
            _ => rest.push(a.clone()),
        }
    }

    let (root, _, dirs) = opts(&rest);
    writes(&root);
    if dirs.is_empty() {
        die("nothing to build".into());
    }

    for d in &dirs {
        let p = match pkg::load(Path::new(d)) {
            Ok(p) => p,
            Err(e) => die(e.to_string()),
        };
        let targets = match &want {
            Some(t) if !p.targets.contains(t) => die(format!("{} does not build for {t}", p.name)),
            Some(t) => vec![t.clone()],
            None => p.targets.clone(),
        };
        if let Err(e) = build(&root, &p, &targets, verbose) {
            die(e);
        }
    }

    say!("cached {}", root.join("var/kiry/cache").display());
}

fn build(
    root: &Path,
    p: &Package,
    targets: &[String],
    verbose: bool,
) -> Result<Vec<PathBuf>, String> {
    let srcs = sources(root, p)?;
    let hash = recipe_hash(p, &srcs)?;

    let mut built = Vec::new();
    for t in targets {
        let start = Instant::now();
        let work = compile(root, p, t, &srcs, verbose)?;
        say!("{} {} {t} ok {}", p.name, p.version.upstream, took(start));
        built.push((t.clone(), work));
    }

    let mut ready = Vec::new();
    for (t, work) in &built {
        ready.push((t.clone(), pack(root, p, t, work)?));
    }

    // sidecar lands after the rename. ahead of it, a target failing to tar leaves a
    // .meta for an artifact that never arrives
    for (t, (art, part)) in &ready {
        fs::rename(part, art).map_err(|e| format!("{}: {e}", art.display()))?;
        meta(p, t, &hash, art)?;
    }
    for (_, work) in &built {
        let _ = fs::remove_dir_all(work);
    }
    Ok(ready.into_iter().map(|(_, (art, _))| art).collect())
}

// the gnu tier is a library layer. its recipes are the same ones the musl target uses,
// so they also build the binaries and drop the config files that come with a library --
// a second bzip2, a second CA.pl -- and /usr/bin and /etc have one owner across every
// target, so the install is refused over a file nobody wanted twice
//
// the same holds for the documentation under usr/share. that path is musl's datadir --
// the target's is inside the sysroot -- and the man page, the licence and the html
// manual are byte-identical between the two builds anyway, so the second one is refused
// over a file it had no reason to write. anything under usr/share a gnu build genuinely
// needs, a pkg-config file or a cmake package, gets relocated by the recipe instead
//
// dropped rather than refused, because the alternative is the same three lines in
// twenty recipes and each one able to forget. printed rather than silent, because a
// build that quietly discards what it just made is the kind of thing that gets
// diagnosed twice
// the gnu ld.so.cache is built by running these, and steam wants a cache to exist
const KEPT: &[&str] = &[
    // DESTDIR has no usrmerge symlink, so glibc's sbin is a real directory here
    "sbin/ldconfig",
    "usr/sbin/ldconfig",
    "etc/kiry/hooks.d/50-ldconfig",
];

fn trim(dest: &Path, t: &str, name: &str) -> Result<(), String> {
    if !t.ends_with("gnu") {
        return Ok(());
    }

    let aside = dest.join(".kiry-kept");
    let mut kept = Vec::new();
    for k in KEPT {
        let at = dest.join(k);
        if !at.exists() {
            continue;
        }
        let to = aside.join(k);
        mkdirs(to.parent().unwrap_or(&aside))?;
        fs::rename(&at, &to).map_err(|e| format!("{}: {e}", at.display()))?;
        kept.push(*k);
    }

    let mut dropped = Vec::new();
    for d in [
        "usr/bin",
        "usr/sbin",
        // /bin and /sbin are symlinks into /usr here, so a recipe that installs to
        // either lands on the same owned path by a different name
        "bin",
        "sbin",
        "etc",
        "usr/lib64/udev",
        "usr/share/man",
        "usr/share/doc",
        "usr/share/info",
        "usr/share/licenses",
        "usr/share/locale",
    ] {
        let at = dest.join(d);
        if at.exists() {
            fs::remove_dir_all(&at).map_err(|e| format!("{}: {e}", at.display()))?;
            dropped.push(d);
        }
    }

    for k in &kept {
        let at = dest.join(k);
        mkdirs(at.parent().unwrap_or(dest))?;
        fs::rename(aside.join(k), &at).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    if aside.exists() {
        fs::remove_dir_all(&aside).map_err(|e| format!("{}: {e}", aside.display()))?;
    }

    if !dropped.is_empty() {
        say!("{name} {t} dropped {}", dropped.join(" "));
    }
    if !kept.is_empty() {
        say!("{name} {t} kept {}", kept.join(" "));
    }
    Ok(())
}

fn sources(root: &Path, p: &Package) -> Result<Vec<(String, PathBuf, String)>, String> {
    let cache = root.join("var/kiry/cache/sources");
    mkdirs(&cache)?;

    let mut out = Vec::new();
    for (i, s) in p.sources.iter().enumerate() {
        let (name, from) = filename(s)?;
        let path = if from.contains("://") {
            let dst = cache.join(name);
            if !dst.exists() {
                grab(from, &dst)?;
            }
            dst
        } else {
            p.dir.join(from)
        };

        let sum = sha(&path)?;
        match p.checksums.get(i) {
            Some(want) if want != &sum => {
                return Err(format!(
                    "{}: checksum is {sum}, recipe says {want}",
                    path.display()
                ))
            }
            Some(_) => {}
            None => eprintln!("kiry: {}: no checksum, sha256 is {sum}", path.display()),
        }
        out.push((name.to_string(), path, sum));
    }
    Ok(out)
}

// name::url says what to call the thing, for the many urls that end in download or v1.2
// or nothing at all. it is a file name in a shared cache, so it cannot be a path
fn filename(s: &str) -> Result<(&str, &str), String> {
    let (name, from) = match s.split_once("::") {
        Some((n, u)) if !n.contains(['/', ':']) => (n, u),
        _ => (s.rsplit('/').next().unwrap_or(""), s),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("{s}: no file name in that"));
    }
    Ok((name, from))
}

fn grab(url: &str, dst: &Path) -> Result<(), String> {
    let mut part = dst.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let tmpl = std::env::var("KIRY_FETCH").unwrap_or_else(|_| "curl -fL --retry 3 -o %o %u".into());
    let mut words = tmpl.split_whitespace();
    let Some(prog) = words.next() else {
        return Err("KIRY_FETCH is empty".into());
    };

    let mut c = Command::new(prog);
    for w in words {
        c.arg(
            w.replace("%o", &part.display().to_string())
                .replace("%u", url),
        );
    }
    run(&mut c, url)?;

    fs::rename(&part, dst).map_err(|e| format!("{}: {e}", dst.display()))
}

fn compile(
    root: &Path,
    p: &Package,
    t: &str,
    srcs: &[(String, PathBuf, String)],
    verbose: bool,
) -> Result<PathBuf, String> {
    let work = root.join("var/kiry/stage").join(format!(
        "{}-{}-{}.{t}",
        p.name, p.version.upstream, p.version.rev
    ));
    let _ = fs::remove_dir_all(&work);

    let src = work.join("src");
    let dest = work.join("dest");
    mkdirs(&src)?;
    mkdirs(&dest)?;

    for (name, path, _) in srcs {
        let name = name.as_str();
        if tarball(name) {
            // as root tar restores the archive's own uids, and the sandbox maps one id,
            // so a build running as root gets a /src it cannot chown inside the namespace
            run(
                Command::new("tar")
                    .arg("--no-same-owner")
                    .arg("-xf")
                    .arg(path)
                    .arg("-C")
                    .arg(&src),
                name,
            )?;
        } else {
            fs::copy(path, src.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
    }

    let script = p.dir.join("build");
    if !script.is_file() {
        return Err(format!("{}: no build script", p.dir.display()));
    }

    let sysroot = work.join("sysroot");
    let deps: Vec<Dep> = p.depends.iter().filter(|d| d.applies(t)).cloned().collect();
    let members = sandbox::closure(root, t, &deps)?;
    sandbox::assemble(root, &members, &sysroot)?;

    fs::copy(&script, sysroot.join("build")).map_err(|e| format!("build script: {e}"))?;
    let share = sysroot.join("usr/share/kiry");
    mkdirs(&share)?;
    fs::write(share.join("lib.sh"), LIB_SH).map_err(|e| format!("lib.sh: {e}"))?;
    // files rather than shell functions because ash refuses a hyphen in a function name,
    // and generated because libdir is the target's
    let bin = sysroot.join("usr/bin");
    mkdirs(&bin)?;
    // muon does not read argv0, so compile and install need a name to reach it by too
    let cmake_toolchain = share.join("toolchain.cmake");
    fs::write(
        &cmake_toolchain,
        TOOLCHAIN_CMAKE
            .replace("@LIBDIR@", libdir(t).trim_start_matches("/usr/"))
            .replace("@INCLUDEDIR@", incdir(t).trim_start_matches("/usr/"))
            .replace("@DATADIR@", datadir(t).trim_start_matches("/usr/"))
            .replace("@FIND@", if t.ends_with("gnu") { TOOLCHAIN_FIND } else { "" }),
    )
    .map_err(|e| format!("toolchain.cmake: {e}"))?;

    // autoconf reads CONFIG_SITE before it decides anything, so a gnu package lands in
    // /usr/lib64 without a single recipe saying --libdir. same standing as the cmake
    // toolchain file: the target decides the layout, not the recipe
    let config_site = share.join("config.site");
    fs::write(
        &config_site,
        CONFIG_SITE
            .replace("@LIBDIR@", libdir(t))
            .replace("@INCLUDEDIR@", incdir(t))
            .replace("@DATADIR@", datadir(t)),
    )
    .map_err(|e| format!("config.site: {e}"))?;

    // cc is clang and clang defaults to the triple it was built for, which is the
    // host's. a plain ./configure and a plain Makefile call cc and never look at CHOST,
    // so without this a gnu build links musl, installs into usr/lib and reports success
    //
    // the gnu sysroot is where glibc puts its headers -- /usr/include is musl's -- and
    // core/glibc installs the usr/lib64 name inside it that makes the libraries
    // reachable from there too
    // compiler-rt and libunwind in this tree are built for musl, so a gnu link asking
    // for clang's defaults comes back with -lunwind missing. libgcc is what the gnu
    // substrate ships, and core/gcc-runtime keeps its archives and linker script for
    // exactly this
    // the sysroot holds the headers and the runtime libraries are outside it, at
    // /usr/lib64, so -l has to be told. -lstdc++ is the one that finds this out: it has
    // no pkg-config file to carry a -L, so elfutils' demangler probe linked against
    // nothing and configure decided libstdc++ had no __cxa_demangle in it
    //
    // -stdlib on the c compiler too, because this clang defaults to libc++ and rewrites
    // a literal -lstdc++ to match its default -- so the probe went looking for libc++,
    // which is llvm-runtimes' and built for musl
    let sysroot_arg = if t.ends_with("gnu") {
        " --sysroot=/usr/x86_64-linux-gnu --start-no-unused-arguments \
-L/usr/lib64 -Wl,-rpath-link,/usr/lib64 -stdlib=libstdc++ \
--rtlib=libgcc --unwindlib=libgcc --end-no-unused-arguments"
    } else {
        ""
    };
    let cc = CC_WRAPPER
        .replace("@TRIPLE@", triple(t))
        .replace("@SYSROOT@", sysroot_arg);
    // no directory means no c++ in this closure, which a c package is entitled to. the
    // flags come out empty and a c++ compile then fails at its first include, which says
    // more than a wrapper that refused to be written
    let cxx_extra = match gcc_headers(&sysroot) {
        Some(d) => format!(
            " -cxx-isystem {d} -cxx-isystem {d}/x86_64-linux-gnu -cxx-isystem {d}/backward"
        ),
        None => String::new(),
    };

    for (n, body) in [
        ("abuild-meson", meson_wrapper(t)),
        ("abuild-muon", meson_wrapper(t)),
        (
            "meson",
            "#!/bin/sh -e\nexec muon meson \"$@\"\n".to_string(),
        ),
        ("kiry-cc", cc.replace("@REAL@", "clang")),
        (
            "kiry-c++",
            cc.replace("@REAL@", "clang++").replace("\"$@\"", &format!("{cxx_extra} \"$@\"")),
        ),
    ] {
        let at = bin.join(n);
        fs::write(&at, &body).map_err(|e| format!("{n}: {e}"))?;
        fs::set_permissions(&at, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{n}: {e}"))?;
    }

    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut c = Command::new(me);
    c.arg("sandbox")
        // an inherited CFLAGS or PYTHONPATH would change a build without appearing
        // in any recipe, and the closure is meant to be the whole of what one sees
        .env_clear()
        .env("KIRY_SANDBOX", abs(&work)?)
        .env("HOME", "/src")
        .env("TMPDIR", "/tmp")
        .env("TERM", "dumb")
        // c.utf8 rather than c: musl has it, and python trips over unicode paths
        // under plain c
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC")
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("DESTDIR", "/dest")
        // every apkbuild build() leans on abuild exporting this and never says -j itself
        .env("MAKEFLAGS", format!("-j{}", jobs()))
        // build is this machine, host is what the output has to run on. they are the
        // same triple for a musl package built on musl and they are not for a gnu one,
        // which is the only cross this tree does. told they matched, gcc's configure
        // read musl's headers as glibc's and compiled a call to mallinfo2
        .env("CBUILD", triple(&sandbox::host()))
        .env("CHOST", triple(t))
        .env("CTARGET", triple(t))
        // the machine half of the target, which is what a case branch switches on
        .env("CARCH", t.split('-').next().unwrap_or(t))
        .env("CTARGET_ARCH", t.split('-').next().unwrap_or(t))
        .env("CMAKE_TOOLCHAIN_FILE", "/usr/share/kiry/toolchain.cmake")
        .env("CONFIG_SITE", "/usr/share/kiry/config.site")
        // for the configures that are not autoconf and read no site file. a handful of
        // recipes pass these as --libdir and friends; the rest never see them
        .env("KIRY_LIBDIR", libdir(t))
        .env("KIRY_INCLUDEDIR", incdir(t))
        .env("KIRY_DATADIR", datadir(t))
        // pkgconf's built-in path is /usr/lib/pkgconfig, which is musl's. a gnu build
        // asking for libdrm would be handed the musl one's cflags and libs and link
        // against it without a word, which is the whole failure the split directory
        // exists to prevent, arriving through a text file
        .env(
            "PKG_CONFIG_LIBDIR",
            format!("{}/pkgconfig:{}/pkgconfig", libdir(t), datadir(t)),
        )
        // named rather than left to cc, because a configure that goes looking finds
        // clang either way and the point is the flags that come with it
        .env("CC", "kiry-cc")
        .env("CXX", "kiry-c++")
        .env("KIRY_SRCDIR", "/src")
        .env("KIRY_TARGET", t)
        .env("KIRY_NAME", &p.name)
        .env("KIRY_VERSION", &p.version.upstream)
        .env("KIRY_REV", p.version.rev.to_string());

    // read on the far side of the clear, so they have to cross by name. without the
    // cap a build that allocates without bound takes the machine down, not itself
    for k in ["KIRY_MEM", "KIRY_HOST"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }

    // the log is written either way. -v only decides whether you also watch it
    let log = root.join("var/kiry/log").join(format!(
        "{}-{}-{}.{t}.log",
        p.name, p.version.upstream, p.version.rev
    ));
    if !verbose {
        mkdirs(&root.join("var/kiry/log"))?;
        let out = fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("{}: {e}", log.display()))?;
        c.stdout(out).stderr(err);
    }

    match c.status() {
        Ok(s) if s.success() => {
            trim(&dest, t, &p.name)?;
            Ok(work)
        }
        Ok(_) if verbose => Err(format!("{} {t}: build failed", p.name)),
        Ok(_) => Err(format!(
            "{} {t}: build failed, log is {}",
            p.name,
            log.display()
        )),
        Err(e) => Err(format!("{} {t}: {e}", p.name)),
    }
}

// one top directory and nothing else means the build starts inside it, which is
// what every recipe expects
// the abuild helpers a converted body still calls. generated rather than packaged so it
// cannot drift from the binary that writes it
const LIB_SH: &str = "\
default_prepare() {
\tfor _s in $source; do
\t\t# name::url says what a source is called, so the name decides both whether
\t\t# this is a patch and what to look for it under
\t\t_f=${_s%%::*}
\t\t_f=${_f##*/}
\t\tcase \"$_f\" in
\t\t*.patch) patch ${patch_args:--p1} -i \"$srcdir/$_f\" || return 1 ;;
\t\t*.patch.gz) gzip -cd \"$srcdir/$_f\" | patch ${patch_args:--p1} || return 1 ;;
\t\t*.patch.xz) xz -cd \"$srcdir/$_f\" | patch ${patch_args:--p1} || return 1 ;;
\t\t*.patch.bz2) bzip2 -cd \"$srcdir/$_f\" | patch ${patch_args:--p1} || return 1 ;;
\t\tesac
\tdone
}

msg() { echo \">>> $*\"; }
warning() { echo \">>> WARNING: $*\" >&2; }
error() { echo \">>> ERROR: $*\" >&2; }
die() { error \"$@\"; exit 1; }

# abuild refreshes config.sub so an old one recognises musl. a tarball new enough to
# package usually already does, and one that is not says so at configure
update_config_sub() { :; }
update_config_guess() { :; }

# kiry builds one package and does not run upstream test suites, so both are always no
subpackages_has() { return 1; }
want_check() { return 1; }
options_has() { case \" $options \" in *\" $1 \"*) return 0 ;; esac; return 1; }
";

// autoconf sources this before it decides anything, so libdir follows the target
// without a recipe passing --libdir. hand-rolled configures do not read it and still
// need the flag; that is a handful of recipes rather than all of them
const CONFIG_SITE: &str = "\
libdir=@LIBDIR@
includedir=@INCLUDEDIR@
datarootdir=@DATADIR@
";

// cc is clang and clang defaults to the triple it was built for. a plain ./configure
// and a plain Makefile call cc and never look at CHOST, so a gnu build without this
// links musl and installs into usr/lib and says it worked
//
// the no-unused-arguments pair is not decoration. --rtlib and --unwindlib mean nothing
// to a -c compile and clang warns about each one, and a configure that probes by
// checking whether the compiler printed anything reads that as the feature missing
// zlib decided it had no strerror this way, then failed to build against its own
// conclusion
//
// exec rather than a shell function so $CC being split on whitespace behaves, and -e so
// a missing clang says so instead of returning success
const CC_WRAPPER: &str = "\
#!/bin/sh -e
exec @REAL@ --target=@TRIPLE@@SYSROOT@ \"$@\"
";

// abuild's wrapper, deviating twice: auto_features stays auto because the closure is
// what a detection can see, and libdir follows the target
// cmake's GNUInstallDirs picks lib64 on any 64-bit linux that is not debian, arch or
// alpine -- it looks for /etc/alpine-release, which this tree deliberately does not have
// usr/lib64 is the gnu tier, so a musl package landing there is the cross-tier bug the
// split directory exists to prevent. a toolchain file is the documented way in, and
// cmake reads its path from the environment
const TOOLCHAIN_CMAKE: &str = "\
set(CMAKE_INSTALL_LIBDIR \"@LIBDIR@\")
set(CMAKE_INSTALL_INCLUDEDIR \"@INCLUDEDIR@\")
set(CMAKE_INSTALL_DATAROOTDIR \"@DATADIR@\")
@FIND@";

// find_package looks under the prefixes cmake knows about, and the gnu tier's headers
// and cmake packages are in the sysroot, which is not one of them -- vulkan-loader asks
// for VulkanHeaders and is told it is not installed while it sits in the closure
// programs stay off it: a build tool that runs here is the host's
const TOOLCHAIN_FIND: &str = "\
set(CMAKE_PREFIX_PATH \"/usr/x86_64-linux-gnu/usr\")
set(CMAKE_FIND_ROOT_PATH \"/usr/x86_64-linux-gnu\")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
";

const MESON: &str = "\
#!/bin/sh -e
exec muon meson setup \\
\t-Dprefix=/usr \\
\t-Dlibdir=@LIBDIR@ \\
\t-Dlibexecdir=/usr/libexec \\
\t-Dbindir=/usr/bin \\
\t-Dsbindir=/usr/sbin \\
\t-Dincludedir=@INCLUDEDIR@ \\
\t-Ddatadir=@DATADIR@ \\
\t-Dmandir=@DATADIR@/man \\
\t-Dlocaledir=@DATADIR@/locale \\
\t-Dsysconfdir=/etc \\
\t-Dlocalstatedir=/var \\
\t-Dsharedstatedir=/var/lib \\
\t-Dbuildtype=plain \\
\t-Dauto_features=auto \\
\t-Dwrap_mode=nodownload \\
\t-Ddefault_library=shared \\
\t-Db_lto=false \\
\t-Db_staticpic=true \\
\t-Db_pie=true \\
\t-Dwerror=false \\
\t\"$@\"
";

// /usr/include is musl's and a glibc header set over it would make every later musl
// build compile against the wrong libc, so the gnu tier's headers live in the sysroot
// core/glibc already populates. the same goes for everything arch-independent: two
// targets of one recipe ship the same man page, and one filesystem has one owner, so
// without this the second target is refused at install over a file that is genuinely
// identical. libraries stay in /usr/lib64, which is where the loader looks
fn incdir(t: &str) -> &'static str {
    if t.ends_with("gnu") {
        "/usr/x86_64-linux-gnu/usr/include"
    } else {
        "/usr/include"
    }
}

fn datadir(t: &str) -> &'static str {
    if t.ends_with("gnu") {
        "/usr/x86_64-linux-gnu/usr/share"
    } else {
        "/usr/share"
    }
}

// libc++ is llvm-runtimes' and llvm-runtimes is built for musl, so a gnu c++ compile
// reaches through libc++'s headers into musl's internal ones -- bits/alltypes.h, which
// glibc has never heard of. libstdc++ is what the gnu substrate ships
//
// -stdlib alone does not get there: clang works out where libstdc++'s headers are from
// the gcc installation sitting next to its own binary and never consults --sysroot for
// them, so it looks in /usr/include/c++, which is musl's side
fn gcc_headers(sysroot: &Path) -> Option<String> {
    let at = sysroot.join("usr/x86_64-linux-gnu/usr/include/c++");
    let mut v: Vec<String> = fs::read_dir(&at)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    Some(format!(
        "/usr/x86_64-linux-gnu/usr/include/c++/{}",
        v.pop()?
    ))
}

fn meson_wrapper(t: &str) -> String {
    MESON
        .replace("@LIBDIR@", libdir(t))
        .replace("@INCLUDEDIR@", incdir(t))
        .replace("@DATADIR@", datadir(t))
}

fn libdir(t: &str) -> &'static str {
    if t.ends_with("gnu") {
        "/usr/lib64"
    } else {
        "/usr/lib"
    }
}

// what clang -print-target-triple answers. a recipe reads it for a name rather than for
// a decision -- LLVM_HOST_TRIPLE, clang/$CHOST.cfg, [target.$CHOST] -- and an empty one
// is a wrong answer that builds
fn triple(t: &str) -> &'static str {
    if t.ends_with("gnu") {
        "x86_64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-musl"
    }
}

fn jobs() -> usize {
    match std::env::var("KIRY_JOBS").ok().and_then(|v| v.parse().ok()) {
        Some(n) if n > 0 => n,
        _ => std::thread::available_parallelism().map_or(1, |n| n.get()),
    }
}

fn unpacked(src: &Path) -> PathBuf {
    let Ok(rd) = fs::read_dir(src) else {
        return src.to_path_buf();
    };

    // patches and the odd config file sit next to the tarball they belong to, so only
    // the directories decide this. two of them and there is no one tree to start in
    let mut only = None;
    for e in rd.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        if only.is_some() {
            return src.to_path_buf();
        }
        only = Some(e.path());
    }
    only.unwrap_or_else(|| src.to_path_buf())
}

fn tarball(name: &str) -> bool {
    name.contains(".tar")
        || name.ends_with(".tgz")
        || name.ends_with(".txz")
        || name.ends_with(".tbz2")
        || name.ends_with(".tzst")
}

fn pack(root: &Path, p: &Package, t: &str, work: &Path) -> Result<(PathBuf, PathBuf), String> {
    let cache = root.join("var/kiry/cache");
    mkdirs(&cache)?;

    let art = cache.join(format!(
        "{}-{}-{}.{t}.tar.zst",
        p.name, p.version.upstream, p.version.rev
    ));
    let mut part = art.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut c = Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(work.join("dest"))
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tar: {e}"))?;

    let Some(pipe) = c.stdout.take() else {
        return Err("tar gave us no pipe".into());
    };

    let z = Command::new("zstd")
        .arg("-19")
        .arg("-T0")
        .arg("-qf")
        .arg("-o")
        .arg(&part)
        .stdin(pipe)
        .status()
        .map_err(|e| format!("zstd: {e}"))?;

    let tar = c.wait().map_err(|e| format!("tar: {e}"))?;
    if !tar.success() {
        return Err(format!("tar: {tar}"));
    }
    if !z.success() {
        return Err(format!("zstd: {z}"));
    }
    Ok((art, part))
}

fn meta(p: &Package, t: &str, hash: &str, art: &Path) -> Result<(), String> {
    let mut d = art.as_os_str().to_owned();
    d.push(".meta");
    let d = PathBuf::from(d);
    let _ = fs::remove_dir_all(&d);
    mkdirs(&d)?;

    put(&d.join("name"), &format!("{}\n", p.name))?;
    put(&d.join("version"), &format!("{}\n", p.version))?;
    put(&d.join("targets"), &format!("{t}\n"))?;
    put(&d.join("hash"), &format!("{hash}\n"))?;
    put(&d.join("users"), &format!("{}\n", p.users.join("\n")))?;

    // one line per dep that applies to this target, the same narrowing targets gets --
    // an artifact is built for one target and carries what that build actually used
    let mut deps = String::new();
    for x in p.depends.iter().filter(|x| x.applies(t)) {
        deps.push_str(&format!("{x}\n"));
    }
    put(&d.join("depends"), &deps)?;
    Ok(())
}

// no target in here on purpose. every target of a package has to come out with the
// same hash, which is the whole check
fn recipe_hash(p: &Package, srcs: &[(String, PathBuf, String)]) -> Result<String, String> {
    let rd = fs::read_dir(&p.dir).map_err(|e| format!("{}: {e}", p.dir.display()))?;
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    names.sort();

    let mut blob = Vec::new();
    for n in &names {
        let body = fs::read(p.dir.join(n)).map_err(|e| format!("{n}: {e}"))?;
        blob.extend_from_slice(format!("{n} {}\n", body.len()).as_bytes());
        blob.extend_from_slice(&body);
    }
    // anything the build reads is either one of those files or a listed source
    for (_, _, sum) in srcs {
        blob.extend_from_slice(sum.as_bytes());
        blob.push(b'\n');
    }

    kiry_core::sha256(&blob[..]).map_err(|e| e.to_string())
}

fn run(c: &mut Command, what: &str) -> Result<(), String> {
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{what}: {s}")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

fn sha(p: &Path) -> Result<String, String> {
    let f = fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    kiry_core::sha256(f).map_err(|e| format!("{}: {e}", p.display()))
}

fn abs(p: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn mkdirs(p: &Path) -> Result<(), String> {
    fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn put(p: &Path, s: &str) -> Result<(), String> {
    fs::write(p, s).map_err(|e| format!("{}: {e}", p.display()))
}

fn took(start: Instant) -> String {
    let s = start.elapsed().as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn install_cmd(args: &[String]) {
    let (root, force, names) = opts(args);
    writes(&root);
    let archives: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    if archives.is_empty() {
        die("nothing to install".into());
    }
    let jobs = match install::plan(&root, &archives, force) {
        Ok(j) => j,
        Err(e) => die(e.to_string()),
    };
    let done = match install::apply(&root, &jobs) {
        Ok(b) => b,
        Err(e) => die(e.to_string()),
    };

    for j in &jobs {
        say!("{} {} {} ok", j.name, j.version.upstream, j.target);
    }
    for p in &done.edits {
        say!("kept /{p}, edited since it was installed");
    }
    enqueue(&root, &done.broke, &named(&jobs));
    hooks(&root, jobs.iter().map(|j| j.target.clone()).collect());
}

// a generated file that has to be rebuilt whenever a target's tree changes, and the
// first one is the gnu ld.so.cache: pressure-vessel reads it, and glibc's ldconfig has
// to be run somewhere /usr/lib is not visible, because its builtin directory list is
// /usr/lib and /usr/lib64 and no config file subtracts from it. that is a chroot and a
// bind mount, so it cannot live in kiry-core and does not belong in the binary either
//
// config rather than package data. a directory of scripts is fixable from a rescue
// shell without rebuilding whatever installed it, which is the hand-edit invariant
// applied to the one thing that runs while a system is half-assembled
//
// argv is the targets the transaction touched, so a hook for one tier can return
// without doing anything. the transaction already happened, so a hook that fails is
// reported and not fatal -- there is nothing left to roll back, and dying here would
// only hide what landed
fn hooks(root: &Path, targets: BTreeSet<String>) {
    let dir = root.join("etc/kiry/hooks.d");
    let Ok(rd) = fs::read_dir(&dir) else {
        return;
    };

    let mut found: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    found.sort();

    for h in found {
        // executable or it is not a hook. that turns one off without deleting it, and
        // it passes over editor droppings for free
        match h.metadata() {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {}
            _ => continue,
        }
        let mut c = Command::new(&h);
        c.args(&targets).env("KIRY_ROOT", root);
        match c.status() {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("kiry: {}: {s}", h.display()),
            Err(e) => eprintln!("kiry: {}: {e}", h.display()),
        }
    }
}

fn named(jobs: &[install::Job]) -> HashSet<(String, String)> {
    jobs.iter()
        .map(|j| (j.target.clone(), j.name.clone()))
        .collect()
}

fn remove_cmd(args: &[String]) {
    let (root, force, names) = opts(args);
    writes(&root);
    if names.is_empty() {
        die("nothing to remove".into());
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut plan: Vec<(&String, Vec<String>)> = Vec::new();
    for t in &targets {
        let have = match db::installed(&root, t) {
            Ok(h) => h,
            Err(e) => die(e.to_string()),
        };
        let mine: Vec<String> = names.iter().filter(|n| have.contains(n)).cloned().collect();
        if !mine.is_empty() {
            plan.push((t, mine));
        }
    }
    for name in &names {
        if !plan.iter().any(|(_, mine)| mine.contains(name)) {
            die(format!("{name} is not installed"));
        }
    }

    for (t, mine) in &plan {
        let done = match install::remove(&root, t, mine, force) {
            Ok(d) => d,
            Err(e) => die(e.to_string()),
        };
        for (name, r) in done {
            let mut notes = Vec::new();
            if r.kept > 0 {
                notes.push(format!("{} modified, left alone", r.kept));
            }
            if r.missing > 0 {
                notes.push(format!("{} already gone", r.missing));
            }
            let note = if notes.is_empty() {
                String::new()
            } else {
                format!("  {}", notes.join(", "))
            };
            let s = if r.gone == 1 { "" } else { "s" };
            say!("{name} {t} removed {} file{s}{note}", r.gone);
        }
    }
    hooks(&root, plan.iter().map(|(t, _)| (*t).clone()).collect());
}

fn list_cmd(args: &[String]) {
    let (root, _, _) = opts(args);
    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    for t in &targets {
        let names = match db::installed(&root, t) {
            Ok(n) => n,
            Err(e) => die(e.to_string()),
        };
        for n in names {
            match db::read(&root, t, &n) {
                Ok(r) => say!("{} {} {}", r.name, r.version.upstream, t),
                Err(e) => die(e.to_string()),
            }
        }
    }
}

fn repos(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(root.join("etc/kiry/repos")) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(PathBuf::from)
        .collect()
}

fn recipe(root: &Path, name: &str) -> Option<PathBuf> {
    repos(root)
        .into_iter()
        .map(|d| d.join(name))
        .find(|d| d.join("build").is_file())
}

fn levels(want: &[(String, String)], recipes: &HashMap<String, Package>) -> Vec<Vec<usize>> {
    let mut left: Vec<usize> = (0..want.len()).collect();
    let mut out = Vec::new();
    while !left.is_empty() {
        let now: Vec<usize> = left
            .iter()
            .copied()
            .filter(|i| {
                let deps = recipes.get(&want[*i].0).map(|p| &p.depends);
                !left.iter().any(|j| {
                    j != i
                        && deps.is_some_and(|d| {
                            d.iter().any(|x| {
                                !x.make && x.applies(&want[*i].1) && x.name == want[*j].0
                            })
                        })
                })
            })
            .collect();
        if now.is_empty() {
            // a cycle has to be declared, not guessed at. bootstrap is the file that
            // says which member breaks it and none of these carry one
            let names: Vec<&str> = left.iter().map(|i| want[*i].0.as_str()).collect();
            die(format!(
                "these need each other and none says how to start: {}",
                names.join(" ")
            ));
        }
        left.retain(|i| !now.contains(i));
        out.push(now);
    }
    out
}

fn rebuild_cmd(args: &[String]) {
    let mut dry = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "-n" {
            dry = true;
        } else {
            rest.push(a.clone());
        }
    }
    let (root, _, extra) = opts(&rest);
    writes(&root);
    if let Some(a) = extra.first() {
        die(format!("rebuild takes no arguments, got {a}"));
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut want: Vec<(String, String)> = Vec::new();
    // the queue names consumers of a library whose abi moved, which resolves fine and
    // so is invisible to a check. the checks name what is already broken
    let queued = db::read_queue(&root).unwrap_or_default();
    let (healed, live): (Vec<_>, Vec<_>) = queued.into_iter().partition(|q| back(&root, q));
    for q in &healed {
        say!("{} {} dropped, {} is whole again", q.name, q.target, q.soname);
    }
    if !healed.is_empty() {
        if let Err(e) = db::write_queue(&root, &live) {
            die(e.to_string());
        }
    }
    for q in live {
        if !want.contains(&(q.name.clone(), q.target.clone())) {
            want.push((q.name, q.target));
        }
    }
    for t in &targets {
        for f in check(&root, t) {
            if f.what.rebuilds() && !want.contains(&(f.pkg.clone(), t.clone())) {
                want.push((f.pkg.clone(), t.clone()));
            }
        }
    }
    if want.is_empty() {
        return;
    }

    let mut recipes: HashMap<String, Package> = HashMap::new();
    for (name, _) in &want {
        if recipes.contains_key(name) {
            continue;
        }
        let Some(dir) = recipe(&root, name) else {
            die(format!(
                "{name} needs rebuilding and no repo has a recipe for it"
            ));
        };
        match pkg::load(&dir) {
            Ok(p) => recipes.insert(name.clone(), p),
            Err(e) => die(e.to_string()),
        };
    }

    let plan = levels(&want, &recipes);
    if dry {
        for i in plan.into_iter().flatten() {
            say!("{} {} would rebuild", want[i].0, want[i].1);
        }
        return;
    }

    for level in plan {
        // every member of a level compiles before any of it is installed, and the level
        // below it is already in place, so each one links against what it will run with
        let mut made = Vec::new();
        for i in &level {
            let (name, target) = &want[*i];
            let p = &recipes[name];
            match build(&root, p, std::slice::from_ref(target), false) {
                Ok(arts) => made.extend(arts),
                Err(e) => die(e),
            }
        }
        let jobs = match install::plan(&root, &made, false) {
            Ok(j) => j,
            Err(e) => die(e.to_string()),
        };
        let done = match install::apply(&root, &jobs) {
            Ok(b) => b,
            Err(e) => die(e.to_string()),
        };
        let broke = done.broke;
        for p in &done.edits {
            say!("kept /{p}, edited since it was installed");
        }
        for j in &jobs {
            say!("{} {} {} rebuilt", j.name, j.version.upstream, j.target);
        }
        enqueue(&root, &broke, &named(&jobs));
        hooks(&root, jobs.iter().map(|j| j.target.clone()).collect());
    }

    // a rebuild that ran is off the queue whether or not it fixed anything, or the next
    // drain starts from the same list. what these rebuilds queued in turn stays
    let left_over: Vec<db::Queued> = db::read_queue(&root)
        .unwrap_or_default()
        .into_iter()
        .filter(|q| !want.contains(&(q.name.clone(), q.target.clone())))
        .collect();
    if let Err(e) = db::write_queue(&root, &left_over) {
        die(e.to_string());
    }

    let mut left = 0;
    for t in &targets {
        for f in check(&root, t) {
            if f.what.rebuilds() {
                say!("{} {t} {}", f.path, f.what);
                left += 1;
            }
        }
    }
    if left > 0 {
        std::process::exit(1);
    }
}

// an abi break is only a break for the consumers that used what moved. everything that
// links libfoo is the set doctor would give; the ones whose undefined symbols name a
// symbol that actually changed is the set worth rebuilding
fn affected(
    root: &Path,
    broke: &[install::Broke],
    just: &HashSet<(String, String)>,
) -> Vec<db::Queued> {
    let mut out = Vec::new();
    for b in broke {
        let versions = b.target.ends_with("gnu");
        let moved: HashSet<&str> = b.changed.iter().map(|c| c.symbol()).collect();
        // only a name that left can be shown to have come back. a grown object is
        // present either way, so nothing is recorded for it and the entry never clears
        let gone: HashSet<&str> = b
            .changed
            .iter()
            .filter_map(|c| match c {
                elf::Change::Gone(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        let Ok(names) = db::installed(root, &b.target) else {
            continue;
        };
        for name in names {
            if just.contains(&(b.target.clone(), name.clone())) {
                continue;
            }
            let Ok(rec) = db::read(root, &b.target, &name) else {
                continue;
            };
            let Ok(seen) = install::scan(root, &rec.manifest) else {
                continue;
            };
            let uses = seen.iter().any(|(_, s)| {
                let install::Seen::Elf(o) = s else {
                    return false;
                };
                o.needed.contains(&b.soname)
                    && o.undefined
                        .iter()
                        .any(|u| moved.contains(symbol(u, versions).as_str()))
            });
            if uses {
                out.push(db::Queued {
                    target: b.target.clone(),
                    soname: b.soname.clone(),
                    name,
                    changed: gone.iter().map(|s| (*s).to_string()).collect(),
                });
            }
        }
    }
    out
}

// an entry records the names that left. if the library exports all of them again the
// consumer was never going to break, whatever put them back
fn back(root: &Path, q: &db::Queued) -> bool {
    if q.changed.is_empty() {
        return false;
    }
    let Ok(names) = db::installed(root, &q.target) else {
        return false;
    };
    let versions = q.target.ends_with("gnu");
    for name in names {
        let Ok(ps) = db::read_provides(root, &q.target, &name) else {
            continue;
        };
        for pv in ps.iter().filter(|p| p.soname == q.soname) {
            let Ok(o) = elf::read(&root.join(&pv.path)) else {
                continue;
            };
            let have: HashSet<String> = o.exports.iter().map(|e| symbol(e, versions)).collect();
            return q.changed.iter().all(|c| have.contains(c));
        }
    }
    false
}

// the same key compare built its changes with, or the two sides never meet
fn symbol(s: &elf::Sym, versions: bool) -> String {
    match (versions, &s.version) {
        (true, Some(v)) => format!("{}@{v}", s.name),
        _ => s.name.clone(),
    }
}

// what the batch left behind for rebuild to drain. an empty set is the early cutoff:
// a library whose exports only grew breaks nobody and queues nothing
fn enqueue(root: &Path, broke: &[install::Broke], just: &HashSet<(String, String)>) {
    let want = affected(root, broke, just);
    if want.is_empty() {
        return;
    }
    let mut all = db::read_queue(root).unwrap_or_default();
    all.extend(want);
    all.sort();
    all.dedup();
    match db::write_queue(root, &all) {
        Ok(()) => say!("queued {}", all.len()),
        Err(e) => die(e.to_string()),
    }
}

fn convert_cmd(args: &[String]) {
    let mut offline = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "-n" {
            offline = true;
        } else {
            rest.push(a.clone());
        }
    }
    let (root, _, rest) = opts(&rest);
    let (out, names) = match rest.split_last() {
        Some((out, names)) if !names.is_empty() => (PathBuf::from(out), names),
        _ => die("convert wants one or more APKBUILDs and a directory to write into".into()),
    };

    let repos = repos(&root);
    let alias = convert::aliases(&repos);
    // the run is the survey, so an apkbuild that will not read is a line in it
    let mut failed = 0;
    for n in names {
        match convert::recipe(Path::new(n), &out, !offline, &alias, &repos) {
            Ok(r) => {
                say!("{} converted", r.name);
                for note in &r.notes {
                    say!("  {note}");
                }
            }
            Err(e) => {
                failed += 1;
                say!("{n} failed {e}");
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

// a path answers with the package that claims it, on whichever target claims it. more
// than one is a bug the path check exists to prevent, so all of them are printed
fn owns_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    if rest.is_empty() {
        die("owns wants a path".into());
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut owner: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for t in &targets {
        for name in db::installed(&root, t).unwrap_or_default() {
            let Ok(rec) = db::read(&root, t, &name) else {
                continue;
            };
            for e in rec.manifest {
                owner
                    .entry(e.path)
                    .or_default()
                    .push((name.clone(), t.clone()));
            }
        }
    }

    let mut missed = false;
    for want in &rest {
        // the manifest holds paths relative to the root, and a caller types the
        // absolute one they were just shown
        let key = want.trim_start_matches('/');
        match owner.get(key) {
            Some(who) => {
                for (name, t) in who {
                    say!("/{key} {name} {t}");
                }
            }
            None => {
                say!("/{key} owned by nobody");
                missed = true;
            }
        }
    }
    if missed {
        std::process::exit(1);
    }
}

fn doctor_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let mut files = false;
    let mut orphans = false;
    for a in &rest {
        match a.as_str() {
            "--files" => files = true,
            "--orphans" => orphans = true,
            _ => die(format!("doctor takes no arguments, got {a}")),
        }
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut found = 0;
    for t in &targets {
        for f in check(&root, t) {
            say!("{} {t} {}", f.path, f.what);
            found += 1;
        }
    }
    for f in drift(&root, &targets) {
        say!("{} {} {}", f.path, f.pkg, f.what);
        found += 1;
    }
    if files {
        for f in changed(&root, &targets) {
            say!("{} {} {}", f.path, f.pkg, f.what);
            found += 1;
        }
    }
    if orphans {
        let (loose, ignored) = unowned(&root, &targets);
        for f in &loose {
            say!("{} {}", f.path, f.what);
        }
        found += loose.len();
        if ignored > 0 {
            say!("{ignored} more matched /etc/kiry/unowned");
        }
    }
    if found > 0 {
        std::process::exit(1);
    }
}

// off by default. every manifest names a hash and reading five gigabytes back is a
// different kind of check from the ones that answer out of the index
fn changed(root: &Path, targets: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    for t in targets {
        for name in db::installed(root, t).unwrap_or_default() {
            let Ok(rec) = db::read(root, t, &name) else {
                continue;
            };
            for p in install::modified(root, &rec.manifest).unwrap_or_default() {
                out.push(Finding {
                    pkg: name.clone(),
                    path: p,
                    what: What::Modified,
                });
            }
        }
    }
    out
}

// what no manifest claims, under the directories packages install into. /etc/kiry/unowned
// takes one path prefix per line, and what it hides is counted rather than dropped
fn unowned(root: &Path, targets: &[String]) -> (Vec<Finding>, usize) {
    let mut owned: HashSet<String> = HashSet::new();
    for t in targets {
        for name in db::installed(root, t).unwrap_or_default() {
            if let Ok(rec) = db::read(root, t, &name) {
                owned.extend(rec.manifest.into_iter().map(|e| e.path));
            }
        }
    }

    let skip: Vec<String> = fs::read_to_string(root.join("etc/kiry/unowned"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_start_matches('/').to_string())
        .collect();

    let mut out = Vec::new();
    let mut ignored = 0;
    let mut stack: Vec<String> = vec!["usr".into(), "etc".into()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let Some(leaf) = e.file_name().to_str().map(String::from) else {
                continue;
            };
            let path = format!("{rel}/{leaf}");
            // a symlink is an entry, never a way further down: following one leaves the
            // tree being walked
            let dir = e.file_type().is_ok_and(|f| f.is_dir());
            if dir {
                stack.push(path.clone());
            }
            if owned.contains(&path) {
                continue;
            }
            if skip.iter().any(|s| path == *s || path.starts_with(&format!("{s}/"))) {
                ignored += 1;
                continue;
            }
            // a directory nobody claims is where an unowned file lives, and saying both
            // is the same finding twice
            if !dir {
                out.push(Finding {
                    pkg: "-".into(),
                    path,
                    what: What::Unowned,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    (out, ignored)
}

// every target of one recipe is built from the same files, so the hashes have to match
// they stop matching when a recipe is bumped and only one target is rebuilt
fn drift(root: &Path, targets: &[String]) -> Vec<Finding> {
    let mut seen: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for t in targets {
        for name in db::installed(root, t).unwrap_or_default() {
            let Ok(rec) = db::read(root, t, name.as_str()) else {
                continue;
            };
            if !rec.hash.is_empty() {
                seen.entry(name).or_default().push((t.clone(), rec.hash));
            }
        }
    }

    let mut out = Vec::new();
    for (name, mut ts) in seen {
        ts.sort();
        let Some((_, first)) = ts.first() else {
            continue;
        };
        if let Some((t, _)) = ts.iter().find(|(_, h)| h != first) {
            out.push(Finding {
                pkg: t.clone(),
                path: name,
                what: What::TargetDrift(ts[0].0.clone()),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// kiry never writes /etc/passwd. a recipe says which accounts it expects and doctor
// reports the ones that are not there
fn missing_users(root: &Path, target: &str, names: &[String]) -> Vec<Finding> {
    let passwd = fs::read_to_string(root.join("etc/passwd")).unwrap_or_default();
    let have: HashSet<&str> = passwd.lines().filter_map(|l| l.split(':').next()).collect();

    let mut out = Vec::new();
    for name in names {
        let Ok(rec) = db::read(root, target, name) else {
            continue;
        };
        for u in rec.users.iter().filter(|u| !have.contains(u.as_str())) {
            out.push(Finding {
                pkg: name.clone(),
                path: name.clone(),
                what: What::MissingUser(u.clone()),
            });
        }
    }
    out
}

// the linker gives every object these for itself, and the loader reaches DT_INIT and
// DT_FINI by address rather than by name. counting them makes every pair of libraries in
// the tree a duplicate, which is how a check that finds a rare real bug becomes noise
const HOUSEKEEPING: &[&str] = &["_init", "_fini", "_edata", "_end", "__bss_start", "_etext"];

// a finding is a value rather than a line on stdout, so a caller can group or count
// them without parsing what doctor printed
struct Finding {
    pkg: String,
    path: String,
    what: What,
}

enum What {
    UnknownTarget,
    Unreadable,
    StaleProvides,
    CrossTier(String),
    Unresolved(String),
    NoInterpreter(String),
    MissingSymbol(String),
    Duplicate(usize, String),
    MissingUser(String),
    TargetDrift(String),
    Modified,
    Unowned,
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            What::UnknownTarget => write!(f, "- unknown-target"),
            What::Unreadable => write!(f, "unreadable"),
            What::StaleProvides => write!(f, "stale-provides"),
            What::CrossTier(p) => write!(f, "cross-tier {p}"),
            What::Unresolved(n) => write!(f, "unresolved {n}"),
            What::NoInterpreter(w) => write!(f, "no-interpreter {w}"),
            What::MissingSymbol(s) => write!(f, "missing-symbol {s}"),
            What::Duplicate(n, p) => write!(f, "duplicate-symbols {n} {p}"),
            What::MissingUser(u) => write!(f, "missing-user {u}"),
            What::TargetDrift(o) => write!(f, "target-drift {o}"),
            What::Modified => write!(f, "modified"),
            What::Unowned => write!(f, "unowned"),
        }
    }
}

impl What {
    fn rebuilds(&self) -> bool {
        matches!(self, What::Unresolved(_) | What::MissingSymbol(_))
    }
}

fn check(root: &Path, target: &str) -> Vec<Finding> {
    let Some(dirs) = defaults(target) else {
        return vec![Finding {
            pkg: "-".into(),
            path: target.to_string(),
            what: What::UnknownTarget,
        }];
    };

    let names = match db::installed(root, target) {
        Ok(n) => n,
        Err(e) => die(e.to_string()),
    };

    let mut elves = Vec::new();
    // index-aligned with elves, the way here already is
    let mut owners: Vec<String> = Vec::new();
    let mut here: HashMap<String, usize> = HashMap::new();
    let mut links: HashMap<String, String> = HashMap::new();
    let mut shebangs: Vec<(String, String, Vec<String>)> = Vec::new();
    // every regular file, not only the ones that parse as elf: an interpreter is a file
    // a hardlink is one too -- the same inode under a second name, which is how perl
    // ships /usr/bin/perl beside perl5.44.0
    let mut present: HashSet<String> = HashSet::new();
    let mut out: Vec<Finding> = Vec::new();

    for name in &names {
        let rec = match db::read(root, target, name) {
            Ok(r) => r,
            Err(e) => die(e.to_string()),
        };
        // the loader opens a path, so a symlink on the way to a library is part of
        // resolution. musl reaches its libc through one: usr/lib/libc.musl-x86_64.so.1
        // points at the loader itself
        index(&rec.manifest, &mut present, &mut links);

        let seen = match install::scan(root, &rec.manifest) {
            Ok(s) => s,
            Err(e) => die(e.to_string()),
        };

        let mut mine = Vec::new();
        for (path, what) in seen {
            let o = match what {
                install::Seen::Elf(o) => o,
                install::Seen::Script(words) => {
                    shebangs.push((name.clone(), path, words));
                    continue;
                }
                install::Seen::Other => continue,
                install::Seen::Bad => {
                    out.push(Finding {
                        pkg: name.clone(),
                        path,
                        what: What::Unreadable,
                    });
                    continue;
                }
            };
            here.insert(fold(&path), elves.len());
            if let Some(soname) = &o.soname {
                mine.push(db::Provide {
                    soname: soname.clone(),
                    versioned: o.versioned,
                    path: path.clone(),
                });
            }
            owners.push(name.clone());
            elves.push((path, o));
        }

        // the only thing that ever reads the recorded file, and a moved soname is
        // what it notices
        match db::read_provides(root, target, name) {
            Ok(mut was) => {
                let mut is = mine;
                was.sort_by(|a, b| (&a.path, &a.soname).cmp(&(&b.path, &b.soname)));
                is.sort_by(|a, b| (&a.path, &a.soname).cmp(&(&b.path, &b.soname)));
                if was != is {
                    out.push(Finding {
                        pkg: name.clone(),
                        path: name.clone(),
                        what: What::StaleProvides,
                    });
                }
            }
            Err(e) => die(e.to_string()),
        }
    }

    let sets = exported(&elves);
    let mut linked: HashSet<usize> = HashSet::new();
    let mut needs: Vec<Vec<usize>> = vec![Vec::new(); elves.len()];
    for (i, (path, o)) in elves.iter().enumerate() {
        let where_ = search(o, path, dirs);
        for want in &o.needed {
            match provider(&here, &links, want, &where_) {
                Some(j) => {
                    linked.insert(j);
                    needs[i].push(j);
                    // musl ignores symbol versions, so a gnu binary that reaches into
                    // the musl tree binds to whatever has the right name and nothing
                    // errors. loader paths keep them apart until an rpath crosses over
                    if target.ends_with("gnu") && elves[j].0.starts_with("usr/lib/") {
                        out.push(Finding {
                            pkg: owners[i].clone(),
                            path: path.clone(),
                            what: What::CrossTier(elves[j].0.clone()),
                        });
                    }
                }
                None => out.push(Finding {
                    pkg: owners[i].clone(),
                    path: path.clone(),
                    what: What::Unresolved(want.clone()),
                }),
            }
        }
    }

    // the kernel will not start a script whose interpreter is not there, which is the
    // same failure DT_NEEDED describes and nothing was checking it
    let (anywhere, anylinks) = everywhere(root);
    for (pkg, path, words) in &shebangs {
        let mut want = words[0].trim_start_matches('/').to_string();
        // env looks the real one up on PATH, so that is the name that has to exist
        if want.ends_with("/env") || want == "env" {
            // env takes its own options and VAR=value pairs first. -S is the common one
            match words
                .iter()
                .skip(1)
                .find(|w| !w.starts_with('-') && !w.contains('='))
            {
                Some(w) => want.clone_from(w),
                None => continue,
            }
        }
        let there = if want.contains('/') {
            exists(&anywhere, &anylinks, &want)
        } else {
            ["usr/bin", "usr/sbin"]
                .iter()
                .any(|d| exists(&anywhere, &anylinks, &format!("{d}/{want}")))
        };
        if !there {
            out.push(Finding {
                pkg: pkg.clone(),
                path: path.clone(),
                what: What::NoInterpreter(words.join(" ")),
            });
        }
    }

    // a perl xs module is dlopened, so nothing links it and it is not asked. the library
    // it pulls in is linked, but only by something equally unknowable, and the question
    // is no more answerable there: texinfo's libtexinfo leaves PL_current_context to
    // whatever interpreter loads it. what decides is reachability from something the
    // loader itself resolves, not whether anything at all names the file
    let mut asked: HashSet<usize> = HashSet::new();
    let mut walk: Vec<usize> = elves
        .iter()
        .enumerate()
        .filter(|(_, (_, o))| o.interp)
        .map(|(i, _)| i)
        .collect();
    while let Some(i) = walk.pop() {
        if !asked.insert(i) {
            continue;
        }
        walk.extend(needs[i].iter().copied());
    }

    for (i, (path, o)) in elves.iter().enumerate() {
        let _ = o;
        if !asked.contains(&i) {
            continue;
        }
        for want in missing(&elves, &sets, &here, &links, dirs, i) {
            out.push(Finding {
                pkg: owners[i].clone(),
                path: path.clone(),
                what: What::MissingSymbol(want),
            });
        }
    }

    // two libraries exporting one name means load order decides which implementation a
    // caller gets, silently. the index holds every export already, so this is a group-by
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, o)) in elves.iter().enumerate() {
        if o.soname.is_none() || !asked.contains(&i) {
            continue;
        }
        for sym in &o.exports {
            if sym.weak
                || sym.version.as_deref() == Some(sym.name.as_str())
                || HOUSEKEEPING.contains(&sym.name.as_str())
            {
                continue;
            }
            let who = by_name.entry(&sym.name).or_default();
            if who.last() != Some(&i) {
                who.push(i);
            }
        }
    }
    let mut pairs: HashMap<(usize, usize), usize> = HashMap::new();
    for (_, who) in by_name {
        for a in 0..who.len() {
            for b in a + 1..who.len() {
                *pairs
                    .entry((who[a].min(who[b]), who[a].max(who[b])))
                    .or_default() += 1;
            }
        }
    }
    let mut dupes: Vec<((usize, usize), usize)> = pairs.into_iter().collect();
    dupes.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    for ((a, b), n) in dupes {
        out.push(Finding {
            pkg: owners[a].clone(),
            path: elves[a].0.clone(),
            what: What::Duplicate(n, elves[b].0.clone()),
        });
    }
    out.extend(missing_users(root, target, &names));
    out
}

// DT_NEEDED names a file, not a soname. the loader opens the first directory on the
// search path holding a file by that name and never looks at what the library calls
// itself, so a library with no DT_SONAME at all still loads. matching sonames instead
// declared alpine's libscudo.so missing while it sat in usr/lib
fn provider(
    here: &HashMap<String, usize>,
    links: &HashMap<String, String>,
    want: &str,
    where_: &[String],
) -> Option<usize> {
    where_
        .iter()
        .find_map(|d| at_path(here, links, &format!("{d}/{want}")))
}

// the loader opens a path, so a symlink on the way to a library is part of resolution
// musl reaches its libc through one: usr/lib/libc.musl-x86_64.so.1 points at the loader
// itself. a hardlink is a file too, which is how perl ships /usr/bin/perl beside
// perl5.44.0
fn index(
    manifest: &[db::Entry],
    present: &mut HashSet<String>,
    links: &mut HashMap<String, String>,
) {
    for e in manifest {
        if matches!(e.kind, db::Kind::File(_) | db::Kind::Hard(_)) {
            present.insert(fold(&e.path));
        }
        if let db::Kind::Link(t) = &e.kind {
            let to = if let Some(abs) = t.strip_prefix('/') {
                abs.to_string()
            } else {
                format!("{}/{t}", dirname(&e.path))
            };
            links.insert(fold(&e.path), fold(&to));
        }
    }
}

// a shebang names a path and a path has no tier. /bin/sh runs whatever libc built it,
// so glibc's own scripts reach the busybox sh the musl side owns. linkage stays inside
// one target -- that is what cross-tier exists to catch -- but this does not
fn everywhere(root: &Path) -> (HashSet<String>, HashMap<String, String>) {
    let mut present = HashSet::new();
    let mut links = HashMap::new();
    let Ok(targets) = db::targets(root) else {
        return (present, links);
    };
    for t in &targets {
        let Ok(names) = db::installed(root, t) else {
            continue;
        };
        for n in &names {
            if let Ok(rec) = db::read(root, t, n) {
                index(&rec.manifest, &mut present, &mut links);
            }
        }
    }
    (present, links)
}

fn exists(present: &HashSet<String>, links: &HashMap<String, String>, path: &str) -> bool {
    let mut at = fold(path);
    for _ in 0..8 {
        if present.contains(&at) {
            return true;
        }
        match links.get(&at) {
            Some(to) => at = to.clone(),
            None => return false,
        }
    }
    false
}

fn at_path(
    here: &HashMap<String, usize>,
    links: &HashMap<String, String>,
    path: &str,
) -> Option<usize> {
    let mut at = fold(path);
    // a chain of eight is past anything real and short of looping forever
    for _ in 0..8 {
        if let Some(i) = here.get(&at) {
            return Some(*i);
        }
        at = links.get(&at)?.clone();
    }
    None
}

type Exports<'a> = (HashSet<&'a str>, HashSet<(&'a str, Option<&'a str>)>);

fn exported(elves: &[(String, elf::Elf)]) -> Vec<Exports<'_>> {
    elves
        .iter()
        .map(|(_, o)| {
            let mut names = HashSet::new();
            let mut versioned = HashSet::new();
            for s in &o.exports {
                names.insert(s.name.as_str());
                versioned.insert((s.name.as_str(), s.version.as_deref()));
            }
            (names, versioned)
        })
        .collect()
}

fn missing(
    elves: &[(String, elf::Elf)],
    sets: &[Exports<'_>],
    here: &HashMap<String, usize>,
    links: &HashMap<String, String>,
    dirs: &[&str],
    root: usize,
) -> Vec<String> {
    let mut seen = HashSet::from([root]);
    let mut queue = vec![root];
    let mut closure = vec![root];

    while let Some(i) = queue.pop() {
        let (path, o) = &elves[i];
        let where_ = search(o, path, dirs);
        for want in &o.needed {
            if let Some(j) = provider(here, links, want, &where_) {
                if seen.insert(j) {
                    queue.push(j);
                    closure.push(j);
                }
            }
        }
    }

    elves[root]
        .1
        .undefined
        .iter()
        // a weak undefined is allowed to stay undefined, which is the whole point of
        // it. __gmon_start__ sits in nearly every binary on the system
        .filter(|s| !s.weak)
        .filter(|s| {
            !closure.iter().any(|&i| match s.version.as_deref() {
                None => sets[i].0.contains(s.name.as_str()),
                // an unversioned definition still satisfies a versioned request, which
                // is the case where the loader binds it and only warns
                Some(v) => {
                    sets[i].1.contains(&(s.name.as_str(), Some(v)))
                        || sets[i].1.contains(&(s.name.as_str(), None))
                }
            })
        })
        .map(|s| match &s.version {
            Some(v) => format!("{}@{v}", s.name),
            None => s.name.clone(),
        })
        .collect()
}

// no ld.so.conf and no cache exist anywhere in this system: musl uses the search path
// compiled into it, and the gnu tree is built with libdir=/usr/lib64
fn defaults(target: &str) -> Option<&'static [&'static str]> {
    match target.rsplit('-').next() {
        Some("musl") => Some(&["usr/lib", "usr/local/lib"]),
        Some("gnu") => Some(&["usr/lib64"]),
        _ => None,
    }
}

// the loader's remaining precedence reorders the search without changing what it finds
fn search(o: &elf::Elf, path: &str, dirs: &[&str]) -> Vec<String> {
    let own = dirname(path);
    let listed = o.runpath.as_deref().or(o.rpath.as_deref()).unwrap_or("");
    let mut out: Vec<String> = listed
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| fold(&d.replace("${ORIGIN}", own).replace("$ORIGIN", own)))
        .collect();
    out.extend(dirs.iter().map(|d| (*d).to_string()));
    out
}

fn dirname(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[..i],
        None => "",
    }
}

// /lib, /usr/lib and usr/bin/../lib all name one directory here
fn fold(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    if matches!(parts.first(), Some(&"lib" | &"lib64" | &"bin" | &"sbin")) {
        parts.insert(0, "usr");
    }
    parts.join("/")
}

fn show(dir: &str) {
    let dir = PathBuf::from(dir);
    let p = match pkg::load(&dir) {
        Ok(p) => p,
        Err(e) => die(e.to_string()),
    };

    say!("{} {}", p.name, p.version);
    say!("targets {}", p.targets.join(" "));

    for d in &p.depends {
        say!("dep {d}");
    }

    for (i, src) in p.sources.iter().enumerate() {
        let sum = p
            .checksums
            .get(i)
            .and_then(|s| s.get(..8))
            .unwrap_or("--------");
        say!("src {sum} {src}");
    }
}
