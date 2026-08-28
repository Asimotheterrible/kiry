mod sandbox;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use kiry_core::pkg::Package;
use kiry_core::{db, elf, install, pkg};

// a closed pipe is the readers business, not a failure of ours. rust ignores SIGPIPE
// and turns the write error into a panic, which is how `kiry l | head` printed a
// backtrace where every other unix tool goes quiet
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
        Some("b") => build_cmd(&args[1..]),
        Some("i") => install_cmd(&args[1..]),
        Some("r") => remove_cmd(&args[1..]),
        Some("l") => list_cmd(&args[1..]),
        Some("doctor") => doctor_cmd(&args[1..]),
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
    say!("       kiry doctor [--root DIR]");
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

// every target compiles and packs before any of them is renamed into place, so a
// package cannot leave half its targets in the cache
fn build(root: &Path, p: &Package, targets: &[String], verbose: bool) -> Result<(), String> {
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
    Ok(())
}

fn sources(root: &Path, p: &Package) -> Result<Vec<(PathBuf, String)>, String> {
    let cache = root.join("var/kiry/cache/sources");
    mkdirs(&cache)?;

    let mut out = Vec::new();
    for (i, s) in p.sources.iter().enumerate() {
        let path = if s.contains("://") {
            let name = s.rsplit('/').next().unwrap_or("");
            if name.is_empty() {
                return Err(format!("{s}: no file name at the end of that"));
            }
            let dst = cache.join(name);
            if !dst.exists() {
                grab(s, &dst)?;
            }
            dst
        } else {
            p.dir.join(s)
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
        out.push((path, sum));
    }
    Ok(out)
}

// %u is the url, %o the output. split on whitespace and exec directly, no shell,
// so a url never gets a second round of quoting
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
    srcs: &[(PathBuf, String)],
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

    for (path, _) in srcs {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if tarball(name) {
            run(
                Command::new("tar").arg("-xf").arg(path).arg("-C").arg(&src),
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
    let members = sandbox::closure(root, t, &p.depends)?;
    sandbox::assemble(root, t, &members, &sysroot)?;

    fs::copy(&script, sysroot.join("build")).map_err(|e| format!("build script: {e}"))?;

    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut c = Command::new(me);
    c.arg("sandbox")
        .env("KIRY_SANDBOX", abs(&work)?)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("DESTDIR", "/dest")
        .env("KIRY_SRCDIR", "/src")
        .env("KIRY_TARGET", t)
        .env("KIRY_NAME", &p.name)
        .env("KIRY_VERSION", &p.version.upstream)
        .env("KIRY_REV", p.version.rev.to_string());

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
        Ok(s) if s.success() => Ok(work),
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
fn unpacked(src: &Path) -> PathBuf {
    let Ok(rd) = fs::read_dir(src) else {
        return src.to_path_buf();
    };

    let mut only = None;
    for e in rd.flatten() {
        if only.is_some() || !e.path().is_dir() {
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

    let deps = p.dir.join("depends");
    if deps.is_file() {
        fs::copy(&deps, d.join("depends")).map_err(|e| format!("{}: {e}", deps.display()))?;
    }
    Ok(())
}

// no target in here on purpose. every target of a package has to come out with the
// same hash, which is the whole check
fn recipe_hash(p: &Package, srcs: &[(PathBuf, String)]) -> Result<String, String> {
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
    for (_, sum) in srcs {
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
    let archives: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    if archives.is_empty() {
        die("nothing to install".into());
    }
    let jobs = match install::plan(&root, &archives, force) {
        Ok(j) => j,
        Err(e) => die(e.to_string()),
    };
    if let Err(e) = install::apply(&root, &jobs) {
        die(e.to_string());
    }

    for j in &jobs {
        say!("{} {} {} ok", j.name, j.version.upstream, j.target);
    }
}

fn remove_cmd(args: &[String]) {
    let (root, force, names) = opts(args);
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

// the stale check goes when provides stops being a stored file, the linkage check
// when nothing outside kiry can write to the root, so never
fn doctor_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    if let Some(a) = rest.first() {
        die(format!("doctor takes no arguments, got {a}"));
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
    if found > 0 {
        std::process::exit(1);
    }
}

// the linker gives every object these for itself, and the loader reaches DT_INIT and
// DT_FINI by address rather than by name. counting them makes every pair of libraries in
// the tree a duplicate, which is how a check that finds a rare real bug becomes noise
const HOUSEKEEPING: &[&str] = &["_init", "_fini", "_edata", "_end", "__bss_start", "_etext"];

// a finding is a value rather than a line on stdout, so a caller can group or count
// them without parsing what doctor printed
struct Finding {
    path: String,
    what: String,
}

fn check(root: &Path, target: &str) -> Vec<Finding> {
    let Some(dirs) = defaults(target) else {
        return vec![Finding {
            path: target.to_string(),
            what: "- unknown-target".into(),
        }];
    };

    let names = match db::installed(root, target) {
        Ok(n) => n,
        Err(e) => die(e.to_string()),
    };

    let mut elves = Vec::new();
    let mut here: HashMap<String, usize> = HashMap::new();
    let mut links: HashMap<String, String> = HashMap::new();
    let mut shebangs: Vec<(String, Vec<String>)> = Vec::new();
    // every regular file, not only the ones that parse as elf: an interpreter is a file
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
        for e in &rec.manifest {
            if matches!(e.kind, db::Kind::File(_)) {
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

        let seen = match install::scan(root, &rec.manifest) {
            Ok(s) => s,
            Err(e) => die(e.to_string()),
        };

        let mut mine = Vec::new();
        for (path, what) in seen {
            let o = match what {
                install::Seen::Elf(o) => o,
                install::Seen::Script(words) => {
                    shebangs.push((path, words));
                    continue;
                }
                install::Seen::Other => continue,
                install::Seen::Bad => {
                    out.push(Finding {
                        path,
                        what: "unreadable".into(),
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
                        path: name.clone(),
                        what: "stale-provides".into(),
                    });
                }
            }
            Err(e) => die(e.to_string()),
        }
    }

    let sets = exported(&elves);
    let mut linked: HashSet<usize> = HashSet::new();
    for (path, o) in &elves {
        let where_ = search(o, path, dirs);
        for want in &o.needed {
            match provider(&here, &links, want, &where_) {
                Some(j) => {
                    linked.insert(j);
                    // musl ignores symbol versions, so a gnu binary that reaches into
                    // the musl tree binds to whatever has the right name and nothing
                    // errors. loader paths keep them apart until an rpath crosses over
                    if target.ends_with("gnu") && elves[j].0.starts_with("usr/lib/") {
                        out.push(Finding {
                            path: path.clone(),
                            what: format!("cross-tier {}", elves[j].0),
                        });
                    }
                }
                None => out.push(Finding {
                    path: path.clone(),
                    what: format!("unresolved {want}"),
                }),
            }
        }
    }

    // the kernel will not start a script whose interpreter is not there, which is the
    // same failure DT_NEEDED describes and nothing was checking it
    for (path, words) in &shebangs {
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
            exists(&present, &links, &want)
        } else {
            ["usr/bin", "usr/sbin"]
                .iter()
                .any(|d| exists(&present, &links, &format!("{d}/{want}")))
        };
        if !there {
            out.push(Finding {
                path: path.clone(),
                what: format!("no-interpreter {}", words.join(" ")),
            });
        }
    }

    for (i, (path, o)) in elves.iter().enumerate() {
        // a library nothing links can only arrive through dlopen, and then its symbols
        // come from whichever process opened it. python ships 4825 of those
        if !o.interp && !linked.contains(&i) {
            continue;
        }
        for want in missing(&elves, &sets, &here, &links, dirs, i) {
            out.push(Finding {
                path: path.clone(),
                what: format!("missing-symbol {want}"),
            });
        }
    }

    // two libraries exporting one name means load order decides which implementation a
    // caller gets, silently. the index holds every export already, so this is a group-by
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, o)) in elves.iter().enumerate() {
        if o.soname.is_none() || !linked.contains(&i) {
            continue;
        }
        for sym in &o.exports {
            // a weak export is c++ vague linkage: an inline function or a template
            // instantiation lands in every object that used it, and the linker means for
            // them to collapse. a version label names its own version node and every
            // library that shares a version name carries one
            if sym.weak
                || sym.version.as_deref() == Some(sym.name.as_str())
                || HOUSEKEEPING.contains(&sym.name.as_str())
            {
                continue;
            }
            let who = by_name.entry(&sym.name).or_default();
            // one library exporting a name at two versions is one library, and exports
            // arrive here in file order, so the last entry is the only one to look at
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
            path: elves[a].0.clone(),
            what: format!("duplicate-symbols {n} {}", elves[b].0),
        });
    }
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

// a soname check passes a library that kept its name and dropped half its symbols, and
// the binary linked against it dies at the first call that is not there. lazy binding
// means launching it proves nothing either, so the definitions get counted here
//
// rpath inheritance down the closure is not modelled: a transitive library reachable
// only through its parent's rpath reports as unresolved on its own walk instead, which
// is a second finding rather than a missed one
// one pair of sets per file, built once for the whole walk. unioning a closure per
// consumer instead is tens of millions of inserts on a real root, to answer a question
// that only ever needed a lookup
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
        if d.make {
            say!("dep {} make", d.name);
        } else {
            say!("dep {}", d.name);
        }
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
