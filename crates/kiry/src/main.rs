mod convert;
mod sandbox;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

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
        Some("-h") | Some("--help") | Some("help") => usage(),
        Some("--version") => say!("kiry {}", env!("CARGO_PKG_VERSION")),
        Some("b") => build_cmd(&args[1..]),
        Some("i") => install_cmd(&args[1..]),
        Some("r") => remove_cmd(&args[1..]),
        Some("l") => list_cmd(&args[1..]),
        Some("doctor") => doctor_cmd(&args[1..]),
        Some("owns") => owns_cmd(&args[1..]),
        Some("rebuild") => rebuild_cmd(&args[1..]),
        Some("flags") => flags_cmd(&args[1..]),
        Some("why") => why_cmd(&args[1..]),
        Some("ahead") => ahead_cmd(&args[1..]),
        Some("sync") => sync_cmd(&args[1..]),
        Some("promote") => promote_cmd(&args[1..]),
        Some("commit") => commit_cmd(&args[1..]),
        Some("rollback") => rollback_cmd(&args[1..]),
        Some("gc") => gc_cmd(&args[1..]),
        Some("search") => search_cmd(&args[1..]),
        Some("log") => log_cmd(&args[1..]),
        Some("stats") => stats_cmd(&args[1..]),
        Some("convert") => convert_cmd(&args[1..]),
        Some("sandbox") => {
            if let Err(e) = sandbox::init() {
                die(e);
            }
        }
        Some(_) => show(&args),
        None => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    say!("usage: kiry <command> [args]");
    say!("");
    say!("  i <pkg>...    build what is missing, then install (-n to just say what)");
    say!("                  --live forces / when it would have used the other root");
    say!("  b <pkg>...    build only, into the cache");
    say!("  r <pkg>...    remove");
    say!("  l             what is installed");
    say!("");
    say!("i is enough on its own. it builds the package and everything under it, in");
    say!("order, then installs. use b when you want the build without the install, or");
    say!("when you want to force one: b always compiles, i takes what is in the cache.");
    say!("so `i foo` installs foo, and `b foo` then `i foo` rebuilds it.");
    say!("");
    say!("b and i also take a set: @outdated is everything installed at a version the");
    say!("tree has since moved past, which is the upgrade, and @world is everything");
    say!("installed. `i @outdated` is the whole of it -- one batch, in order, or none.");
    say!("");
    say!("a package is named rather than pointed at: mesa, core/mesa, or a path to a");
    say!("recipe. b and i take --target T, -v to stream the build, --recover to let the");
    say!("failure table retry, and --force to override a conflict.");
    say!("");
    say!("keeping up");
    say!("  ahead [--net] [pkg]...  which versions upstream moved past");
    say!("  sync [-n] [--net]       bump those into testing/");
    say!("  promote <pkg>...        testing/ into the tree");
    say!("  rebuild [-n]            drain the soname rebuild queue");
    say!("");
    say!("the other root");
    say!("  commit                  keep the root that is running");
    say!("  rollback                go back to the one before it");
    say!("");
    say!("looking around");
    say!("  owns <path>...          which package owns a file");
    say!("  why <pkg>               what pulls it in");
    say!("  flags <pkg> | --queue   resolved flags and where they came from");
    say!("  log <pkg>               the last build log");
    say!("  search [term]           recipes on offer");
    say!("  doctor [--files] [--orphans]   every installed ELF resolves");
    say!("  stats");
    say!("  <pkg>                   what a recipe says");
    say!("");
    say!("housekeeping");
    say!("  gc [-n]                 drop what /var/kiry no longer needs");
    say!("  convert [-n] <APKBUILD>... <dir>");
    say!("");
    say!("--root DIR applies to anything that touches the installed tree and defaults to");
    say!("/; KIRY_ROOT is the environment form. --version prints the version alone.");
}

fn die(msg: String) -> ! {
    // an error carrying several findings writes one per line, and a line without the
    // prefix is a line that does not grep
    for l in msg.lines() {
        eprintln!("kiry: {l}");
    }
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

// what opts left behind, minus the flags the command knows. a leftover flag is refused
// rather than filtered out: --dry is not -n, and dropping it silently is how a sync that
// was asked to describe itself went and did the work instead
fn asked(rest: Vec<String>, known: &[&str]) -> Vec<String> {
    let mut bad = Vec::new();
    let mut want = Vec::new();
    for a in rest {
        match a.starts_with('-') {
            true if !known.contains(&a.as_str()) => bad.push(format!("no such flag: {a}")),
            true => {}
            false => want.push(a),
        }
    }
    if !bad.is_empty() {
        die(bad.join("\n"));
    }
    want
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
    let mut fix = false;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" => verbose = true,
            "--recover" => fix = true,
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
    let dirs = expand(&root, dirs);
    if dirs.is_empty() {
        return;
    }

    // loaded up front so the whole batch can be estimated before any of it starts, and
    // so a name that is nowhere is said before an hour of building rather than after
    let mut recipes: HashMap<String, Package> = HashMap::new();
    let mut todo: Vec<(String, Vec<String>)> = Vec::new();
    for d in &dirs {
        let at = match resolve(&root, d) {
            Ok(at) => at,
            Err(e) => die(e),
        };
        let p = match pkg::load(&at) {
            Ok(p) => p,
            Err(e) => die(e.to_string()),
        };
        let targets = match &want {
            Some(t) if !p.targets.contains(t) => die(format!("{} does not build for {t}", p.name)),
            Some(t) => vec![t.clone()],
            None => p.targets.clone(),
        };
        todo.push((p.name.clone(), targets));
        recipes.insert(p.name.clone(), p);
    }

    // b always builds, so what is in the cache does not shorten the estimate
    let past = history(&root);
    let builds: usize = todo.iter().map(|(_, ts)| ts.len()).sum();
    if builds > 1 {
        let (mut secs, mut new) = (0u64, 0);
        for (name, ts) in &todo {
            for t in ts {
                match past.get(&(name.clone(), t.clone())) {
                    Some(s) => secs += s,
                    None => new += 1,
                }
            }
        }
        let note = match new {
            0 => String::new(),
            u => format!("  {u} never built here"),
        };
        say!("{builds} builds  ~{}{note}", clock(secs));
    }

    // every target of a package at once. a target that packed while a later one was
    // still to fail is the drift a multi-target recipe exists to prevent
    for (name, targets) in &todo {
        let p = &recipes[name];
        let r = match fix {
            true => targets.iter().try_for_each(|t| recover(&root, p, t, verbose)),
            false => build(&root, p, targets, verbose, false, false).map(|_| ()),
        };
        if let Err(e) = r {
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
    reuse: bool,
    boot: bool,
) -> Result<Vec<PathBuf>, String> {
    let srcs = sources(root, p)?;
    let hash = recipe_hash(p, &srcs)?;
    let mut f = flags(root, &p.name, Some(&p.dir))?;
    f.reuse = reuse;

    let past = history(root);
    let mut built = Vec::new();
    for t in targets {
        let eta = match past.get(&(p.name.clone(), t.clone())) {
            Some(s) => format!("~{}", clock(*s)),
            None => "-".to_string(),
        };
        say!("{} {} {t} building {eta}", p.name, p.version.upstream);
        let start = Instant::now();
        let work = compile(root, p, t, &srcs, &f, verbose, boot)?;
        let secs = start.elapsed().as_secs();
        say!("{} {} {t} ok {}", p.name, p.version.upstream, clock(secs));
        keep_time(root, p, t, secs);
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
        meta(p, t, &hash, &f, art)?;
    }
    for (_, work) in &built {
        let _ = fs::remove_dir_all(work);
    }
    Ok(ready.into_iter().map(|(_, (art, _))| art).collect())
}

// what the build linked against, set beside what the recipe declared. a transitive
// member's shared libraries are in the namespace because link resolution needs them, so
// a direct dependency nobody wrote down links fine and nothing says a word. the header
// rule catches the ones that need a header to compile; this is the rest of them
//
// reported, not refused. the answer is a line in a depends file and it is his to write,
// and a build that just took twenty minutes is the wrong place to find that out fatally
fn undeclared(
    root: &Path,
    members: &[sandbox::Member],
    deps: &[Dep],
    target: &str,
    dest: &Path,
) -> Vec<(String, String, &'static str)> {
    let mut owner: HashMap<String, String> = HashMap::new();
    for m in members {
        let Ok(ps) = db::read_provides(root, &m.target, &m.name) else {
            continue;
        };
        for pv in ps {
            owner.entry(pv.soname).or_insert(m.name.clone());
        }
    }

    // what will still be there once the build is over. a make dep is a tool, and nothing
    // installs one on the strength of something linking it -- so a shipped file linking
    // a make dep's library is a package that works here and not on a clean root
    let mut declared: HashSet<String> = deps
        .iter()
        .filter(|d| !d.make && d.applies(target))
        .map(|d| d.name.clone())
        .collect();
    declared.extend(
        sandbox::substrate(target)
            .iter()
            .filter(|(_, make)| !make)
            .map(|(n, _)| (*n).to_string()),
    );

    let mut runtime: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = declared.iter().cloned().collect();
    while let Some(n) = queue.pop() {
        if !runtime.insert(n.clone()) {
            continue;
        }
        if let Ok(rec) = db::read(root, target, &n) {
            queue.extend(
                rec.depends
                    .iter()
                    .filter(|d| !d.make && d.applies(target))
                    .map(|d| d.name.clone()),
            );
        }
    }

    let mut files = Vec::new();
    under(dest, &mut files);
    let mut mine: HashSet<String> = HashSet::new();
    let mut want: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let Ok(o) = elf::read(f) else { continue };
        if let Some(s) = &o.soname {
            mine.insert(s.clone());
        }
        want.extend(o.needed.iter().cloned());
    }

    want.into_iter()
        // a package linking what it just built itself declares nothing
        .filter(|w| !mine.contains(w))
        .filter_map(|w| {
            let who = owner.get(&w)?;
            if declared.contains(who) {
                return None;
            }
            // reachable through a declared dep's own deps, so it is there today and
            // nothing wrote down that it has to be
            let kind = if runtime.contains(who) {
                "undeclared"
            } else {
                "build-only"
            };
            Some((w, who.clone(), kind))
        })
        .collect()
}

fn under(at: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(at) else { return };
    for e in rd.flatten() {
        match e.file_type() {
            // a symlink is the same file under a second name and its target is walked
            // anyway, so following one only reads everything twice
            Ok(ft) if ft.is_dir() => under(&e.path(), out),
            Ok(ft) if ft.is_file() => out.push(e.path()),
            _ => {}
        }
    }
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

    run(&mut fetcher(url, &part)?, url)?;

    fs::rename(&part, dst).map_err(|e| format!("{}: {e}", dst.display()))
}

// split out so a probe can silence the progress meter. the template is the same one, and
// a second env var for the same job is a second thing to get out of step
fn fetcher(url: &str, dst: &Path) -> Result<Command, String> {
    let tmpl = std::env::var("KIRY_FETCH").unwrap_or_else(|_| "curl -fL --retry 3 -o %o %u".into());
    let mut words = tmpl.split_whitespace();
    let Some(prog) = words.next() else {
        return Err("KIRY_FETCH is empty".into());
    };

    let mut c = Command::new(prog);
    for w in words {
        c.arg(
            w.replace("%o", &dst.display().to_string())
                .replace("%u", url),
        );
    }
    Ok(c)
}

// full flags, then less of them. a check failure prints nothing a table could match, so
// after the signatures are out this is the only thing left that knows anything
const LADDER: &[(bool, &str)] = &[
    (true, "filter-lto"),
    (false, "LTO none"),
    (false, "OPT -O2"),
    (false, "CFLAGS_MARCH"),
    (false, "CFLAGS -pipe"),
];

fn note(at: &Path, line: &str, why: &str) -> Result<(), String> {
    let had = fs::read_to_string(at).unwrap_or_default();
    if !line.is_empty() && had.lines().any(|l| l.split('#').next().unwrap_or("").trim() == line) {
        return Ok(());
    }
    if let Some(d) = at.parent() {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let gap = if had.is_empty() || had.ends_with('\n') { "" } else { "\n" };
    fs::write(at, format!("{had}{gap}# {} {why}\n{line}\n", today()))
        .map_err(|e| format!("{}: {e}", at.display()))?;
    if !line.is_empty() {
        say!("{} {line}", at.display());
    }
    Ok(())
}

fn stuck(p: &Package, t: &str, why: &str) -> String {
    let _ = note(&p.dir.join("filter"), "", &format!("stuck on {t}: {why}"));
    format!("{} {t}: stuck, {why}", p.name)
}

// a flag can only be the reason if a toolchain got far enough to have an opinion. a zip
// tool that was not installed, a cd into a directory that is not there, a source whose
// checksum moved -- none of those get better at -O2, and the ladder spends five rungs
// finding that out, leaving four settings and a filter line behind that someone then has
// to notice and take back out. a check that failed counts: it ran, and a miscompile is
// exactly what the ladder is for
fn compiled(log: &str) -> bool {
    phase(log) == "check"
        || log.lines().any(|l| {
            l.contains("error:")
                || l.contains("ld.lld:")
                || l.contains("LLVM ERROR")
                || l.contains("undefined reference")
                || l.contains("undefined symbol")
                || l.contains("collect2:")
                || l.trim_start().starts_with("FAILED:")
        })
}

// the line to put in front of someone. the last line is usually not it -- mozbuild signs
// off with where it wrote a profile, four lines under the ValueError that stopped it --
// so walk back for something that reads like a complaint and only fall back to the end
fn blame(log: &str) -> &str {
    let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .find(|l| {
            let s = l.to_lowercase();
            s.contains("error") || s.contains("not found") || s.contains("no such")
        })
        .or_else(|| lines.last())
        .map_or("the log is empty", |l| l.trim())
}

// build fails, read the log, fix it, build again. three signature retries and never the
// same action twice, then the ladder, then it is stuck and says why
fn recover(root: &Path, p: &Package, t: &str, verbose: bool) -> Result<(), String> {
    let rs = rules(root)?;
    let one = [t.to_string()];
    let mut tried: Vec<String> = Vec::new();
    let mut rung = 0;
    let mut reuse = false;
    // so a first.log that is there is always this run's
    let first = firstlog(root, p, t);
    let _ = fs::remove_file(&first);

    loop {
        match build(root, p, &one, verbose, reuse, false) {
            Ok(_) => return Ok(()),
            Err(e) => say!("{e}"),
        }
        let log = fs::read_to_string(logpath(root, p, t)).unwrap_or_default();
        reuse = false;
        // every retry truncates the log, so without this the failure that started it all
        // is gone by the time anyone reads it and the rung that fixed it cannot be judged
        if !first.exists() {
            let _ = fs::copy(logpath(root, p, t), &first);
        }

        if tried.len() < 3 {
            if let Some(f) = scan(&rs, &log) {
                if let Some(why) = f.act.strip_prefix("notaflag ") {
                    return Err(stuck(p, t, why));
                }
                if !tried.contains(&f.act) {
                    tried.push(f.act.clone());
                    say!("{} {t} {} phase, {}", p.name, phase(&log), f.rule);
                    if write_fix(root, &p.dir, &p.name, &f, &log)? {
                        reuse = f.reuse;
                        continue;
                    }
                }
            }
        }

        if !compiled(&log) {
            // no note anywhere: the recipe and the settings are both innocent here
            say!("first failure is {}", first.display());
            return Err(format!(
                "{} {t}: stuck, nothing compiled: {}",
                p.name,
                blame(&log)
            ));
        }

        let Some((filter, line)) = LADDER.get(rung) else {
            say!("first failure is {}", first.display());
            return Err(stuck(p, t, "the ladder ran out"));
        };
        rung += 1;
        let at = match filter {
            true => p.dir.join("filter"),
            false => root.join("etc/kiry/pkg").join(&p.name),
        };
        note(&at, line, &format!("rung {rung} after {} failed on {t}", phase(&log)))?;
    }
}

fn cached(root: &Path, p: &Package, t: &str) -> Option<PathBuf> {
    let a = root.join("var/kiry/cache").join(format!(
        "{}-{}-{}.{t}.tar.zst",
        p.name, p.version.upstream, p.version.rev
    ));
    if !a.is_file() {
        return None;
    }
    match stale(root, p, &a) {
        None => Some(a),
        // an artifact whose name still fits and whose key no longer does is news. the
        // version did not move and the thing that decides the build did
        Some(why) => {
            say!("{} {} {t} rebuilding  {why}", p.name, p.version.upstream);
            None
        }
    }
}

// the file name carries name, version and rev, so what is left to decide is everything
// that changes a build without moving any of them: the recipe, and the flags it compiles
// with. the dependency closure is deliberately not in here -- see TODO
//
// a sidecar written before kiry recorded one of these says nothing rather than no. the
// check would otherwise throw away a cache that is mostly still good on the day it lands
fn stale(root: &Path, p: &Package, art: &Path) -> Option<&'static str> {
    let mut d = art.as_os_str().to_owned();
    d.push(".meta");
    let d = PathBuf::from(d);

    if let Ok(was) = fs::read_to_string(d.join("recipe")) {
        match dir_hash(p) {
            Ok(now) if now == was.trim() => {}
            _ => return Some("recipe changed"),
        }
    }
    if let Ok(was) = fs::read_to_string(d.join("flags")) {
        let had: Vec<String> = was
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        match flags(root, &p.name, Some(&p.dir)) {
            Ok(f) if f.record() == had => {}
            _ => return Some("flags changed"),
        }
    }
    None
}

// the attempt that started a recovery, kept beside the log the retries keep overwriting
fn firstlog(root: &Path, p: &Package, t: &str) -> PathBuf {
    root.join("var/kiry/log").join(format!(
        "{}-{}-{}.{t}.first.log",
        p.name, p.version.upstream, p.version.rev
    ))
}

fn logpath(root: &Path, p: &Package, t: &str) -> PathBuf {
    root.join("var/kiry/log").join(format!(
        "{}-{}-{}.{t}.log",
        p.name, p.version.upstream, p.version.rev
    ))
}

fn compile(
    root: &Path,
    p: &Package,
    t: &str,
    srcs: &[(String, PathBuf, String)],
    f: &Flags,
    verbose: bool,
    boot: bool,
) -> Result<PathBuf, String> {
    let work = root.join("var/kiry/stage").join(format!(
        "{}-{}-{}.{t}",
        p.name, p.version.upstream, p.version.rev
    ));
    let src = work.join("src");
    let dest = work.join("dest");

    // a reuse retry keeps every object that compiled and re-invokes the build system,
    // which relinks and nothing else. the source is byte-identical and only flags moved,
    // so what is stale is decidable -- this is not the persistent tree that was rejected,
    // and the tree still goes when the sequence ends
    let again = f.reuse && src.is_dir();
    if !again {
        let _ = fs::remove_dir_all(&work);
        mkdirs(&src)?;
    }
    let _ = fs::remove_dir_all(&dest);
    mkdirs(&dest)?;

    for (name, path, _) in srcs.iter().filter(|_| !again) {
        let name = name.as_str();
        if tarball(name) {
            untar(path, &src, name)?;
        } else {
            fs::copy(path, src.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
    }

    // a bootstrap file declares that this package is the one that breaks its cycle. with
    // nothing in it that is the whole claim -- the first pass is the ordinary build, run
    // before the rest of the cycle rather than after, which is all some cycles are. one
    // with a script in it runs that instead
    let mut script = p.dir.join("build");
    if boot {
        let at = p.dir.join("bootstrap");
        let has = fs::read_to_string(&at).map_or(false, |t| {
            t.lines().any(|l| {
                let l = l.trim();
                !l.is_empty() && !l.starts_with('#')
            })
        });
        if has {
            script = at;
        }
    }
    if !script.is_file() {
        return Err(format!("{}: no build script", p.dir.display()));
    }

    let sysroot = work.join("sysroot");
    let deps: Vec<Dep> = p.depends.iter().filter(|d| d.applies(t)).cloned().collect();
    let members = sandbox::closure(root, t, &deps)?;
    sandbox::assemble(root, t, &members, &sysroot)?;

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
        .env("KIRY_REV", p.version.rev.to_string())
        .env("CFLAGS", &f.cflags)
        .env("CXXFLAGS", &f.cxxflags)
        .env("LDFLAGS", &f.ldflags)
        .env("RUSTFLAGS", &f.rustflags)
        .env("KIRY_FLAGS", f.use_flags.join(" "));
    for (k, v) in &f.env {
        c.env(k, v);
    }

    // read on the far side of the clear, so they have to cross by name. without the
    // cap a build that allocates without bound takes the machine down, not itself
    for k in ["KIRY_MEM", "KIRY_HOST"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }

    // the log is written either way. -v only decides whether you also watch it
    let log = logpath(root, p, t);
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
            // after trim, so a gnu binary that gets dropped does not argue for a dep
            for (soname, who, kind) in undeclared(root, &members, &deps, t, &dest) {
                say!("{} {t} {kind} {who}  {soname}", p.name);
            }
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
# gentoo's names, doing to the environment what the recipe's filter file does to the
# resolution. both halves exist because some builds only find out at configure time
_kiry_drop() {
\t_pat=$1
\tshift
\t_out=
\tfor _f in \"$@\"; do
\t\tcase \"$_f\" in
\t\t$_pat) ;;
\t\t*) _out=\"$_out $_f\" ;;
\t\tesac
\tdone
\techo \"${_out# }\"
}

filter-flags() {
\tfor _p in \"$@\"; do
\t\tCFLAGS=$(_kiry_drop \"$_p\" $CFLAGS)
\t\tCXXFLAGS=$(_kiry_drop \"$_p\" $CXXFLAGS)
\tdone
\texport CFLAGS CXXFLAGS
}

filter-ldflags() {
\tfor _p in \"$@\"; do
\t\tLDFLAGS=$(_kiry_drop \"$_p\" $LDFLAGS)
\tdone
\texport LDFLAGS
}

filter-lto() {
\tfilter-flags '-flto*'
\tfilter-ldflags '-flto*' '-Wl,--thinlto-jobs=*'
}

append-flags() {
\tCFLAGS=\"$CFLAGS $*\"
\tCXXFLAGS=\"$CXXFLAGS $*\"
\texport CFLAGS CXXFLAGS
}

# in place, so the declarative half in the recipe filter file gives the same answer
replace-flags() {
\t_c= _x=
\tfor _f in $CFLAGS; do
\t\tcase \"$_f\" in $1) _c=\"$_c $2\" ;; *) _c=\"$_c $_f\" ;; esac
\tdone
\tfor _f in $CXXFLAGS; do
\t\tcase \"$_f\" in $1) _x=\"$_x $2\" ;; *) _x=\"$_x $_f\" ;; esac
\tdone
\tCFLAGS=${_c# }
\tCXXFLAGS=${_x# }
\texport CFLAGS CXXFLAGS
}

strip-flags() {
\t_c= _x=
\tfor _f in $CFLAGS; do
\t\tcase \"$_f\" in -O*|-march=*|-mtune=*|-mcpu=*|-pipe|-g*) _c=\"$_c $_f\" ;; esac
\tdone
\tfor _f in $CXXFLAGS; do
\t\tcase \"$_f\" in -O*|-march=*|-mtune=*|-mcpu=*|-pipe|-g*) _x=\"$_x $_f\" ;; esac
\tdone
\tCFLAGS=${_c# }
\tCXXFLAGS=${_x# }
\texport CFLAGS CXXFLAGS
}

# the one that has to ask the compiler, so it prints what survived instead of setting it
test-flags() {
\t_out=
\tfor _f in \"$@\"; do
\t\tif ${CC:-cc} \"$_f\" -x c -c -o /dev/null /dev/null 2>/dev/null; then
\t\t\t_out=\"$_out $_f\"
\t\tfi
\tdone
\techo \"${_out# }\"
}

want_check() { return 1; }
options_has() { case \" $options \" in *\" $1 \"*) return 0 ;; esac; return 1; }
";

// autoconf sources this before it decides anything, so libdir follows the target
// without a recipe passing --libdir. hand-rolled configures do not read it and still
// need the flag; that is a handful of recipes rather than all of them
//
// only where the recipe said nothing. autoconf reads the site file after it has parsed
// the arguments, so a plain assignment here silently beats --includedir -- nspr asked
// for /usr/include/nspr, got /usr/include, and pkg-config then dropped the -I as a
// system default, which is how librewolf's gyp read ended up with an include of '%'
// the strings compared are autoconf's own defaults, still unexpanded at this point
// each one is an if rather than a && so the file cannot end on a false test, which
// autoconf 2.71 treats as the site script having failed
const CONFIG_SITE: &str = "\
if test \"$libdir\" = '${exec_prefix}/lib'; then libdir=@LIBDIR@; fi
if test \"$includedir\" = '${prefix}/include'; then includedir=@INCLUDEDIR@; fi
if test \"$datarootdir\" = '${prefix}/share'; then datarootdir=@DATADIR@; fi
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
// cache entries rather than plain variables, bc GNUInstallDirs runs set_property(CACHE)
// over whatever is already defined and a plain set leaves that nothing to act on --
// libjpeg-turbo bundles a fork that errors out right there. FORCE bc which libdir a
// target uses is kiry's call, not the recipe's -- every converted APKBUILD hardcodes
// alpine's lib, and letting that through puts a gnu package in the musl tree
// the retype above them is the other half: an untyped -DCMAKE_INSTALL_LIBDIR=lib lands in
// the cache as UNINITIALIZED, and cmake resolves a relative one against the build dir the
// moment anything retypes it to PATH. libogg packaged its own build tree that way. settling
// the type here means the conversion never gets the chance
//
// the per-config flags are the same argument as -Dbuildtype=plain on the meson side
// CMAKE_BUILD_TYPE=Release means -O3 -DNDEBUG and cmake appends it after the CXXFLAGS
// kiry exported, so a config that says OPT -O2 gets built at -O3 and nothing says so
// llvm found this the expensive way: the -O3 reaches the link too, becomes
// -plugin-opt=O3, and lld 20.1.8 segfaults there with no diagnostic at all. NDEBUG stays,
// bc that is what the build type means once the optimisation level is kiry's -- llvm
// without it builds its assertions. defined here before the compiler is enabled, bc
// cmake_initialize_per_config_variable only fills these in when they are not already set
const TOOLCHAIN_CMAKE: &str = "\
foreach(_d CMAKE_INSTALL_LIBDIR CMAKE_INSTALL_INCLUDEDIR CMAKE_INSTALL_DATAROOTDIR)
  if(DEFINED CACHE{${_d}})
    set_property(CACHE ${_d} PROPERTY TYPE STRING)
  endif()
endforeach()
set(CMAKE_INSTALL_LIBDIR \"@LIBDIR@\" CACHE STRING \"\" FORCE)
set(CMAKE_INSTALL_INCLUDEDIR \"@INCLUDEDIR@\" CACHE STRING \"\" FORCE)
set(CMAKE_INSTALL_DATAROOTDIR \"@DATADIR@\" CACHE STRING \"\" FORCE)
foreach(_l C CXX)
  set(CMAKE_${_l}_FLAGS_DEBUG \"\" CACHE STRING \"\" FORCE)
  foreach(_c RELEASE MINSIZEREL RELWITHDEBINFO)
    set(CMAKE_${_l}_FLAGS_${_c} \"-DNDEBUG\" CACHE STRING \"\" FORCE)
  endforeach()
endforeach()
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

// as root tar restores the archive's own uids, and the sandbox maps one id, so a build
// running as root gets a /src it cannot chown inside the namespace
//
// busybox's xz decoder caps the dictionary it will allocate and rustc's source tarball is
// packed with a 128 MiB one, so tar reads it as "corrupted data" on a file whose sha256 is
// exactly what the recipe asked for. xz itself does the decoding and tar only ever sees a
// tar -- the same shape as pack(), which already pipes through zstd. everything else keeps
// tar's own decompression, which has never been the thing that was wrong
fn untar(path: &Path, into: &Path, name: &str) -> Result<(), String> {
    let tar = |extra: Option<Stdio>| {
        let mut c = Command::new("tar");
        c.arg("--no-same-owner").arg("-xf");
        match extra {
            Some(p) => c.arg("-").stdin(p),
            None => c.arg(path),
        };
        c.arg("-C").arg(into);
        c
    };

    if !(name.ends_with(".xz") || name.ends_with(".txz") || name.ends_with(".lzma")) {
        return run(&mut tar(None), name);
    }

    let mut x = Command::new("xz")
        .arg("-dc")
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: xz: {e}"))?;
    let Some(pipe) = x.stdout.take() else {
        return Err(format!("{name}: xz gave us no pipe"));
    };

    let t = tar(Some(Stdio::from(pipe)))
        .status()
        .map_err(|e| format!("{name}: tar: {e}"))?;
    let x = x.wait().map_err(|e| format!("{name}: xz: {e}"))?;
    if !x.success() {
        return Err(format!("{name}: xz: {x}"));
    }
    if !t.success() {
        return Err(format!("{name}: tar: {t}"));
    }
    Ok(())
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

fn meta(p: &Package, t: &str, hash: &str, f: &Flags, art: &Path) -> Result<(), String> {
    let mut d = art.as_os_str().to_owned();
    d.push(".meta");
    let d = PathBuf::from(d);
    let _ = fs::remove_dir_all(&d);
    mkdirs(&d)?;

    put(&d.join("name"), &format!("{}\n", p.name))?;
    put(&d.join("version"), &format!("{}\n", p.version))?;
    put(&d.join("targets"), &format!("{t}\n"))?;
    put(&d.join("hash"), &format!("{hash}\n"))?;
    put(&d.join("recipe"), &format!("{}\n", dir_hash(p)?))?;
    put(&d.join("users"), &format!("{}\n", p.users.join("\n")))?;
    put(&d.join("flags"), &format!("{}\n", f.record().join("\n")))?;

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
    let mut blob = dir_blob(p)?;
    // anything the build reads is either one of those files or a listed source
    for (_, _, sum) in srcs {
        blob.extend_from_slice(sum.as_bytes());
        blob.push(b'\n');
    }
    kiry_core::sha256(&blob[..]).map_err(|e| e.to_string())
}

// the recipe half of the cache key, and no sources in it on purpose: a source that
// changed is a checksums line that changed, and checksums is one of these files. that is
// what lets a cache hit be decided without fetching anything or rehashing a tarball
fn dir_hash(p: &Package) -> Result<String, String> {
    kiry_core::sha256(&dir_blob(p)?[..]).map_err(|e| e.to_string())
}

fn dir_blob(p: &Package) -> Result<Vec<u8>, String> {
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
    Ok(blob)
}

// what a build compiles with. /etc/kiry/config first, then /etc/kiry/pkg/<name>, and
// the two do not combine the same way: a knob is replaced so a package can say -O2
// against a global -O3, while CFLAGS and its siblings append so a global change still
// reaches a package that only added to it. taking something back out is what a recipe's
// filter file is for
//
// knob names are the ones the failure table writes, so that table lands without a
// translation step
const KNOBS: &[(&str, &str)] = &[
    ("OPT", "-O2"),
    // empty rather than znver3: the machine belongs in the config, not the binary
    ("CFLAGS_MARCH", ""),
    ("LTO", "none"),
    ("KIRY_THINLTO_JOBS", "8"),
];

struct Flags {
    // set only by a recovery retry whose rule said the fix touches no compile flag
    reuse: bool,
    cflags: String,
    cxxflags: String,
    ldflags: String,
    rustflags: String,
    use_flags: Vec<String>,
    // KIRY_* settings, handed to the build as themselves
    env: Vec<(String, String)>,
    // every line that contributed, in the order it was read
    from: Vec<(String, String, String)>,
}

impl Flags {
    // what goes in the record and the sidecar, and what a later resolve is compared
    // against. only what a compiler sees: the provenance is not part of the answer
    fn record(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, v) in [
            ("CFLAGS", &self.cflags),
            ("CXXFLAGS", &self.cxxflags),
            ("LDFLAGS", &self.ldflags),
            ("RUSTFLAGS", &self.rustflags),
        ] {
            if !v.is_empty() {
                out.push(format!("{k} {v}"));
            }
        }
        if !self.use_flags.is_empty() {
            out.push(format!("FLAGS {}", self.use_flags.join(" ")));
        }
        out
    }
}

fn settings(root: &Path, name: &str) -> Result<Vec<(String, String, String)>, String> {
    let mut out = Vec::new();
    for f in [
        PathBuf::from("etc/kiry/config"),
        Path::new("etc/kiry/pkg").join(name),
    ] {
        let at = root.join(&f);
        let text = match fs::read_to_string(&at) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", at.display())),
        };
        for (n, l) in text.lines().enumerate() {
            let l = l.split('#').next().unwrap_or("").trim();
            if l.is_empty() {
                continue;
            }
            let (k, v) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
            let known = k == "flags"
                || k.starts_with("KIRY_")
                || matches!(k, "CFLAGS" | "CXXFLAGS" | "LDFLAGS" | "RUSTFLAGS")
                || KNOBS.iter().any(|(n, _)| *n == k);
            if !known {
                return Err(format!("/{}:{}: {k} is not a setting", f.display(), n + 1));
            }
            out.push((format!("/{}", f.display()), k.into(), v.trim().into()));
        }
    }
    Ok(out)
}

// gentoo's names, because the portage grep that seeds these files writes them and a
// translation step would be one more thing to get wrong. subtractive on purpose: the
// recipe takes something out of what resolved, it does not restate the set, so raising
// -O2 to -O3 globally still reaches a package that only dropped lto
const VERBS: &[&str] = &[
    "filter-lto",
    "filter-flags",
    "filter-ldflags",
    "strip-flags",
    "append-flags",
    "replace-flags",
];

// what strip-flags leaves standing. anything that changes code generation in a way a
// build can be picky about goes, and the ones that decide how fast and for what stay
const KEPT_FLAGS: &[&str] = &["-O", "-march=", "-mtune=", "-mcpu=", "-pipe", "-g"];

// one trailing * and nothing else. enough for -march=* and -flto*, and a full glob
// would invite patterns nobody can predict the effect of
fn like(pat: &str, s: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(p) => s.starts_with(p),
        None => s == pat,
    }
}

fn filters(dir: &Path) -> Result<Vec<(String, String, String)>, String> {
    let at = dir.join("filter");
    let text = match fs::read_to_string(&at) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", at.display())),
    };
    let mut out = Vec::new();
    for (n, l) in text.lines().enumerate() {
        // the reason lives on the line, so a filter that looks wrong in six months has
        // it beside it rather than in a commit nobody will go looking for
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        let (k, v) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
        if !VERBS.contains(&k) {
            return Err(format!("{}:{}: {k} is not a flag-o-matic verb", at.display(), n + 1));
        }
        out.push((at.display().to_string(), k.into(), v.trim().into()));
    }
    Ok(out)
}

// applied after both config files, so what a recipe takes out is taken out of whatever
// resolved rather than of a fixed set
fn filtered(
    from: &[(String, String, String)],
    c: &mut Vec<String>,
    cxx: &mut Vec<String>,
    ld: &mut Vec<String>,
) -> Result<(), String> {
    for (src, k, v) in from {
        let args: Vec<&str> = v.split_whitespace().collect();
        match k.as_str() {
            // the flag is in all three: clang wants it compiling and linking both, and
            // the jobs cap is meaningless once it is gone
            "filter-lto" => {
                for l in [&mut *c, &mut *cxx, &mut *ld] {
                    l.retain(|f| !f.starts_with("-flto") && !f.starts_with("-Wl,--thinlto-jobs"));
                }
            }
            "filter-flags" => {
                for l in [&mut *c, &mut *cxx] {
                    l.retain(|f| !args.iter().any(|p| like(p, f)));
                }
            }
            "filter-ldflags" => ld.retain(|f| !args.iter().any(|p| like(p, f))),
            "strip-flags" => {
                for l in [&mut *c, &mut *cxx] {
                    l.retain(|f| KEPT_FLAGS.iter().any(|k| f.starts_with(k)));
                }
            }
            "append-flags" => {
                for a in &args {
                    c.push((*a).to_string());
                    cxx.push((*a).to_string());
                }
            }
            "replace-flags" => {
                let [was, now] = args[..] else {
                    return Err(format!("{src}: replace-flags wants two flags, got {v:?}"));
                };
                for l in [&mut *c, &mut *cxx] {
                    for f in l.iter_mut() {
                        if like(was, f) {
                            *f = now.to_string();
                        }
                    }
                }
            }
            _ => unreachable!("verb list and match are the same list"),
        }
    }
    Ok(())
}

fn flags(root: &Path, name: &str, dir: Option<&Path>) -> Result<Flags, String> {
    let from = settings(root, name)?;
    let mut knob: HashMap<&str, String> =
        KNOBS.iter().map(|(k, v)| (*k, (*v).to_string())).collect();
    let (mut c, mut cxx, mut ld) = (Vec::new(), Vec::new(), Vec::new());
    let mut rs: Vec<String> = Vec::new();
    let mut use_flags: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();

    for (_, k, v) in &from {
        match k.as_str() {
            "CFLAGS" => c.push(v.clone()),
            "CXXFLAGS" => cxx.push(v.clone()),
            "LDFLAGS" => ld.push(v.clone()),
            "RUSTFLAGS" => rs.push(v.clone()),
            // a later - takes one back off, so a package subtracts from the global set
            // without restating it
            "flags" => {
                for t in v.split_whitespace() {
                    let (on, n) = match t.strip_prefix('-') {
                        Some(n) => (false, n),
                        None => (true, t.strip_prefix('+').unwrap_or(t)),
                    };
                    use_flags.retain(|x| x != n);
                    if on {
                        use_flags.push(n.to_string());
                    }
                }
            }
            _ => {
                if let Some(slot) = knob.get_mut(k.as_str()) {
                    slot.clone_from(v);
                }
                if k.starts_with("KIRY_") {
                    env.retain(|(x, _)| x != k);
                    env.push((k.clone(), v.clone()));
                }
            }
        }
    }

    let lto = knob["LTO"].as_str();
    if !matches!(lto, "none" | "thin" | "full") {
        return Err(format!("LTO is none, thin or full, not {lto}"));
    }
    let jobs = &knob["KIRY_THINLTO_JOBS"];
    if jobs.parse::<u32>().is_err() {
        return Err(format!("KIRY_THINLTO_JOBS wants a number, not {jobs}"));
    }

    let mut base: Vec<String> = Vec::new();
    if !knob["OPT"].is_empty() {
        base.push(knob["OPT"].clone());
    }
    if !knob["CFLAGS_MARCH"].is_empty() {
        base.push(format!("-march={}", knob["CFLAGS_MARCH"]));
    }
    if lto != "none" {
        base.push(format!("-flto={lto}"));
    }
    base.push("-g0".into());

    let mut link: Vec<String> = Vec::new();
    if lto != "none" {
        link.push(format!("-flto={lto}"));
    }
    // 16 links at once against 32GB is where the machine starts swapping, and swapping
    // costs more than the parallelism returns
    if lto == "thin" {
        link.push(format!("-Wl,--thinlto-jobs={jobs}"));
    }
    // lld's branch-to-branch rewriting, which it does not do below -O2
    link.push("-Wl,-O2".into());

    // rustc reads none of the above, so -march never reached a rust package. no lto
    // here though: cargo picks embed-bitcode from the profile and rustc refuses the
    // pair, so which lto a crate gets stays its Cargo.toml's business
    let mut rbase: Vec<String> = Vec::new();
    if !knob["CFLAGS_MARCH"].is_empty() {
        rbase.push(format!("-C target-cpu={}", knob["CFLAGS_MARCH"]));
    }
    if let Some(l) = knob["OPT"].strip_prefix("-O") {
        if matches!(l, "0" | "1" | "2" | "3" | "s" | "z") {
            rbase.push(format!("-C opt-level={l}"));
        }
    }

    // the file's own lines last, so appending -O0 or -g really does win
    let mut cf: Vec<String> = base.iter().chain(c.iter()).flat_map(words).collect();
    let mut cxf: Vec<String> = base.iter().chain(cxx.iter()).flat_map(words).collect();
    let mut ldf: Vec<String> = link.iter().chain(ld.iter()).flat_map(words).collect();
    let rsf: Vec<String> = rbase.iter().chain(rs.iter()).flat_map(words).collect();

    let mut from = from;
    if let Some(d) = dir {
        let f = filters(d)?;
        filtered(&f, &mut cf, &mut cxf, &mut ldf)?;
        from.extend(f);
    }

    use_flags.sort();
    Ok(Flags {
        reuse: false,
        cflags: cf.join(" "),
        cxxflags: cxf.join(" "),
        ldflags: ldf.join(" "),
        rustflags: rsf.join(" "),
        use_flags,
        env,
        from,
    })
}

// a setting holds a whole line of flags and a filter names one at a time, so the line
// has to come apart before anything can be taken out of it
fn words(s: &String) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn flags_cmd(args: &[String]) {
    let mut queue = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "--queue" {
            queue = true;
        } else {
            rest.push(a.clone());
        }
    }
    let (root, _, want) = opts(&rest);
    if let Some(name) = want.first() {
        if want.len() > 1 {
            die("flags takes one package".into());
        }
        if queue {
            die("--queue takes no package".into());
        }
        let dir = recipe(&root, name);
        let f = match flags(&root, name, dir.as_deref()) {
            Ok(f) => f,
            Err(e) => die(e),
        };
        for (src, k, v) in &f.from {
            match v.is_empty() {
                true => say!("{src} {k}"),
                false => say!("{src} {k} {v}"),
            }
        }
        for l in f.record() {
            say!("resolved {l}");
        }
        return;
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };
    let mut stale = Vec::new();
    for t in &targets {
        for name in db::installed(&root, t).unwrap_or_default() {
            let Ok(rec) = db::read(&root, t, &name) else {
                continue;
            };
            let dir = recipe(&root, &name);
            let now = match flags(&root, &name, dir.as_deref()) {
                Ok(f) => f.record(),
                Err(e) => die(e),
            };
            if rec.flags == now {
                continue;
            }
            let why = if rec.flags.is_empty() {
                "unrecorded"
            } else {
                "stale"
            };
            say!("{name} {t} {why}");
            stale.push(db::Queued {
                target: t.clone(),
                name,
                soname: "flags".into(),
                changed: Vec::new(),
            });
        }
    }
    if !queue {
        return;
    }
    // this command owns the flags rows. a package that resolves clean now should not
    // still be listed because it was stale an hour ago
    let mut all: Vec<db::Queued> = db::read_queue(&root)
        .unwrap_or_default()
        .into_iter()
        .filter(|q| q.soname != "flags")
        .collect();
    all.extend(stale);
    all.sort();
    all.dedup();
    match db::write_queue(&root, &all) {
        Ok(()) if all.is_empty() => {}
        Ok(()) => say!("queued {}", all.len()),
        Err(e) => die(e.to_string()),
    }
}

// the subset the failure table needs and no more: literals, . , [\w=-] classes, \w \d \s,
// (a|b) with a capture, and + * ? on anything that is not a group. five more crates for
// patterns nobody is going to write is the trade thiserror and blake3 already lost
//
// two deliberate limits, both rejected at parse time rather than silently: a group takes
// no quantifier, and groups do not nest. the table's alternations are literal strings,
// so the first end a group finds is its only one
#[derive(Debug, Clone)]
enum Node {
    Lit(char),
    Any,
    Class { spans: Vec<(char, char)>, neg: bool },
    Group(usize, Vec<Vec<Piece>>),
}

#[derive(Debug, Clone)]
struct Piece {
    node: Node,
    rep: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct Rx {
    prog: Vec<Piece>,
    groups: usize,
    src: String,
}

fn class_for(c: char) -> Option<Vec<(char, char)>> {
    Some(match c {
        'w' => vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
        'd' => vec![('0', '9')],
        's' => vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
        _ => return None,
    })
}

fn brackets(cs: &[char], i: &mut usize) -> Result<Node, String> {
    let neg = cs.get(*i) == Some(&'^');
    if neg {
        *i += 1;
    }
    let mut spans = Vec::new();
    let mut first = true;
    loop {
        let Some(&c) = cs.get(*i) else {
            return Err("[ with no ]".into());
        };
        if c == ']' && !first {
            *i += 1;
            return Ok(Node::Class { spans, neg });
        }
        first = false;
        *i += 1;
        if c == '\\' {
            let Some(&e) = cs.get(*i) else {
                return Err("\\ at the end of a class".into());
            };
            *i += 1;
            match class_for(e) {
                Some(s) => spans.extend(s),
                None => spans.push((e, e)),
            }
            continue;
        }
        // a - before the ] is a literal one, which is how [\w=-] is written
        if cs.get(*i) == Some(&'-') && cs.get(*i + 1).is_some_and(|n| *n != ']') {
            let hi = cs[*i + 1];
            *i += 2;
            spans.push((c, hi));
        } else {
            spans.push((c, c));
        }
    }
}

fn pieces(cs: &[char], i: &mut usize, groups: &mut usize, top: bool) -> Result<Vec<Piece>, String> {
    let mut out: Vec<Piece> = Vec::new();
    while let Some(&c) = cs.get(*i) {
        if !top && (c == '|' || c == ')') {
            break;
        }
        *i += 1;
        let node = match c {
            '.' => Node::Any,
            '[' => brackets(cs, i)?,
            '\\' => {
                let Some(&e) = cs.get(*i) else {
                    return Err("\\ at the end of the pattern".into());
                };
                *i += 1;
                match class_for(e) {
                    Some(spans) => Node::Class { spans, neg: false },
                    None => Node::Lit(e),
                }
            }
            '(' => {
                let n = *groups;
                *groups += 1;
                let mut alts = Vec::new();
                loop {
                    alts.push(pieces(cs, i, groups, false)?);
                    match cs.get(*i) {
                        Some('|') => *i += 1,
                        Some(')') => {
                            *i += 1;
                            break;
                        }
                        _ => return Err("( with no )".into()),
                    }
                }
                Node::Group(n, alts)
            }
            ')' | '|' => return Err(format!("{c} outside a group")),
            '+' | '*' | '?' => return Err(format!("{c} with nothing before it")),
            _ => Node::Lit(c),
        };
        let rep = match cs.get(*i) {
            Some('+') => (1, usize::MAX),
            Some('*') => (0, usize::MAX),
            Some('?') => (0, 1),
            _ => (1, 1),
        };
        if rep != (1, 1) {
            *i += 1;
            if matches!(node, Node::Group(..)) {
                return Err("a group takes no + * or ?".into());
            }
        }
        out.push(Piece { node, rep });
    }
    Ok(out)
}

impl Rx {
    pub fn new(src: &str) -> Result<Rx, String> {
        let cs: Vec<char> = src.chars().collect();
        let (mut i, mut groups) = (0, 0);
        let prog = pieces(&cs, &mut i, &mut groups, true)?;
        Ok(Rx {
            prog,
            groups,
            src: src.to_string(),
        })
    }

    // unanchored, leftmost. the whole line is the haystack and the table's patterns are
    // fragments of it
    pub fn find(&self, hay: &str) -> Option<Vec<String>> {
        let s: Vec<char> = hay.chars().collect();
        for start in 0..=s.len() {
            let mut caps = vec![None; self.groups];
            if seq(&self.prog, &s, start, &mut caps).is_some() {
                return Some(
                    caps.into_iter()
                        .map(|c| match c {
                            Some((a, b)) => s[a..b].iter().collect(),
                            None => String::new(),
                        })
                        .collect(),
                );
            }
        }
        None
    }

    pub fn as_str(&self) -> &str {
        &self.src
    }
}

fn one(n: &Node, s: &[char], i: usize) -> Option<usize> {
    let &c = s.get(i)?;
    let ok = match n {
        Node::Lit(l) => c == *l,
        Node::Any => true,
        Node::Class { spans, neg } => spans.iter().any(|(a, b)| c >= *a && c <= *b) != *neg,
        Node::Group(..) => return None,
    };
    ok.then_some(i + 1)
}

fn seq(ps: &[Piece], s: &[char], i: usize, caps: &mut Vec<Option<(usize, usize)>>) -> Option<usize> {
    let Some((p, rest)) = ps.split_first() else {
        return Some(i);
    };

    if let Node::Group(n, alts) = &p.node {
        for a in alts {
            let Some(mid) = seq(a, s, i, caps) else { continue };
            if let Some(end) = seq(rest, s, mid, caps) {
                caps[*n] = Some((i, mid));
                return Some(end);
            }
        }
        return None;
    }

    // greedy, then give characters back one at a time
    let mut ends = vec![i];
    let mut at = i;
    while ends.len() <= p.rep.1 {
        let Some(next) = one(&p.node, s, at) else { break };
        at = next;
        ends.push(at);
    }
    for k in (p.rep.0..ends.len()).rev() {
        if let Some(e) = seq(rest, s, ends[k], caps) {
            return Some(e);
        }
    }
    None
}

// a build failure has an error message in it, so read the message and fix it in one
// attempt. the ladder below is for a check failure, which has no message at all
//
// first match wins and the rows are most-specific first, so /etc/kiry/failures.d is
// consulted ahead of the table proper
const FAILURES: &str = "\
# regex                                      action                     retry
LLVM ERROR: out of memory.*lto               set LTO thin               clean
(out of memory|signal 9|exit status 137)     set KIRY_THINLTO_JOBS /2   reuse
(Not a valid object file|invalid bitcode)    filter-lto                 clean
unknown argument: '(-[fm][\\w=-]+)'          drop $1                    clean
recompile with -fPIC                         append -fPIC               clean
undefined symbol: __\\w+_chk                  append -U_FORTIFY_SOURCE   clean
error: instruction requires:                 set CFLAGS_MARCH x86-64    clean
undefined reference to `__isoc99_            notaflag musl-portability  clean
PLEASE submit a bug report                   set OPT -O2                clean
";

struct Rule {
    rx: Rx,
    act: String,
    // whether the work dir survives: it depends on whether the fix changes CFLAGS, not
    // on which phase died and not on where the action is written
    reuse: bool,
}

fn rules(root: &Path) -> Result<Vec<Rule>, String> {
    let mut text = String::new();
    // the user's rows first, so a local rule beats the shipped one it narrows
    if let Ok(rd) = fs::read_dir(root.join("etc/kiry/failures.d")) {
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        files.sort();
        for f in files {
            text.push_str(&fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?);
            text.push('\n');
        }
    }
    match fs::read_to_string(root.join("usr/share/kiry/failures")) {
        Ok(t) => text.push_str(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => text.push_str(FAILURES),
        Err(e) => return Err(format!("usr/share/kiry/failures: {e}")),
    }

    let mut out = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        // the pattern has single spaces in it, so only a run of two or a tab ends it
        let mut f = l.split('\t').flat_map(|x| x.split("  ")).map(str::trim).filter(|x| !x.is_empty());
        let (Some(rx), Some(act), reuse) = (f.next(), f.next(), f.next()) else {
            return Err(format!("failures:{}: wants a pattern, an action and a retry", n + 1));
        };
        out.push(Rule {
            rx: Rx::new(rx).map_err(|e| format!("failures:{}: {e}", n + 1))?,
            act: act.to_string(),
            reuse: match reuse {
                Some("reuse") => true,
                Some("clean") | None => false,
                Some(x) => return Err(format!("failures:{}: retry is reuse or clean, not {x}", n + 1)),
            },
        });
    }
    Ok(out)
}

// what died, recorded beside the rule that fired. it decides nothing -- it is the first
// thing wanted when a rule turns out to match the wrong thing
fn phase(log: &str) -> &'static str {
    let mut seen = "compile";
    for l in log.lines() {
        if l.starts_with("checking ") || l.contains("configure:") {
            seen = "configure";
        } else if l.contains("ld.lld:") || l.contains("undefined reference") || l.contains("relocation") {
            seen = "link";
        } else if l.contains("FAILED:")
            || l.contains("Test suite")
            || l.contains("tests failed")
            || l.contains("Testing")
            || l.starts_with("PASS")
        {
            seen = "check";
        }
    }
    seen
}

struct Fix {
    rule: String,
    line: String,
    act: String,
    reuse: bool,
}

fn scan(rules: &[Rule], log: &str) -> Option<Fix> {
    for r in rules {
        for l in log.lines() {
            let Some(caps) = r.rx.find(l) else { continue };
            let mut act = r.act.clone();
            for (i, c) in caps.iter().enumerate() {
                act = act.replace(&format!("${}", i + 1), c);
            }
            return Some(Fix {
                rule: r.rx.as_str().to_string(),
                line: l.trim().to_string(),
                act,
                reuse: r.reuse,
            });
        }
    }
    None
}

// drop, append and filter-lto are the recipe's to carry; set is the machine's opinion
// about one package and belongs beside the other settings
fn write_fix(root: &Path, dir: &Path, name: &str, f: &Fix, log: &str) -> Result<bool, String> {
    let mut w = f.act.split_whitespace();
    let verb = w.next().unwrap_or("");
    let rest: Vec<&str> = w.collect();

    let (at, line) = match verb {
        "drop" => (dir.join("filter"), format!("filter-flags {}", rest.join(" "))),
        "append" => (dir.join("filter"), format!("append-flags {}", rest.join(" "))),
        "filter-lto" => (dir.join("filter"), "filter-lto".to_string()),
        "set" => {
            let [k, v] = rest[..] else {
                return Err(format!("set wants a name and a value, got {:?}", f.act));
            };
            let v = match v.strip_prefix('/') {
                // the thinlto row halves what is there rather than naming a number, so
                // the same rule fires again on a machine with different memory
                Some(d) => {
                    let by: usize = d.parse().map_err(|_| format!("{v} is not a divisor"))?;
                    let now = flags(root, name, Some(dir))?;
                    let was = now
                        .from
                        .iter()
                        .rev()
                        .find(|(_, n, _)| n == k)
                        .and_then(|(_, _, x)| x.parse::<usize>().ok())
                        .unwrap_or(8);
                    (was / by.max(1)).max(1).to_string()
                }
                None => v.to_string(),
            };
            (
                root.join("etc/kiry/pkg").join(name),
                format!("{k} {v}"),
            )
        }
        "notaflag" => return Ok(false),
        _ => return Err(format!("{verb} is not an action")),
    };

    let had = fs::read_to_string(&at).unwrap_or_default();
    // never the same fix twice: a rule that fired and did not help fires again on the
    // next log, and an entry appended each time would grow without bound
    if had.lines().any(|l| l.split('#').next().unwrap_or("").trim() == line) {
        return Ok(false);
    }
    if let Some(d) = at.parent() {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let day = today();
    let body = format!(
        "{had}{}# {day} {} failed, {}\n#   {}\n{line}\n",
        if had.is_empty() || had.ends_with('\n') { "" } else { "\n" },
        phase(log),
        f.rule,
        f.line,
    );
    fs::write(&at, body).map_err(|e| format!("{}: {e}", at.display()))?;
    say!("{} {}", at.display(), line);
    Ok(true)
}

// civil-from-days, for the one date a provenance line needs. a calendar crate to format
// nine characters is the trade thiserror already lost
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", yoe + era * 400 + i64::from(m <= 2), m, d)
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

fn clock(s: u64) -> String {
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn times(root: &Path) -> PathBuf {
    root.join("var/kiry/times")
}

// appended rather than rewritten, so a batch that dies partway through has still
// recorded what it finished. a line per build rather than per package keeps it honest
// about what a package used to cost, and a few thousand of them is a hundred kilobytes
fn keep_time(root: &Path, p: &Package, t: &str, secs: u64) {
    let at = times(root);
    let Some(up) = at.parent() else { return };
    if mkdirs(up).is_err() {
        return;
    }
    let line = format!("{} {} {t} {secs} {}\n", p.name, p.version.upstream, today());
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&at)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

// what each package and target took the last time it was built. later lines win, which
// is what lets the file be appended to
fn history(root: &Path) -> HashMap<(String, String), u64> {
    let mut out = HashMap::new();
    for l in plain(&times(root)) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if let [name, _, target, secs, ..] = f[..] {
            if let Ok(s) = secs.parse() {
                out.insert((name.to_string(), target.to_string()), s);
            }
        }
    }
    out
}

// said before any of it starts, because the whole value of an estimate is deciding
// whether to wait, and that is a decision made at the beginning
fn forecast(root: &Path, todo: &[(String, String)], recipes: &HashMap<String, Package>) {
    let past = history(root);
    let (mut n, mut secs, mut new) = (0, 0u64, 0);
    for (name, t) in todo {
        let Some(p) = recipes.get(name) else { continue };
        if cached(root, p, t).is_some() {
            continue;
        }
        n += 1;
        match past.get(&(name.clone(), t.clone())) {
            Some(s) => secs += s,
            None => new += 1,
        }
    }
    if n < 2 {
        return;
    }
    let note = match new {
        0 => String::new(),
        u => format!("  {u} never built here"),
    };
    say!("{n} builds  ~{}{note}", clock(secs));
}

fn install_cmd(args: &[String]) {
    let mut want = None;
    let mut verbose = false;
    let mut fix = false;
    let mut dry = false;
    let mut live = false;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" => verbose = true,
            "-n" => dry = true,
            "--live" => live = true,
            "--recover" => fix = true,
            "--target" => match it.next() {
                Some(t) => want = Some(t.clone()),
                None => die("--target wants a name".into()),
            },
            _ => rest.push(a.clone()),
        }
    }

    let (root, force, names) = opts(&rest);
    if !dry {
        writes(&root);
    }
    if names.is_empty() {
        die("nothing to install".into());
    }
    let names = expand(&root, names);
    if names.is_empty() {
        return;
    }

    // an archive or a package. told apart by the suffix a built artifact always carries,
    // which no recipe directory is named after
    let (archives, asked): (Vec<String>, Vec<String>) =
        names.into_iter().partition(|n| n.ends_with(".tar.zst"));
    if asked.is_empty() {
        let paths: Vec<PathBuf> = archives.iter().map(PathBuf::from).collect();
        let going: Vec<(String, String)> = paths.iter().filter_map(|a| sidecar(a)).collect();
        let dest = settle(&root, &going, 1, live);
        apply_batch(&dest, &paths, force);
        landed(&root, &dest);
        return;
    }
    if !archives.is_empty() {
        die("install takes archives or package names, not both at once".into());
    }

    let mut recipes: HashMap<String, Package> = HashMap::new();
    let mut seeds: Vec<(String, String)> = Vec::new();
    for n in &asked {
        let at = match resolve(&root, n) {
            Ok(at) => at,
            Err(e) => die(e),
        };
        let p = match pkg::load(&at) {
            Ok(p) => p,
            Err(e) => die(e.to_string()),
        };
        let targets = match &want {
            Some(t) if !p.targets.contains(t) => die(format!("{} does not build for {t}", p.name)),
            Some(t) => vec![t.clone()],
            None => p.targets.clone(),
        };
        for t in targets {
            // the same version already in is the question answered. reinstalling over
            // it is a repair, which is b and the artifact and a different thing to ask
            match db::read(&root, &t, &p.name) {
                Ok(r) if r.version == p.version => {
                    say!("{} {} {t} already installed", p.name, p.version.upstream);
                }
                _ => seeds.push((p.name.clone(), t)),
            }
        }
        recipes.insert(p.name.clone(), p);
    }
    if seeds.is_empty() {
        return;
    }

    let set = wanted(&root, &seeds, &mut recipes);
    let plan = levels(&set, &recipes);
    if dry {
        for (n, t) in &set {
            let how = match cached(&root, &recipes[n], t) {
                Some(_) => "cached",
                None => "builds",
            };
            say!("{n} {} {t} {how}", recipes[n].version.upstream);
        }
        // the same answer the real run would get, override included
        let (ab, why) = route(&root, &set);
        match (ab, live) {
            (true, true) => say!("route live  --live over A/B: {why}"),
            (true, false) => say!("route A/B  {why}"),
            (false, _) => say!("route live  {why}"),
        }
        return;
    }
    // decided once, before any of it is built, so a batch cannot change its mind halfway
    let dest = settle(&root, &set, plan.len(), live);
    forecast(&root, &set, &recipes);

    // a level has to be in place before the one above it can build against it, which is
    // what forces the install to happen a level at a time. when every member is already
    // built that ordering buys nothing, so the set goes in as one transaction instead
    let ready: Option<Vec<PathBuf>> = set
        .iter()
        .map(|(n, t)| cached(&root, &recipes[n], t))
        .collect();
    if let Some(all) = ready {
        for (n, t) in &set {
            say!("{n} {} {t} cached", recipes[n].version.upstream);
        }
        apply_batch(&dest, &all, force);
        landed(&root, &dest);
        return;
    }

    for level in plan {
        // grouped by package, because every target of one builds before any of it is
        // packed. a musl mesa installed beside a gnu mesa that failed is exactly the
        // drift a multi-target recipe exists to prevent
        let mut order: Vec<&String> = Vec::new();
        for (i, _) in &level {
            let name = &set[*i].0;
            if !order.contains(&name) {
                order.push(name);
            }
        }

        let mut made = Vec::new();
        for name in order {
            let p = &recipes[name];
            let mine: Vec<&(usize, bool)> = level.iter().filter(|(i, _)| set[*i].0 == *name).collect();
            let boot = mine.iter().any(|(_, b)| *b);
            let targets: Vec<String> = mine.iter().map(|(i, _)| set[*i].1.clone()).collect();

            let need: Vec<String> = targets
                .iter()
                .filter(|t| match cached(&root, p, t) {
                    Some(_) => {
                        say!("{} {} {t} cached", p.name, p.version.upstream);
                        false
                    }
                    None => true,
                })
                .cloned()
                .collect();
            if !need.is_empty() {
                let r = match (boot, fix) {
                    (true, _) => build(&root, p, &need, verbose, false, true).map(|_| ()),
                    (false, true) => need.iter().try_for_each(|t| recover(&root, p, t, verbose)),
                    (false, false) => build(&root, p, &need, verbose, false, false).map(|_| ()),
                };
                if let Err(e) = r {
                    die(e);
                }
            }
            for t in &targets {
                match cached(&root, p, t) {
                    Some(a) => made.push(a),
                    None => die(format!("{} {t}: built nothing", p.name)),
                }
            }
        }
        apply_batch(&dest, &made, force);
    }
    landed(&root, &dest);
}

// the resolve every consumer does at load, run against what just landed instead of left
// for the first time something runs it. a gnu binary wanting foo@GLIBC_2.40 against a
// substrate providing 2.38 installs quietly and both sides of that are already indexed
//
// not an exit code. a soname bump leaves its consumers unresolved on purpose and the
// queue is what fixes them, so saying so is the point and failing the install is not
fn skew(root: &Path) {
    let hurt = broken(root);
    if hurt.is_empty() {
        return;
    }
    let queued: HashSet<String> = db::read_queue(root)
        .unwrap_or_default()
        .into_iter()
        .map(|q| q.name)
        .collect();
    for (t, f) in &hurt {
        let note = if queued.contains(&f.pkg) { "  queued" } else { "" };
        say!("{} {t} {}{note}", f.path, f.what);
    }
}

// the mount is transient and nothing should be left holding it. what boots the result is
// not built yet, so the last word is where it went rather than what to do about it
fn landed(root: &Path, dest: &Path) {
    skew(dest);
    if dest == root {
        return;
    }
    let _ = run(Command::new("umount").arg(dest), "umount");
    match arm() {
        Ok((sub, num)) => say!("{sub} boots next  Boot{num}  reboot to try it, kiry commit to keep it"),
        Err(e) => say!("staged in the inactive root, and nothing boots it: {e}"),
    }
}

// everything that has to be present before the named packages can go in. a dep already
// installed stops the walk there: install refuses a job whose dep is missing, so what is
// under an installed package is installed too, and depends carry no version constraint
// so there is nothing further to compare
//
// make deps come along rather than being skipped. a build assembles its closure out of
// the installed database, so a tool that is not installed is one the sandbox has nothing
// to hand the build
fn wanted(
    root: &Path,
    seeds: &[(String, String)],
    recipes: &mut HashMap<String, Package>,
) -> Vec<(String, String)> {
    let host = sandbox::host();
    let mut out = seeds.to_vec();
    let mut seen: HashSet<(String, String)> = seeds.iter().cloned().collect();
    let mut i = 0;
    while i < out.len() {
        let (name, t) = out[i].clone();
        i += 1;
        if !recipes.contains_key(&name) {
            let at = match resolve(root, &name) {
                Ok(at) => at,
                Err(e) => die(e),
            };
            match pkg::load(&at) {
                Ok(p) => recipes.insert(name.clone(), p),
                Err(e) => die(e.to_string()),
            };
        }
        let p = &recipes[&name];
        if !p.targets.contains(&t) {
            die(format!("{name} is wanted for {t} and does not build for it"));
        }
        let next: Vec<(String, String)> = p
            .depends
            .iter()
            .filter(|d| d.applies(&t))
            .map(|d| {
                let dt = if d.host { host.clone() } else { t.clone() };
                (d.name.clone(), dt)
            })
            .collect();
        for d in next {
            if db::read(root, &d.1, &d.0).is_ok() || !seen.insert(d.clone()) {
                continue;
            }
            out.push(d);
        }
    }
    out
}

// the pair of root subvolumes. a transaction that cannot go live goes to whichever of
// them is not mounted at /, as a fresh snapshot of the running one rather than a tree
// maintained beside it: /etc, the service definitions and every config file come along by
// copy on write and cost nothing, where two independent roots would mean an edit to
// /etc/kiry/config vanishing the moment you booted the other one
const ROOTS: &[&str] = &["@root-a", "@root-b"];

// on /run, so a mount point left behind by a crash is gone at the next boot
const TOP: &str = "/run/kiry/top";
const DEST: &str = "/run/kiry/root";

// the device and subvolume behind a mount point, read out of the options field
fn mounted_at<'a>(text: &'a str, at: &str) -> Option<(&'a str, &'a str)> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let [dev, on, _fs, opts, ..] = f[..] else {
            return None;
        };
        if on != at {
            return None;
        }
        let sub = opts.split(',').find_map(|o| o.strip_prefix("subvol="))?;
        Some((dev, sub.trim_start_matches('/')))
    })
}

// whether anything is mounted here at all, which mounted_at cannot answer: a mount whose
// subvolume was deleted under it keeps subvolid= and loses subvol= entirely
fn is_mounted(text: &str, at: &str) -> bool {
    text.lines()
        .any(|l| l.split_whitespace().nth(1) == Some(at))
}

// which of the pair is running and which is not, and the device they share
fn pair(text: &str) -> Result<(String, String, String), String> {
    let (dev, now) =
        mounted_at(text, "/").ok_or("nothing in /proc/mounts says which subvolume / is on")?;
    // a root that is not one of the pair has no other half, and guessing one would name
    // a subvolume that means nothing on this machine
    if !ROOTS.contains(&now) {
        return Err(format!(
            "/ is on {now} and the a/b roots are {}",
            ROOTS.join(" and ")
        ));
    }
    let other = ROOTS
        .iter()
        .find(|r| **r != now)
        .ok_or("there is only one root subvolume")?;
    Ok((dev.to_string(), now.to_string(), (*other).to_string()))
}

// a fresh snapshot of the running root, mounted and ready to be installed into. whatever
// the inactive subvolume held is the last cycle's tree and is replaced rather than
// updated, because a transaction applies to what is running now
fn inactive() -> Result<PathBuf, String> {
    let text = fs::read_to_string("/proc/mounts").map_err(|e| format!("/proc/mounts: {e}"))?;
    let (dev, now, other) = pair(&text)?;
    let (top, dest) = (PathBuf::from(TOP), PathBuf::from(DEST));
    mkdirs(&top)?;
    mkdirs(&dest)?;

    // a transaction leaves the inactive root mounted here, so a second one in the same
    // boot would snapshot over a subvolume this still points at. what is left then is a
    // mount of something that no longer exists, which is neither a directory nor a usable
    // mount point, and the mount below fails move_mount with ENOENT
    if is_mounted(&text, DEST) {
        run(Command::new("umount").arg(&dest), "umount the last root")?;
    }

    // the subvolumes sit at the top of the filesystem and nothing mounts that, so it has
    // to be reachable before one of them can be replaced
    run(
        Command::new("mount")
            .args(["-o", "subvolid=5"])
            .arg(&dev)
            .arg(&top),
        "mount subvolid=5",
    )?;
    let made = snapshot(&top, &now, &other);
    let _ = run(Command::new("umount").arg(&top), "umount");
    made?;

    run(
        Command::new("mount")
            .args(["-o", &format!("subvol={other}")])
            .arg(&dev)
            .arg(&dest),
        &format!("mount {other}"),
    )?;
    say!("root {other} is a fresh snapshot of {now}");
    Ok(dest)
}

fn snapshot(top: &Path, now: &str, other: &str) -> Result<(), String> {
    let at = top.join(other);
    if at.exists() {
        run(
            Command::new("btrfs").args(["subvolume", "delete"]).arg(&at),
            &format!("btrfs subvolume delete {other}"),
        )?;
    }
    run(
        Command::new("btrfs")
            .args(["subvolume", "snapshot"])
            .arg(top.join(now))
            .arg(&at),
        &format!("btrfs subvolume snapshot {now} {other}"),
    )
}

// the uefi entry that boots one of the pair. the label names the subvolume and the image
// name carries no kernel version, so a kernel bump rewrites both files and leaves the two
// entries alone -- an entry whose device path has to be rewritten every bump is one that
// is wrong for the hours between the bump and the rewrite
fn label(sub: &str) -> String {
    format!("kiry {sub}")
}

fn efi() -> Result<String, String> {
    let out = Command::new("efibootmgr")
        .output()
        .map_err(|e| format!("efibootmgr: {e}"))?;
    if !out.status.success() {
        return Err(format!("efibootmgr: {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("efibootmgr: {e}"))
}

// BootCurrent, BootOrder and BootNext all start with Boot too, so four hex digits and the
// active marker are what separates an entry line from a header
fn entry(l: &str) -> Option<(&str, &str)> {
    let rest = l.strip_prefix("Boot")?;
    if rest.len() < 5 {
        return None;
    }
    let (num, tail) = rest.split_at(4);
    if !num.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // the device path follows the label after a tab whether or not -v was asked for, so
    // the label is what comes before the first one
    let text = tail.strip_prefix('*').unwrap_or(tail);
    Some((num, text.split('\t').next().unwrap_or(text).trim()))
}

fn entry_for(text: &str, sub: &str) -> Option<String> {
    let want = label(sub);
    text.lines()
        .find_map(|l| entry(l).filter(|(_, n)| *n == want).map(|(n, _)| n.to_string()))
}

fn current(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("BootCurrent:"))
        .map(|n| n.trim().to_string())
}

fn order(text: &str) -> Vec<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("BootOrder:"))
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// first in BootOrder is the whole of what committed means. a marker file beside it could
// disagree with the firmware, and the firmware is the one that decides
fn reorder(num: &str, was: &[String]) -> Vec<String> {
    let mut now = vec![num.to_string()];
    now.extend(was.iter().filter(|n| *n != num).cloned());
    now
}

fn commits(num: &str, was: &[String]) -> Result<(), String> {
    run(
        Command::new("efibootmgr").args(["-o", &reorder(num, was).join(",")]),
        "efibootmgr -o",
    )
}

// the inactive root gets one boot to prove itself. BootNext is one shot in the firmware,
// so a root that never reaches kiry commit is left behind by the boot after it with
// nothing to undo and nothing to time out
fn arm() -> Result<(String, String), String> {
    let text = fs::read_to_string("/proc/mounts").map_err(|e| format!("/proc/mounts: {e}"))?;
    let (_, _, other) = pair(&text)?;
    let seen = efi()?;
    let num = entry_for(&seen, &other).ok_or(format!("no uefi entry named {}", label(&other)))?;
    run(
        Command::new("efibootmgr").args(["-n", &num]),
        "efibootmgr -n",
    )?;
    Ok((other, num))
}

// what says an installed elf cannot run, which is the question a trial boot has to answer
// before it is allowed to become the one that boots. the rest of what doctor reports is
// worth reading and is not worth refusing a commit over: duplicate symbols across two
// libraries that genuinely both export a name is a standing condition, not a regression
fn broken(root: &Path) -> Vec<(String, Finding)> {
    let mut out = Vec::new();
    let world = everywhere(root);
    for t in db::targets(root).unwrap_or_default() {
        for f in check(root, &t, &world) {
            if f.what.breaks() {
                out.push((t.clone(), f));
            }
        }
    }
    out
}

// where the transaction lands, said whichever way it goes
fn settle(root: &Path, going: &[(String, String)], levels: usize, live: bool) -> PathBuf {
    let (ab, why) = route(root, going);
    if !ab {
        say!("route live  {why}");
        say_unkept(root);
        return root.to_path_buf();
    }
    // overruled rather than reconsidered, and the reason is still printed. what is worth
    // having in a log six months later is which answer was set aside, not that one was
    if live {
        say!("route live  --live over A/B: {why}");
        say_unkept(root);
        return root.to_path_buf();
    }
    say!("route A/B  {why}");
    // level N has to be installed before N+1 can build against it, and under A/B the
    // levels land in a root the build cannot see. one level is what this handles, and
    // the rest is refused rather than got quietly wrong
    if levels > 1 {
        die("this needs levels built in order, which the inactive root cannot do yet. install what it depends on first".into());
    }
    match inactive() {
        Ok(d) => d,
        Err(e) => die(e),
    }
}

// name and target out of a built artifact's sidecar, which is where identity lives -- the
// file name is a display name and a version can contain the - that separates its fields
fn sidecar(art: &Path) -> Option<(String, String)> {
    let d = PathBuf::from(format!("{}.meta", art.display()));
    let one = |f: &str| plain(&d.join(f)).into_iter().next();
    Some((one("name")?, one("targets")?))
}

// the kernel and the two libcs. what breaks when one of these is replaced under a running
// system is not the file, which the loader already has open, but the next boot
const SUBSTRATE: &[&str] = &["linux", "musl", "glibc"];

// the root that is running is what a live install writes to, and BootOrder is what
// decides which root comes back. the two are allowed to disagree, and when they do the
// install is gone at the next reboot with nothing having failed and nothing having said
// so. that is how a vulkan-tools bump and a day of database corrections went missing
fn unkept(mounts: &str, seen: &str) -> Option<String> {
    let (_, now, _) = pair(mounts).ok()?;
    let num = entry_for(seen, &now)?;
    match order(seen).first() {
        Some(f) if *f == num => None,
        _ => Some(now),
    }
}

fn say_unkept(root: &Path) {
    if root.canonicalize().as_deref().unwrap_or(root) != Path::new("/") {
        return;
    }
    let (Ok(mounts), Ok(seen)) = (fs::read_to_string("/proc/mounts"), efi()) else {
        return;
    };
    if let Some(now) = unkept(&mounts, &seen) {
        say!("            {now} is running and the firmware boots another root first, so this goes at the next reboot. kiry commit keeps it");
    }
}

// which root a transaction belongs in. routing every install through the inactive
// subvolume is right for a libc and absurd for a new cli tool, and it is decidable rather
// than a question worth asking: kiry knows every file the batch replaces, and /proc says
// which of them something running still has mapped
fn route(root: &Path, going: &[(String, String)]) -> (bool, String) {
    // first, and not one of the reasons below. a/b means snapshotting the subvolume the
    // machine is running from, so it is a property of / and of nothing else -- a staging
    // root asked about a libc must not reach anything that touches the real filesystem
    let at = root.canonicalize();
    if at.as_deref().unwrap_or(root) != Path::new("/") {
        return (false, "not the running root".to_string());
    }
    match why_ab(root, going) {
        Some(w) => (true, w),
        None => (false, nothing_open(root, going)),
    }
}

// what the transaction touches that a live install cannot answer for
fn why_ab(root: &Path, going: &[(String, String)]) -> Option<String> {
    for (name, _) in going {
        if SUBSTRATE.contains(&name.as_str()) {
            return Some(format!("{name} is a libc or the kernel"));
        }
        // core is the base and the toolchain that builds it. a package there is under
        // enough of the system that "nothing has it open" is not the question
        if repo_of(root, name).as_deref() == Some("core") {
            return Some(format!("{name} is in core"));
        }
    }
    let (open, blind) = mapped();
    let mut worst: Option<(usize, String, String)> = None;
    for (name, target) in going {
        // what a job replaces is what the version going out already owns. a package
        // arriving for the first time replaces nothing
        let Ok(old) = db::read(root, target, name) else {
            continue;
        };
        for e in &old.manifest {
            if matches!(e.kind, db::Kind::Dir) {
                continue;
            }
            let Some(n) = open.get(&e.path) else { continue };
            if worst.as_ref().is_none_or(|(w, _, _)| n > w) {
                worst = Some((*n, name.clone(), e.path.clone()));
            }
        }
    }
    let (n, pkg, path) = worst?;
    Some(format!(
        "{pkg} replaces /{path}, mapped by {n} running {}{}",
        if n == 1 { "process" } else { "processes" },
        unsure(blind)
    ))
}

fn nothing_open(_root: &Path, _going: &[(String, String)]) -> String {
    format!("nothing a running process has mapped{}", unsure(mapped().1))
}

// a maps file that would not open is a process this cannot answer for, and saying so
// beats reporting live on the strength of what could not be read
fn unsure(blind: usize) -> String {
    match blind {
        0 => String::new(),
        n => format!(", {n} could not be read"),
    }
}

// which repo holds the recipe for a name, or nothing when no repo does
fn repo_of(root: &Path, name: &str) -> Option<String> {
    let d = recipe(root, name)?;
    let up = d.parent()?;
    Some(up.file_name()?.to_string_lossy().into_owned())
}

// every file a running process has mapped, and how many of them have it, with a count of
// the processes that could not be read. none of the five fields before the path contains
// a slash, so the path is everything from the first one to the end of the line -- which
// is also what keeps a path with a space in it whole
fn mapped() -> (HashMap<String, usize>, usize) {
    let mut out: HashMap<String, usize> = HashMap::new();
    let mut blind = 0;
    let Ok(rd) = fs::read_dir("/proc") else {
        return (out, 1);
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str() else { continue };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let text = match fs::read_to_string(e.path().join("maps")) {
            Ok(t) => t,
            // exited between the readdir and the open, which is not a blind spot
            Err(x) if x.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                blind += 1;
                continue;
            }
        };
        // one process maps a library in several segments and is still one process
        let mut mine: BTreeSet<&str> = BTreeSet::new();
        for l in text.lines() {
            let Some(at) = l.find('/') else { continue };
            // a file that has been deleted is still mapped, and /proc keeps its name
            let path = l[at + 1..].trim_end().trim_end_matches("(deleted)").trim_end();
            if !path.is_empty() {
                mine.insert(path);
            }
        }
        for path in mine {
            *out.entry(path.to_string()).or_insert(0) += 1;
        }
    }
    (out, blind)
}

fn apply_batch(root: &Path, archives: &[PathBuf], force: bool) {
    let jobs = match install::plan(root, archives, force) {
        Ok(j) => j,
        Err(e) => die(e.to_string()),
    };
    let done = match install::apply(root, &jobs) {
        Ok(b) => b,
        Err(e) => die(e.to_string()),
    };

    for j in &jobs {
        say!("{} {} {} ok", j.name, j.version.upstream, j.target);
    }
    for k in &done.edits {
        say!(
            "kept /{}, edited since it was installed. the package's is in {}",
            k.path,
            k.from.display()
        );
    }
    enqueue(root, &done.broke, &named(&jobs));
    hooks(root, jobs.iter().map(|j| j.target.clone()).collect());
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

// in precedence order, which is the whole reason this is a list and not a readdir:
// local overrides everything and testing loses to everything
const REPOS: &[&str] = &["local", "core", "extra", "testing"];
const REPOS_AT: &str = "var/db/kiry";

// the file is the adjustable form and the default is the predetermined one, so a root
// that has configured nothing still finds its recipes. /var/db rather than /var/kiry
// because a recipe is written by hand and /var/kiry is the part you may delete
fn repos(root: &Path) -> Vec<PathBuf> {
    if let Ok(text) = fs::read_to_string(root.join("etc/kiry/repos")) {
        let out: Vec<PathBuf> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(PathBuf::from)
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    let at = root.join(REPOS_AT);
    REPOS.iter().map(|r| at.join(r)).collect()
}

// a set stands for a list of names the tree can work out for itself. @world is every
// installed package and @outdated the ones a recipe has since moved past, which is the
// upgrade: i already plans a batch in dependency order and refuses it whole, so naming
// the set is all an upgrade needs to be
fn expand(root: &Path, names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let add = match n.as_str() {
            "@world" => world(root).unwrap_or_else(|e| die(e)),
            "@outdated" => {
                let moved = outdated(root).unwrap_or_else(|e| die(e));
                let w = moved.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
                let v = moved.iter().map(|(_, is, _)| is.len()).max().unwrap_or(0);
                for (n, is, want) in &moved {
                    say!("{n:w$} {is:v$} -> {want}");
                }
                moved.into_iter().map(|(n, ..)| n).collect()
            }
            _ if n.starts_with('@') => die(format!("no set called {n}")),
            _ => vec![n],
        };
        for a in add {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

// one list with no target on it. which targets a name is built for is the recipe's own
// answer and is decided further in
fn world(root: &Path) -> Result<Vec<String>, String> {
    let mut out = BTreeSet::new();
    for t in db::targets(root).map_err(|e| e.to_string())? {
        out.extend(db::installed(root, &t).map_err(|e| e.to_string())?);
    }
    Ok(out.into_iter().collect())
}

// installed at one version while the tree holds another. not "older": a recipe that went
// backwards is still the version the tree asks for, and the printed line says which way
// it moved rather than leaving it to the word
fn outdated(root: &Path) -> Result<Vec<(String, String, String)>, String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for t in db::targets(root).map_err(|e| e.to_string())? {
        for n in db::installed(root, &t).map_err(|e| e.to_string())? {
            let (Ok(rec), Some(at)) = (db::read(root, &t, &n), recipe(root, &n)) else {
                continue;
            };
            let Ok(p) = pkg::load(&at) else { continue };
            let (is, want) = (rec.version.to_string(), p.version.to_string());
            if is != want && seen.insert(n.clone()) {
                out.push((n, is, want));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn recipe(root: &Path, name: &str) -> Option<PathBuf> {
    repos(root)
        .into_iter()
        .map(|d| d.join(name))
        .find(|d| d.join("build").is_file())
}

// a name, a repo/name, or a path. the path is tried first and only when something is
// actually there, so a staging root or a scratch dir still builds and the two forms
// never have to be told apart by their spelling
fn resolve(root: &Path, arg: &str) -> Result<PathBuf, String> {
    let at = PathBuf::from(arg);
    if at.join("build").is_file() {
        return Ok(at);
    }
    if let Some((repo, name)) = arg.split_once('/') {
        let want = repos(root)
            .into_iter()
            .find(|r| r.file_name().is_some_and(|f| f == repo))
            .map(|r| r.join(name));
        if let Some(d) = want.filter(|d| d.join("build").is_file()) {
            return Ok(d);
        }
    }
    recipe(root, arg).ok_or_else(|| {
        let where_ = repos(root)
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        format!("no recipe {arg} in {where_}")
    })
}

// the bool is a bootstrap pass: the same package built from its bootstrap script, which
// stands in for it until the real one can be built. it stays in the plan afterwards,
// because a first pass is a stand-in and not the article
fn levels(want: &[(String, String)], recipes: &HashMap<String, Package>) -> Vec<Vec<(usize, bool)>> {
    let mut left: Vec<usize> = (0..want.len()).collect();
    let mut done: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    while !left.is_empty() {
        let now: Vec<usize> = left
            .iter()
            .copied()
            .filter(|i| {
                let deps = recipes.get(&want[*i].0).map(|p| &p.depends);
                !left.iter().any(|j| {
                    j != i
                        && !done.contains(j)
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
            // says which member breaks it, and a member only stands in once
            let boot: Vec<usize> = left
                .iter()
                .copied()
                .filter(|i| !done.contains(i))
                .filter(|i| recipes.get(&want[*i].0).is_some_and(|p| p.dir.join("bootstrap").is_file()))
                .collect();
            if boot.is_empty() {
                let names: Vec<&str> = left.iter().map(|i| want[*i].0.as_str()).collect();
                die(format!(
                    "these need each other and none says how to start: {}",
                    names.join(" ")
                ));
            }
            done.extend(&boot);
            out.push(boot.into_iter().map(|i| (i, true)).collect());
            continue;
        }
        left.retain(|i| !now.contains(i));
        out.push(now.into_iter().map(|i| (i, false)).collect());
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
    let world = everywhere(&root);
    for t in &targets {
        for f in check(&root, t, &world) {
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
        for (i, boot) in plan.into_iter().flatten() {
            let how = if boot { "would bootstrap" } else { "would rebuild" };
            say!("{} {} {how}", want[i].0, want[i].1);
        }
        return;
    }

    for level in plan {
        // every member of a level compiles before any of it is installed, and the level
        // below it is already in place, so each one links against what it will run with
        let mut made = Vec::new();
        for (i, boot) in &level {
            let (name, target) = &want[*i];
            let p = &recipes[name];
            // a bootstrap pass is a stand-in that exists to be replaced, so it is built
            // straight rather than put through the recovery loop
            let r = match boot {
                true => build(&root, p, std::slice::from_ref(target), false, false, true).map(|_| ()),
                false => recover(&root, p, target, false),
            };
            if let Err(e) = r {
                die(e);
            }
            match cached(&root, p, target) {
                Some(a) => made.push(a),
                None => die(format!("{} {target}: built nothing", p.name)),
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
        for k in &done.edits {
            say!(
                "kept /{}, edited since it was installed. the package's is in {}",
                k.path,
                k.from.display()
            );
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
    let world = everywhere(&root);
    for t in &targets {
        for f in check(&root, t, &world) {
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
    // a flags row is not a soname. it says the record and the resolve disagree, so it
    // heals the moment they agree again, which is what rebuilding the package does
    if q.soname == "flags" {
        let Ok(rec) = db::read(root, &q.target, &q.name) else {
            return false;
        };
        let dir = recipe(root, &q.name);
        return flags(root, &q.name, dir.as_deref()).is_ok_and(|f| rec.flags == f.record());
    }
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

// what pulls a package in, which is the question asked before removing one. runtime
// edges only: a build dep is gone once the build is
fn why_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let [name] = &rest[..] else {
        die("why wants one package".into());
    };

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut found = false;
    for t in &targets {
        let mut up: HashMap<String, Vec<String>> = HashMap::new();
        let names = db::installed(&root, t).unwrap_or_default();
        for n in &names {
            let Ok(r) = db::read(&root, t, n) else { continue };
            for d in r.depends.iter().filter(|d| !d.make && d.applies(t)) {
                up.entry(d.name.clone()).or_default().push(n.clone());
            }
        }
        if !names.iter().any(|n| n == name) && !up.contains_key(name) {
            continue;
        }
        found = true;

        // shortest path first, so the line printed for a package is the shortest reason
        // it is here rather than every reason
        let mut seen: HashSet<&str> = HashSet::from([name.as_str()]);
        let mut queue: Vec<Vec<&str>> = vec![vec![name.as_str()]];
        let mut out: Vec<String> = Vec::new();
        while let Some(path) = queue.pop() {
            let Some(last) = path.last() else { continue };
            let Some(ups) = up.get(*last) else { continue };
            for u in ups {
                if !seen.insert(u.as_str()) {
                    continue;
                }
                let mut next = path.clone();
                next.push(u.as_str());
                next.reverse();
                out.push(format!("{t} {}", next.join(" ")));
                next.reverse();
                queue.insert(0, next);
            }
        }
        out.sort();
        for l in &out {
            say!("{l}");
        }
        if out.is_empty() {
            say!("{t} {name} nothing depends on it");
        }
    }
    if !found {
        die(format!("{name} is not installed"));
    }
}

// 275 recipes whose versions move by hand, and nothing said which had moved. two sources
// answer that here: the aports clone, which is a file read, and a local/ recipe's tracker,
// which is one fetch. repology was meant to be the third and its domain currently resolves
// to 127.0.0.1, so there is nothing to write against
//
// read only on purpose. what lands as unknown below is what the scheme format has to
// describe, and committing that format to 275 files before seeing the list is how it gets
// written wrong
#[derive(PartialEq)]
enum Part {
    Num(u64),
    Sep,
    Text(String),
}

fn parts(v: &str) -> Vec<Part> {
    let kind = |c: char| {
        if c.is_ascii_digit() {
            0
        } else if ".-_+~:".contains(c) {
            1
        } else {
            2
        }
    };
    let chars: Vec<char> = v.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let k = kind(chars[i]);
        let at = i;
        while i < chars.len() && kind(chars[i]) == k {
            i += 1;
        }
        let s: String = chars[at..i].iter().collect();
        out.push(match (k, s.parse::<u64>()) {
            (0, Ok(n)) => Part::Num(n),
            (1, _) => Part::Sep,
            _ => Part::Text(s),
        });
    }
    out
}

// upstream numbering is not semver, so this orders what it can and answers nothing where
// it cannot. two digit runs compare as numbers and equal text carries on; anything else
// -- rc against a release, a trailing letter, a date where a count was -- is a thing only
// scheme will be able to say, and guessing it would report a bump that is a downgrade
fn compare(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().min(y.len()) {
        match (&x[i], &y[i]) {
            (Part::Num(p), Part::Num(q)) if p != q => return Some(p.cmp(q)),
            (Part::Num(_), Part::Num(_)) | (Part::Sep, Part::Sep) => {}
            (Part::Text(p), Part::Text(q)) if p == q => {}
            _ => return None,
        }
    }

    // 1.2 against 1.2.1 is older and 1.2 against 1.2rc1 is newer, so what is left over
    // decides only while it stays numeric. trailing zeros make it the same version
    let rest = if x.len() > y.len() {
        &x[y.len()..]
    } else {
        &y[x.len()..]
    };
    let mut bigger = false;
    for p in rest {
        match p {
            Part::Num(n) => bigger |= *n > 0,
            Part::Sep => {}
            Part::Text(_) => return None,
        }
    }
    if !bigger {
        Some(Ordering::Equal)
    } else if x.len() > y.len() {
        Some(Ordering::Greater)
    } else {
        Some(Ordering::Less)
    }
}

struct Up {
    version: String,
    from: String,
}

// the first pkgver= line. a value naming a variable needs the shell to resolve and half
// reading it would put a literal $pkgver in the note, so it is left to the next source
fn pkgver(text: &str) -> Option<String> {
    let l = text.lines().find_map(|l| l.trim().strip_prefix("pkgver="))?;
    let v = l.trim().trim_matches(['"', '\'']);
    if v.is_empty() || v.contains('$') || !v.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(v.to_string())
}

// which upstream a recipe tracks, where its own name does not say. one line: none, or
// alpine/<repo>, or alpine/<repo>/<name>. none is the only way a hand written package
// stops being reported as one kiry could not identify
fn pinned(dir: &Path) -> Option<String> {
    plain(&dir.join("pin")).into_iter().next()
}

// the shape every one-value-per-line recipe file has. absent reads as empty, which is
// what optional means for all of them
fn plain(at: &Path) -> Vec<String> {
    fs::read_to_string(at)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

// main before community before testing, which is the order a pin names them in and the
// order aports itself promotes through. a pin narrows it to one repo, and carries the
// name too where alpine spells it differently -- our wlroots is their wlroots0.19 and
// our llvm their llvm20, and neither is derivable from the other
fn from_aports(root: &Path, name: &str, pin: Option<&str>) -> Option<Up> {
    let at = root.join("var/kiry/aports");
    let (repos, name) = match pin.and_then(|p| p.strip_prefix("alpine/")) {
        Some(p) => {
            let mut f = p.split('/');
            let repo = f.next().unwrap_or_default().to_string();
            (vec![repo], f.next().unwrap_or(name).to_string())
        }
        None => (
            ["main", "community", "testing"].map(String::from).to_vec(),
            name.to_string(),
        ),
    };
    for repo in &repos {
        let f = at.join(repo).join(&name).join("APKBUILD");
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        let Some(version) = pkgver(&text) else {
            continue;
        };
        return Some(Up {
            version,
            from: format!("aports/{repo}/{name}"),
        });
    }
    None
}

// a tracker is a url and nothing else, so the body has to be a version on its own. the
// steam one is a debian Packages file and comes back unknown, which is the first thing
// the format will have to grow a way to say
fn from_tracker(dir: &Path) -> Result<Up, &'static str> {
    let text = fs::read_to_string(dir.join("tracker")).map_err(|_| "tracker unreadable")?;
    let url = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or("tracker is empty")?;

    let at = std::env::temp_dir().join(format!("kiry-tracker-{}", std::process::id()));
    let _ = fs::remove_file(&at);
    let mut c = fetcher(url, &at).map_err(|_| "no fetcher")?;
    run(c.stderr(Stdio::null()), url).map_err(|_| "tracker fetch failed")?;
    let body = fs::read_to_string(&at).map_err(|_| "tracker fetch wrote nothing")?;
    let _ = fs::remove_file(&at);

    let mut words = body.split_whitespace();
    let v = words.next().ok_or("tracker answered nothing")?;
    if words.next().is_some() || !v.starts_with(|c: char| c.is_ascii_digit()) {
        return Err("tracker body is not a version on its own");
    }
    Ok(Up {
        version: v.to_string(),
        from: "tracker".to_string(),
    })
}

struct Row {
    name: String,
    ours: String,
    target: String,
    status: &'static str,
    up: Option<Up>,
    // either why there is no upstream version or what is odd about the one there is
    note: String,
}

fn survey(root: &Path, want: &[String], net: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for r in repos(root) {
        let repo = r
            .file_name()
            .map_or_else(String::new, |x| x.to_string_lossy().into_owned());
        let Ok(rd) = fs::read_dir(&r) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let Ok(p) = pkg::load(&d) else { continue };
            if !want.is_empty() && !want.contains(&p.name) {
                continue;
            }

            let pin = pinned(&d);
            let none = pin.as_deref() == Some("none");
            let aport = match none {
                true => None,
                false => from_aports(root, &p.name, pin.as_deref()),
            };
            // a local/ recipe the aports clone now carries is one somebody else maintains
            // for you, and nothing else in the tree would ever say so
            let dropped = repo == "local" && aport.is_some();
            let tracked = d.join("tracker").is_file();

            let mut why = "no source";
            let up = match aport {
                Some(u) => Some(u),
                // a pin is a statement about where to look, so falling back to looking
                // everywhere would answer a question the recipe did not ask. none is the
                // exception: it says alpine is not the place, and a tracker is free to
                // be the place instead
                None if pin.is_some() && !none => {
                    why = "pin names no aport";
                    None
                }
                None if tracked && !net => {
                    why = "tracker not read, pass --net";
                    None
                }
                None if tracked => match from_tracker(&d) {
                    Ok(u) => Some(u),
                    Err(e) => {
                        why = e;
                        None
                    }
                },
                None => None,
            };

            let (status, mut note) = match &up {
                Some(u) => match compare(&p.version.upstream, &u.version) {
                    Some(std::cmp::Ordering::Equal) => ("ok", String::new()),
                    Some(std::cmp::Ordering::Less) => ("behind", String::new()),
                    Some(std::cmp::Ordering::Greater) => ("ahead", String::new()),
                    None => ("unknown", "unordered".to_string()),
                },
                // said out loud rather than not known. these never move, and a report
                // that repeats the same non-answer every run stops being read
                None if none && !tracked => ("untracked", String::new()),
                None => ("unknown", why.to_string()),
            };
            if dropped {
                note = format!("{note} now in aports");
            }
            rows.push(Row {
                name: p.name.clone(),
                ours: p.version.upstream.clone(),
                target: match &p.targets[..] {
                    [one] => one.clone(),
                    _ => "-".to_string(),
                },
                status,
                up,
                note: note.trim().to_string(),
            });
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

// the columns are the standard order and the time is not one this command measures
fn ahead_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let net = rest.iter().any(|a| a == "--net");
    let want = asked(rest, &["--net"]);

    let rows = survey(&root, &want, net);
    let w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let v = rows.iter().map(|r| r.ours.len()).max().unwrap_or(0);
    let t = rows.iter().map(|r| r.target.len()).max().unwrap_or(0);
    for r in &rows {
        let note = match &r.up {
            Some(u) => format!("{} {} {}", u.version, u.from, r.note),
            None => r.note.clone(),
        };
        say!(
            "{:w$} {:v$} {:t$} {:8} {:5} {}",
            r.name,
            r.ours,
            r.target,
            r.status,
            "-",
            note.trim()
        );
    }

    let count = |s: &str| rows.iter().filter(|r| r.status == s).count();
    say!(
        "{} recipes  {} behind  {} ahead  {} ok  {} untracked  {} unknown",
        rows.len(),
        count("behind"),
        count("ahead"),
        count("ok"),
        count("untracked"),
        count("unknown")
    );
}

// what a bump does. -n names the APKBUILD each one would be re-converted from and the
// entry it would land in; without it the conversion happens, into testing/, where it
// sits until promote moves it across
fn sync_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let dry = rest.iter().any(|a| a == "-n");
    let net = rest.iter().any(|a| a == "--net");
    let want = asked(rest, &["-n", "--net"]);
    if !dry {
        writes(&root);
    }
    let list = repos(&root);
    let into = named_repo(&list, "testing");

    let all = survey(&root, &want, net);
    let rows: Vec<&Row> = all.iter().filter(|r| r.status == "behind").collect();
    let w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let v = rows.iter().map(|r| r.ours.len()).max().unwrap_or(0);
    let u = rows
        .iter()
        .filter_map(|r| r.up.as_ref().map(|u| u.version.len()))
        .max()
        .unwrap_or(0);

    let alias = convert::aliases(&list);
    if !dry {
        match seed(&root, &all, &alias, &list) {
            0 => {}
            n => say!("{n} measured against what alpine has now"),
        }
    }
    let mut failed = 0;
    let mut holds = 0;
    for r in &rows {
        let Some(up) = &r.up else { continue };
        let at = match up.from.strip_prefix("aports/") {
            Some(rel) => root.join("var/kiry/aports").join(rel).join("APKBUILD"),
            None => PathBuf::from(&up.from),
        };
        let fresh = into.join(&r.name);
        if let Some(why) = held(&root, &r.name, &fresh, &up.version) {
            say!(
                "{:w$} {:v$} -> {:u$}  held",
                r.name,
                r.ours,
                up.version
            );
            match why.is_empty() {
                true => say!("  hold {}", up.version),
                false => say!("  hold {}  {why}", up.version),
            }
            holds += 1;
            continue;
        }

        if dry {
            say!(
                "{:w$} {:v$} -> {:u$}  {}",
                r.name,
                r.ours,
                up.version,
                fresh.display()
            );
            say!("  {}", at.display());
            continue;
        }

        if let Some(rel) = up.from.strip_prefix("aports/") {
            if let Err(e) = materialise(&root.join("var/kiry/aports"), rel) {
                say!("  {e}");
            }
        }

        let mut notes = match convert::recipe(&at, &into, true, &alias, &list) {
            Ok(rep) => rep.notes,
            Err(e) => {
                say!("{} {} -> {}  failed {e}", r.name, r.ours, up.version);
                failed += 1;
                continue;
            }
        };
        // carry rewrites this in place, and it is what the next bump gets measured
        // against, so it is read before anything touches it
        let generated = fs::read_to_string(fresh.join("build")).unwrap_or_default();
        let conv = converted(&root, &r.name);
        let mut outcome = format!("held      {}", fresh.display());
        if let Err(e) = mkdirs(&conv)
            .and_then(|()| fs::write(conv.join("pending"), &generated).map_err(|e| e.to_string()))
        {
            notes.push(format!("nothing recorded to measure the next bump against {e}"));
        }

        // the recipe being replaced, which is whichever repo already holds that name
        // a bump of something only testing/ has is its own predecessor and carries
        // nothing, since there is nothing older to carry from
        match recipe(&root, &r.name).filter(|d| *d != fresh) {
            None => notes.push("new, nothing to carry".into()),
            Some(old) => {
                let base = conv.join("build");
                match carry(&old, &fresh, Some(base.as_path()).filter(|b| b.is_file())) {
                    Err(e) => {
                        notes.push(format!("carried nothing {e}"));
                        failed += 1;
                    }
                    Ok(b) => {
                        if !b.carried.is_empty() {
                            notes.push(format!("carried {}", b.carried.join(" ")));
                        }
                        for k in &b.kept {
                            notes.push(format!("kept {k} in sources, alpine no longer lists it"));
                        }
                        for f in &b.differ {
                            match f.as_str() {
                                "build" if b.carried.iter().any(|c| c == "build") => notes
                                    .push(format!(
                                        "build is this tree's, alpine's is at {}",
                                        conv.join("pending").display()
                                    )),
                                _ => notes
                                    .push(format!("{f} differs from {}", old.join(f).display())),
                            }
                        }
                        // the note above says where alpine's build is. this says what
                        // is in it, which is the part that predicts a broken build: a
                        // recipe kept from before alpine moved to a git archive calls
                        // configure on a tree that has none
                        if !b.missing.is_empty() {
                            notes.push("alpine's prepare does what ours does not".into());
                            notes.extend(b.missing.iter().map(|l| format!("  {l}")));
                        }
                        notes.extend(b.moved);
                        if !b.upstream.is_empty() {
                            notes.push("alpine changed build()".into());
                            notes.extend(b.upstream.iter().map(|l| format!("  {l}")));
                        }
                        // nothing alpine wrote moved and nothing else needs a decision,
                        // so there is no reading to do and no reason to make you do it
                        if b.settled && b.differ.is_empty() {
                            match promote_one(&root, &list, &r.name) {
                                Ok(to) => outcome = format!("promoted  {}", to.display()),
                                Err(e) => notes.push(format!("not promoted {e}")),
                            }
                        }
                    }
                }
            }
        }
        say!(
            "{:w$} {:v$} -> {:u$}  {}",
            r.name,
            r.ours,
            up.version,
            outcome
        );
        for n in notes {
            say!("  {n}");
        }
    }

    let moving = rows.len() - holds;
    match (dry, holds) {
        (true, 0) => say!("{moving} would be re-converted"),
        (true, _) => say!("{moving} would be re-converted {holds} held"),
        (false, 0) => say!("{} bumped {failed} failed", moving - failed),
        (false, _) => say!("{} bumped {failed} failed {holds} held", moving - failed),
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

// the aports clone is blobless and checks out apkbuilds alone, so every patch and config
// a source line names is missing until something asks for it. one package at a time
// rather than the whole tree: the feed stays a feed, and a conversion still gets the
// files its recipe is about to name
fn materialise(aports: &Path, rel: &str) -> Result<(), String> {
    if !aports.join(rel).join("APKBUILD").is_file() {
        return Ok(());
    }
    // sync runs as root and the clone is not root's, which git refuses to touch by
    // default. the path is the one kiry was going to read either way
    let mut safe = std::ffi::OsString::from("safe.directory=");
    safe.push(aports);
    run(
        Command::new("git")
            .arg("-c")
            .arg(safe)
            .arg("-C")
            .arg(aports)
            .args(["sparse-checkout", "add", rel])
            .stdout(Stdio::null()),
        "git sparse-checkout",
    )
}

// the four assignments a version bump moves. one line each at the top of a build, which
// is the shape convert writes and the shape every recipe here inherited from it
const HEADER: [&str; 4] = ["pkgver=", "pkgrel=", "source=", "builddir="];

fn heading(line: &str) -> Option<&'static str> {
    HEADER.iter().copied().find(|h| line.starts_with(h))
}

// a build with the version assignments reduced to their names, which is what two
// versions of one recipe have in common. blank lines go too, since a bump that moves a
// paragraph break has not moved a decision
fn undated(text: &str) -> Vec<&str> {
    text.lines()
        .map(|l| heading(l).unwrap_or(l))
        .filter(|l| !l.trim().is_empty())
        .collect()
}

// the new version's assignments written into the build this tree already has, so a bump
// keeps every decision the recipe accumulated rather than proposing them all for
// deletion. a build carrying none of them is hand-written and comes across untouched,
// which is the whole of what it needs: there is no version inside it to move. none only
// when ours names an assignment the conversion does not, since there is nothing to
// write there
fn reheaded(ours: &str, fresh: &str) -> Option<String> {
    let mut out = String::new();
    for l in ours.lines() {
        match heading(l) {
            None => out.push_str(l),
            Some(h) => out.push_str(fresh.lines().find(|f| f.starts_with(h))?),
        }
        out.push('\n');
    }
    Some(out)
}

// what alpine moved between the last conversion and this one. lines rather than hunks:
// the question a bump has to answer is which decision changed, and a diff that walks
// position would call a reordered configure list a change
fn moved_upstream(was: &str, is: &str) -> Vec<String> {
    let (a, b) = (undated(was), undated(is));
    let mut out: Vec<String> = b
        .iter()
        .filter(|l| !a.contains(l))
        .map(|l| format!("+ {}", l.trim()))
        .collect();
    out.extend(
        a.iter()
            .filter(|l| !b.contains(l))
            .map(|l| format!("- {}", l.trim())),
    );
    out
}

// prepare runs against whatever sources fetched, so the two move together and carry
// splits them: sources comes from the conversion and build stays this tree's. alpine
// swapping a release tarball for a git archive puts the bootstrap step in their prepare
// and leaves ours calling configure on a tree that has none. libfm, xz and libtool all
// went that way
fn prepared(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        if l.starts_with("prepare()") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if l.starts_with('}') {
            break;
        }
        let t = l.trim();
        // a comment is alpine's reasoning about their own tree, and default_prepare
        // heads every converted prepare there is. neither is a step this one is missing
        if t.is_empty() || t.starts_with('#') || t == "default_prepare" {
            continue;
        }
        out.push(t);
    }
    out
}

// what alpine's prepare does that ours does not. one direction only, for the same reason
// depends is: ours drops their test-suite surgery and bundled-library removal on purpose,
// and reading that back as a finding is a page of saying nothing
fn unprepared(ours: &str, fresh: &str) -> Vec<String> {
    let mine = prepared(ours);
    prepared(fresh)
        .into_iter()
        .filter(|l| !mine.contains(l))
        .map(|l| format!("+ {l}"))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod bumps {
    use super::*;

    const ALPINE: &str = "\
pkgver=1.4.0
source=\"https://github.com/lxde/libfm/archive/$pkgver.tar.gz\"

prepare() {
\tdefault_prepare
\t# needs regenerating, the archive is a git tag
\t./autogen.sh
}

build() {
\t./configure --prefix=/usr
\tmake
}
";

    // the shape that shipped a broken libfm: ours predates alpine moving to a git
    // archive, so it configures a tree that has no configure in it
    const OURS: &str = "\
pkgver=1.3.2
source=\"https://downloads.sourceforge.net/libfm/libfm-$pkgver.tar.xz\"

build() {
\t./configure --prefix=/usr --disable-old-actions
\tmake
}
";

    #[test]
    fn a_prepare_step_alpine_has_and_ours_lacks_is_named() {
        assert_eq!(unprepared(OURS, ALPINE), vec!["+ ./autogen.sh"]);
    }

    // ours drops their test surgery and unbundling deliberately, and every converted
    // prepare opens with default_prepare. neither is this recipe missing something
    #[test]
    fn what_ours_does_alone_is_not_a_finding() {
        let mine = "prepare() {\n\tdefault_prepare\n\t./autogen.sh\n\trm -rf tests\n}\n";
        let theirs = "prepare() {\n\tdefault_prepare\n\t./autogen.sh\n}\n";
        assert!(unprepared(mine, theirs).is_empty());
    }

    // a build with no prepare at all is the common case and answers the question with
    // everything alpine's does, not with a parse that runs off the end
    #[test]
    fn a_build_with_no_prepare_reads_as_doing_none_of_it() {
        assert!(prepared("build() {\n\tmake\n}\n").is_empty());
        assert_eq!(prepared(ALPINE), vec!["./autogen.sh"]);
    }

    // the body stops at its own closing brace. without that, build() below it reads as
    // more of prepare and every configure line becomes a missing step
    #[test]
    fn the_body_ends_at_the_brace_and_does_not_run_into_build() {
        assert!(!prepared(ALPINE).iter().any(|l| l.contains("configure")));
    }
}

// a bump already decided about, and the version it was decided against. scoped to that
// version because a reason recorded for 1.98.1 is not evidence about 1.99, and because a
// hold that never lapses is one nobody remembers setting. the line still prints: an
// answer you cannot see from the output is how a held recipe gets promoted unread
fn held(root: &Path, name: &str, at: &Path, version: &str) -> Option<String> {
    let text = fs::read_to_string(recipe(root, name).filter(|d| d != at)?.join("hold")).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != version {
        return None;
    }
    Some(
        lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

// the generated build from the last time this package was converted. a bump has to say
// whether alpine moved, and the recipe in the tree cannot answer that: it holds this
// tree's own build, so diffing against it re-asks every decision ever made here
fn converted(root: &Path, name: &str) -> PathBuf {
    root.join("var/kiry/converted").join(name)
}

// a bump can only say what alpine changed if there is a conversion to measure it
// against, and nothing has one yet. every package already sitting at alpine's version
// has one available for the asking, so it is taken now rather than making the first bump
// of each one re-ask every decision this tree ever made
fn seed(root: &Path, rows: &[Row], alias: &HashMap<String, String>, list: &[PathBuf]) -> usize {
    // survey walks every repo, so a bump already waiting in testing/ turns up as a
    // second row for the same name at alpine's version. seeding from that one measures
    // the bump against itself, finds nothing moved, and promotes it unread
    let pending: BTreeSet<&str> = rows
        .iter()
        .filter(|r| r.status == "behind")
        .map(|r| r.name.as_str())
        .collect();
    let scratch = root.join("var/kiry/converted/.seed");
    let mut done = 0;
    for r in rows {
        let conv = converted(root, &r.name);
        if r.status != "ok" || pending.contains(r.name.as_str()) || conv.join("build").is_file() {
            continue;
        }
        let Some(rel) = r.up.as_ref().and_then(|u| u.from.strip_prefix("aports/")) else {
            continue;
        };
        let at = root.join("var/kiry/aports").join(rel).join("APKBUILD");
        if !at.is_file() {
            continue;
        }
        let _ = fs::remove_dir_all(&scratch);
        // no fetch: the only part kept is the build, and what a checksum decides is
        // which source entries survive, which is a line the comparison masks anyway
        let Ok(rep) = convert::recipe(&at, &scratch, false, alias, list) else {
            continue;
        };
        if mkdirs(&conv).is_ok()
            && fs::copy(scratch.join(&rep.name).join("build"), conv.join("build")).is_ok()
        {
            done += 1;
        }
    }
    let _ = fs::remove_dir_all(&scratch);
    done
}

// a source entry is either a url alpine owns or a file sitting beside the recipe, and
// only the first kind moves with a version. a patch or a service file this tree added is
// named here and nowhere else, so a conversion writing sources fresh is what drops it --
// openntpd's nitro-run went that way and took its package step with it. the build's own
// $source is rewritten to match, because default_prepare walks that and not the file
fn keep_local(old: &Path, fresh: &Path) -> Result<Vec<String>, String> {
    let rows = |d: &Path| -> Result<Vec<(String, String)>, String> {
        let read = |n: &str| -> Result<Vec<String>, String> {
            Ok(fs::read_to_string(d.join(n))
                .map_err(|e| format!("{n}: {e}"))?
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect())
        };
        let (s, c) = (read("sources")?, read("checksums")?);
        // they pair by position, so an entry that brought only one of its two lines
        // would check every entry after it against the wrong hash
        match s.len() == c.len() {
            true => Ok(s.into_iter().zip(c).collect()),
            false => Err(format!("{}: sources and checksums are different lengths", d.display())),
        }
    };
    for d in [old, fresh] {
        if !d.join("sources").is_file() || !d.join("checksums").is_file() {
            return Ok(Vec::new());
        }
    }
    let (was, mut now) = (rows(old)?, rows(fresh)?);
    let mut kept = Vec::new();
    for (s, c) in was {
        if s.contains("://") || now.iter().any(|(n, _)| *n == s) {
            continue;
        }
        kept.push(s.clone());
        now.push((s, c));
    }
    if kept.is_empty() {
        return Ok(kept);
    }
    let column = |pick: fn(&(String, String)) -> &String| -> String {
        now.iter().map(|r| pick(r).clone() + "\n").collect()
    };
    fs::write(fresh.join("sources"), column(|r| &r.0)).map_err(|e| format!("sources: {e}"))?;
    fs::write(fresh.join("checksums"), column(|r| &r.1)).map_err(|e| format!("checksums: {e}"))?;
    resource(&fresh.join("build"), &now)?;
    Ok(kept)
}

// the source assignment inside a converted build, rewritten to name what sources now
// holds. a hand-written build carries no such line and wants none
fn resource(build: &Path, now: &[(String, String)]) -> Result<(), String> {
    let Ok(text) = fs::read_to_string(build) else {
        return Ok(());
    };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let at: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("source="))
        .map(|(i, _)| i)
        .collect();
    // convert writes the whole list on one line. anything else is a shape this cannot
    // rewrite without guessing where the assignment ends
    if at.len() != 1 || !lines[at[0]].ends_with('"') {
        return Ok(());
    }
    let one: Vec<&str> = now.iter().map(|(s, _)| s.as_str()).collect();
    lines[at[0]] = format!("source=\"{}\"", one.join(" "));
    fs::write(build, lines.join("\n") + "\n").map_err(|e| format!("build: {e}"))
}

// what a bump did, and whether any of it wants reading
struct Bump {
    carried: Vec<String>,
    kept: Vec<String>,
    differ: Vec<String>,
    moved: Vec<String>,
    upstream: Vec<String>,
    missing: Vec<String>,
    settled: bool,
}

// convert writes what an apkbuild says and no more: version, sources, checksums,
// depends, targets, build. everything else in a recipe this tree wrote itself -- a
// filter, a pin, the gentoo entry, a patch -- so it is copied across rather than lost
// targets goes with them, because a conversion always writes x86_64-musl and the recipe
// being bumped is the one that knows
//
// that leaves build and depends. build keeps this tree's own body with the bump's
// assignments written into it, and what alpine changed since the last conversion is
// reported beside it rather than applied. the generated build is right about the version
// and wrong about everything this recipe was corrected for
//
// depends is not the same question. an apkbuild cannot express the distinctions this file
// makes in it -- a runtime edge against a tool, a header set against either, a target a
// line is restricted to -- so alpine's answer is lossy by construction and regenerating
// one throws away every correction the recipe ever accumulated. the recipe's own wins and
// what alpine moved is reported instead
fn carry(old: &Path, fresh: &Path, base: Option<&Path>) -> Result<Bump, String> {
    // before the walk, so the build arm below reheads against a source assignment that
    // already names everything sources holds
    let kept = keep_local(old, fresh)?;
    let rd = fs::read_dir(old).map_err(|e| format!("{}: {e}", old.display()))?;
    let (mut names, mut dirs): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for e in rd.flatten() {
        let Some(n) = e.file_name().to_str().map(String::from) else {
            continue;
        };
        match e.path().is_dir() {
            true => dirs.push(n),
            false => names.push(n),
        }
    }
    names.sort();
    dirs.sort();

    let (mut carried, mut differ, mut moved) = (Vec::new(), Vec::new(), Vec::new());
    let (mut upstream, mut missing, mut settled) = (Vec::new(), Vec::new(), false);
    for n in names {
        let (from, to) = (old.join(&n), fresh.join(&n));
        if !to.is_file() || n == "targets" {
            fs::copy(&from, &to).map_err(|e| format!("{n}: {e}"))?;
            carried.push(n);
            continue;
        }
        if matches!(n.as_str(), "version" | "sources" | "checksums") {
            continue;
        }
        if n == "depends" {
            // one direction only. alpine derives a runtime dep by scanning the built
            // elf and never writes it in an apkbuild, so nearly everything this recipe
            // carries is missing from alpine's list and always was -- reading that as
            // alpine having dropped it is eighty lines of saying nothing
            let (ours, theirs) = (dep_names(&from)?, dep_names(&to)?);
            moved.extend(
                theirs
                    .difference(&ours)
                    .map(|d| format!("alpine now wants {d}")),
            );
            fs::copy(&from, &to).map_err(|e| format!("{n}: {e}"))?;
            carried.push(n);
            continue;
        }
        if n == "build" {
            let ours = fs::read_to_string(&from).map_err(|e| format!("{n}: {e}"))?;
            let made = fs::read_to_string(&to).map_err(|e| format!("{n}: {e}"))?;
            let was = base.and_then(|b| fs::read_to_string(b).ok());
            match reheaded(&ours, &made) {
                // the two disagree about which assignments exist at all, so the bump is
                // a shape change and the conversion stands as written
                None => {
                    if ours != made {
                        differ.push(n);
                    }
                }
                Some(next) => {
                    fs::write(&to, next).map_err(|e| format!("{n}: {e}"))?;
                    carried.push(n.clone());
                    // whether there is a baseline or not: the build being kept is ours,
                    // and what it does not do is the same question either way
                    missing = unprepared(&ours, &made);
                    match was {
                        // no conversion to measure against, so whether alpine moved is
                        // not answerable yet and this one wants reading. what is waiting
                        // is still this tree's build: promoting it can fail a build,
                        // where promoting alpine's throws the recipe away and says
                        // nothing
                        None => {
                            if undated(&ours) != undated(&made) {
                                differ.push(n);
                            }
                        }
                        Some(was) => {
                            upstream = moved_upstream(&was, &made);
                            settled = upstream.is_empty();
                        }
                    }
                }
            }
            continue;
        }
        match (fs::read(&from), fs::read(&to)) {
            (Ok(a), Ok(b)) if a == b => {}
            _ => differ.push(n),
        }
    }
    // a recipe's own files sit in a subdirectory and a conversion never writes one, so
    // without this the promotion is what deletes them. openntpd's nitro-run went that way
    for d in dirs {
        if fresh.join(&d).exists() {
            continue;
        }
        tree_copy(&old.join(&d), &fresh.join(&d))?;
        carried.push(d);
    }
    Ok(Bump { carried, kept, differ, moved, upstream, missing, settled })
}

fn tree_copy(from: &Path, to: &Path) -> Result<(), String> {
    mkdirs(to)?;
    let rd = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for e in rd.flatten() {
        let (a, b) = (e.path(), to.join(e.file_name()));
        match a.is_dir() {
            true => tree_copy(&a, &b)?,
            false => {
                fs::copy(&a, &b).map_err(|e| format!("{}: {e}", a.display()))?;
            }
        }
    }
    Ok(())
}

// the name a depends line starts with, which is the part two versions of a recipe can be
// compared on. what follows it is the distinction alpine does not carry
fn dep_names(p: &Path) -> Result<BTreeSet<String>, String> {
    let text = fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| !w.starts_with('#'))
        .map(String::from)
        .collect())
}

fn named_repo(list: &[PathBuf], want: &str) -> PathBuf {
    match list.iter().find(|r| r.file_name().is_some_and(|f| f == want)) {
        Some(d) => d.clone(),
        None => die(format!("no {want} repo in the repo list")),
    }
}

// testing/ to wherever the tree already keeps that recipe, or extra/ when the name is
// new. the news gate the design asks for has nothing to read yet -- news is not built --
// so what is enforced here is the group rule, a set that moves together not moving in
// halves
fn promote_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    writes(&root);
    if rest.is_empty() {
        die("promote wants a package".into());
    }
    let list = repos(&root);
    for n in &rest {
        match promote_one(&root, &list, n) {
            Ok(to) => say!("{n} {}", to.display()),
            Err(e) => die(e),
        }
    }
}

fn promote_one(root: &Path, list: &[PathBuf], n: &str) -> Result<PathBuf, String> {
    let testing = named_repo(list, "testing");
    let from = testing.join(n);
    if !from.join("build").is_file() {
        return Err(format!("nothing to promote at {}", from.display()));
    }
    for m in plain(&from.join("group")) {
        if m != n && !testing.join(&m).join("build").is_file() {
            return Err(format!("{n} moves with {m} and {m} is not in testing"));
        }
    }
    let to = recipe(root, n)
        .filter(|d| *d != from)
        .unwrap_or_else(|| named_repo(list, "extra").join(n));
    if let Some(up) = to.parent() {
        mkdirs(up)?;
    }
    if to.exists() {
        fs::remove_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    fs::rename(&from, &to).map_err(|e| format!("{} to {}: {e}", from.display(), to.display()))?;
    // the conversion this promotion accepted is what the next bump gets measured
    // against, so a decision taken here is not asked about a second time
    let conv = converted(root, n);
    let _ = fs::rename(conv.join("pending"), conv.join("build"));
    Ok(to)
}

// the two halves of the a/b pair, said from the machine they belong to. --root names a
// tree and a boot entry is not in one, so anything but / here is a question the firmware
// cannot be asked
fn roots(args: &[String], what: &str) -> Result<(String, String), String> {
    let (root, _, rest) = opts(args);
    if let Some(a) = rest.first() {
        die(format!("{what} takes no arguments, got {a}"));
    }
    let at = root.canonicalize();
    if at.as_deref().unwrap_or(&root) != Path::new("/") {
        die(format!("{what} is about this machine's own roots, and --root names a tree"));
    }
    let text = fs::read_to_string("/proc/mounts").map_err(|e| format!("/proc/mounts: {e}"))?;
    let (_, now, other) = pair(&text)?;
    Ok((now, other))
}

fn commit_cmd(args: &[String]) {
    let (now, _) = match roots(args, "commit") {
        Ok(r) => r,
        Err(e) => die(e),
    };
    let seen = match efi() {
        Ok(t) => t,
        Err(e) => die(e),
    };
    let Some(num) = entry_for(&seen, &now) else {
        die(format!("no uefi entry named {}", label(&now)));
    };
    // the entry that was booted, not the one that matches the subvolume. they differ when
    // some third entry reaches the same root, and an entry that has not been through a
    // boot is one nothing has proved
    match current(&seen) {
        Some(c) if c == num => {}
        Some(c) => die(format!(
            "this boot came from Boot{c} and {now} is Boot{num}, so there is nothing here to keep"
        )),
        None => die("efibootmgr says nothing about BootCurrent".into()),
    }
    let was = order(&seen);
    if was.first() == Some(&num) {
        say!("{now} is already what boots");
        return;
    }

    // the whole point of the gate. a root that will not boot is caught by never reaching
    // here at all; this is for the one that boots and is wrong
    let bad = broken(Path::new("/"));
    if !bad.is_empty() {
        for (t, f) in &bad {
            say!("{} {t} {}", f.path, f.what);
        }
        die(format!("{} of those, so {now} stays a trial", bad.len()));
    }
    if let Err(e) = commits(&num, &was) {
        die(e);
    }
    say!("{now} boots from now on  Boot{num}");
}

fn rollback_cmd(args: &[String]) {
    let (now, other) = match roots(args, "rollback") {
        Ok(r) => r,
        Err(e) => die(e),
    };
    let seen = match efi() {
        Ok(t) => t,
        Err(e) => die(e),
    };
    let Some(num) = entry_for(&seen, &other) else {
        die(format!("no uefi entry named {}", label(&other)));
    };
    // no gate going this way. back to the root that was booting before is the direction
    // that needs no permission, and the tree being left is the one under suspicion
    if let Err(e) = commits(&num, &order(&seen)) {
        die(e);
    }
    say!("{other} boots from now on  reboot to leave {now}");
}

// the recipes on offer, which nothing else answers: l lists what is installed and a
// package that is not cannot be found at all otherwise
fn search_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let want = rest.first().map(String::as_str).unwrap_or("");

    let mut have: HashSet<String> = HashSet::new();
    for t in db::targets(&root).unwrap_or_default() {
        have.extend(db::installed(&root, &t).unwrap_or_default());
    }

    let mut out: Vec<String> = Vec::new();
    for r in repos(&root) {
        let repo = r.file_name().map_or_else(String::new, |x| x.to_string_lossy().into_owned());
        let Ok(rd) = fs::read_dir(&r) else { continue };
        for e in rd.flatten() {
            let d = e.path();
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.contains(want) || !d.join("build").is_file() {
                continue;
            }
            let v = fs::read_to_string(d.join("version")).unwrap_or_default();
            let v = v.split_whitespace().next().unwrap_or("?").to_string();
            let mark = if have.contains(&n) { "installed" } else { "-" };
            out.push(format!("{n} {v} {repo} {mark}"));
        }
    }
    out.sort();
    for l in &out {
        say!("{l}");
    }
}

// the log of the last build, which is otherwise a path nobody remembers
fn log_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let [name] = &rest[..] else {
        die("log wants one package".into());
    };

    let d = root.join("var/kiry/log");
    let Ok(rd) = fs::read_dir(&d) else {
        die(format!("{}: no logs", d.display()));
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        // name-version-rev.target.log, and a version can hold a dash of its own
        if !f.starts_with(&format!("{name}-")) || !f.ends_with(".log") {
            continue;
        }
        let Ok(when) = e.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| when > *b) {
            best = Some((when, e.path()));
        }
    }
    let Some((_, at)) = best else {
        die(format!("no build log for {name}"));
    };
    match fs::read_to_string(&at) {
        Ok(t) => {
            say!("{}", at.display());
            for l in t.lines() {
                say!("{l}");
            }
        }
        Err(e) => die(format!("{}: {e}", at.display())),
    }
}

fn stats_cmd(args: &[String]) {
    let (root, _, _) = opts(args);

    for t in db::targets(&root).unwrap_or_default() {
        say!("installed {t} {}", db::installed(&root, &t).unwrap_or_default().len());
    }

    let mut recipes = 0;
    for r in repos(&root) {
        let n = fs::read_dir(&r)
            .map(|rd| rd.flatten().filter(|e| e.path().join("build").is_file()).count())
            .unwrap_or(0);
        say!("recipes {} {n}", r.display());
        recipes += n;
    }
    say!("recipes total {recipes}");

    let (mut arts, mut bytes) = (0u64, 0u64);
    for a in listing(&root.join("var/kiry/cache")) {
        if a.extension().is_some_and(|x| x == "zst") {
            arts += 1;
            bytes += weigh(&a);
        }
    }
    say!("cache {arts} artifacts {}", size(bytes));
    // the three that answer where the disk went, which one number over the cache
    // directory did not: the tarballs under it outweigh the artifacts in it
    for (what, at) in [
        ("sources", "var/kiry/cache/sources"),
        ("stage", "var/kiry/stage"),
        ("log", "var/kiry/log"),
    ] {
        say!("{what} {}", size(weigh(&root.join(at))));
    }
    let past = history(&root);
    if !past.is_empty() {
        let mut rows: Vec<(&(String, String), &u64)> = past.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        // the sum of everything's last build, which is what building it all again costs
        say!("world ~{}", clock(rows.iter().map(|(_, s)| **s).sum()));
        for ((n, t), s) in rows.iter().take(3) {
            say!("slowest {n} {t} {}", clock(**s));
        }
    }
    say!("queued {}", db::read_queue(&root).unwrap_or_default().len());
}

// the only thing that deletes anything. everything under /var/kiry is regenerable by
// construction, which is what the invariant claims, so the question here is never
// whether something can be lost -- only which of it is still worth the disk
fn gc_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let dry = rest.iter().any(|a| a == "-n");
    if !dry {
        writes(&root);
    }
    let var = root.join("var/kiry");
    let sweep = |what: &str, doomed: Vec<PathBuf>, skipped: usize| {
        let mut bytes = 0;
        for at in &doomed {
            bytes += weigh(at);
            if dry {
                continue;
            }
            let r = match at.is_dir() {
                true => fs::remove_dir_all(at),
                false => fs::remove_file(at),
            };
            if let Err(e) = r {
                say!("{}: {e}", at.display());
            }
        }
        let verb = if dry { "would free" } else { "freed" };
        let note = match skipped {
            0 => String::new(),
            n => format!("  {n} kept back"),
        };
        say!("{what} {} {verb} {}{note}", doomed.len(), size(bytes));
    };

    // a work directory is removed by the build that finishes with it, so every one still
    // here belongs to a build that failed or was killed -- or to one running right now,
    // which looks identical. kiry takes no lock, so the only thing separating the two is
    // how recently something was written, and llvm takes three hours
    let now = SystemTime::now();
    let (mut stale, mut busy) = (Vec::new(), 0);
    for e in listing(&var.join("stage")) {
        match touched(&e, now).is_some_and(|d| d.as_secs() > 6 * 3600) {
            true => stale.push(e),
            false => busy += 1,
        }
    }
    sweep("stage", stale, busy);

    // what is installed now, the two newest of everything else so a bad bump has
    // something to go back to, and both libcs always -- the repair case is putting a
    // working libc back on a machine with no network
    let mut arts: Vec<(String, String, String, PathBuf, SystemTime)> = Vec::new();
    for a in listing(&var.join("cache")) {
        let Some(n) = a.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !n.ends_with(".tar.zst") {
            continue;
        }
        let meta = PathBuf::from(format!("{}.meta", a.display()));
        let one = |f: &str| plain(&meta.join(f)).into_iter().next();
        // no sidecar is an artifact from before they were written, and nothing can say
        // what it is. left alone rather than guessed at from the file name
        let (Some(name), Some(target), Some(version)) = (one("name"), one("targets"), one("version"))
        else {
            continue;
        };
        let when = fs::metadata(&a)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        arts.push((name, target, version, a, when));
    }

    let mut keep: HashSet<usize> = HashSet::new();
    let mut by: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, a) in arts.iter().enumerate() {
        by.entry((a.0.clone(), a.1.clone())).or_default().push(i);
    }
    for ((name, target), mut idx) in by {
        idx.sort_by_key(|i| std::cmp::Reverse(arts[*i].4));
        // musl, and glibc for the gnu tier
        let pinned = matches!(name.as_str(), "musl" | "glibc");
        let on = db::read(&root, &target, &name).ok().map(|r| r.version.to_string());
        for (n, i) in idx.into_iter().enumerate() {
            if pinned || n < 2 || on.as_deref() == Some(arts[i].2.as_str()) {
                keep.insert(i);
            }
        }
    }
    let mut doomed = Vec::new();
    for (i, a) in arts.iter().enumerate() {
        if keep.contains(&i) {
            continue;
        }
        doomed.push(a.3.clone());
        doomed.push(PathBuf::from(format!("{}.meta", a.3.display())));
    }
    sweep("cache", doomed, keep.len());

    // a tarball no recipe names any more. the recipes are the only thing that decides
    // this, which is why sources is a cache and not a store
    let mut live: HashSet<String> = HashSet::new();
    let mut recipes = 0;
    for r in repos(&root) {
        for d in listing(&r) {
            let Ok(p) = pkg::load(&d) else { continue };
            recipes += 1;
            for line in &p.sources {
                if let Ok((name, _)) = filename(line) {
                    live.insert(name.to_string());
                }
            }
        }
    }
    // with no recipe in reach nothing names a source or a log and every one of them
    // looks dead. that is a mistyped --root, not an empty tree
    if recipes == 0 {
        die("no recipes in reach, so nothing can say which sources are still live".into());
    }
    let gone: Vec<PathBuf> = listing(&var.join("cache/sources"))
        .into_iter()
        .filter(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !live.contains(n))
        })
        .collect();
    sweep("sources", gone, live.len());

    // a log for a version no recipe builds any more. kiry log answers for the current
    // one, and nothing reads the others
    let mut now_building: HashSet<String> = HashSet::new();
    for r in repos(&root) {
        for d in listing(&r) {
            if let Ok(p) = pkg::load(&d) {
                now_building.insert(format!(
                    "{}-{}-{}.",
                    p.name, p.version.upstream, p.version.rev
                ));
            }
        }
    }
    let old: Vec<PathBuf> = listing(&var.join("log"))
        .into_iter()
        .filter(|f| {
            f.file_name().and_then(|n| n.to_str()).is_none_or(|n| {
                !now_building.iter().any(|pre| n.starts_with(pre.as_str()))
            })
        })
        .collect();
    sweep("log", old, recipes);
}

fn listing(at: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(at) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

// how long since anything directly in it changed. a directory's own mtime moves only
// when its entries do, so a build three levels down leaves the top looking untouched --
// the children are what say whether anything is happening
fn touched(at: &Path, now: SystemTime) -> Option<std::time::Duration> {
    let mut newest = fs::metadata(at).and_then(|m| m.modified()).ok()?;
    for e in listing(at) {
        if let Ok(m) = fs::metadata(&e).and_then(|m| m.modified()) {
            newest = newest.max(m);
        }
    }
    now.duration_since(newest).ok()
}

fn weigh(at: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(at) else {
        return 0;
    };
    if !md.is_dir() {
        return md.len();
    }
    listing(at).iter().map(|e| weigh(e)).sum()
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{} MiB", b / (1 << 20)),
        b => format!("{} KiB", b / 1024),
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
    let world = everywhere(&root);
    for t in &targets {
        for f in check(&root, t, &world) {
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
    Undeclared(&'static str, String, String, String),
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
            What::Undeclared(k, who, s, at) => write!(f, "{k} {who} {s} {at}"),
            What::Modified => write!(f, "modified"),
            What::Unowned => write!(f, "unowned"),
        }
    }
}

impl What {
    fn rebuilds(&self) -> bool {
        matches!(self, What::Unresolved(_) | What::MissingSymbol(_))
    }

    // the three that mean a file on disk will not run, as against the ones worth reading
    fn breaks(&self) -> bool {
        matches!(
            self,
            What::Unresolved(_) | What::MissingSymbol(_) | What::NoInterpreter(_)
        )
    }
}

type Scan = Result<(db::Installed, Vec<(String, install::Seen)>), String>;

// reading one package's files says nothing about any other package's, and the box has
// sixteen threads. everything after this is an index, which is order-dependent, so the
// results come back in the order the names were in
fn scans(root: &Path, target: &str, names: &[String]) -> Vec<Scan> {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..jobs().min(names.len()) {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(name) = names.get(i) else { return };
                let one = db::read(root, target, name)
                    .map_err(|e| e.to_string())
                    .and_then(|rec| {
                        install::scan(root, &rec.manifest)
                            .map_err(|e| e.to_string())
                            .map(|seen| (rec, seen))
                    });
                let _ = tx.send((i, one));
            });
        }
    });
    drop(tx);
    let mut out: Vec<Option<Scan>> = (0..names.len()).map(|_| None).collect();
    for (i, one) in rx {
        out[i] = Some(one);
    }
    out.into_iter().flatten().collect()
}

// a package linking a library that nothing in its own depends can account for. it
// resolves today because something else dragged the owner in, and it stops resolving the
// day that something else stops asking -- with nothing written down that says why
//
// build-only is the worse half: the owner is reachable as a tool and not as a runtime
// edge, so this works here and not on a root built from the recipes
fn accounted(
    target: &str,
    deps: &HashMap<String, Vec<Dep>>,
    elves: &[(String, elf::Elf)],
    owners: &[String],
    needs: &[Vec<usize>],
) -> Vec<Finding> {
    let mut edges: HashMap<&str, (HashSet<String>, HashSet<String>)> = HashMap::new();
    let mut uniq: Vec<&str> = owners.iter().map(String::as_str).collect();
    uniq.sort_unstable();
    uniq.dedup();
    for name in uniq {
        let Some(own) = deps.get(name) else {
            continue;
        };
        let mut direct: HashSet<String> = own
            .iter()
            .filter(|d| !d.make && d.applies(target))
            .map(|d| d.name.clone())
            .collect();
        direct.extend(
            sandbox::substrate(target)
                .iter()
                .filter(|(_, make)| !make)
                .map(|(n, _)| (*n).to_string()),
        );
        let mut reach: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = direct.iter().cloned().collect();
        while let Some(n) = queue.pop() {
            if !reach.insert(n.clone()) {
                continue;
            }
            if let Some(r) = deps.get(&n) {
                queue.extend(
                    r.iter()
                        .filter(|d| !d.make && d.applies(target))
                        .map(|d| d.name.clone()),
                );
            }
        }
        edges.insert(name, (direct, reach));
    }

    let mut said: HashSet<(&str, &str, &str)> = HashSet::new();
    let mut out = Vec::new();
    for (i, providers) in needs.iter().enumerate() {
        let me = owners[i].as_str();
        let Some((direct, reach)) = edges.get(me) else {
            continue;
        };
        for &j in providers {
            let who = owners[j].as_str();
            if who == me || direct.contains(who) {
                continue;
            }
            let soname = elves[j].1.soname.as_deref().unwrap_or(&elves[j].0);
            if !said.insert((me, who, soname)) {
                continue;
            }
            let kind = if reach.contains(who) {
                "undeclared"
            } else {
                "build-only"
            };
            // the package, not the file: the fix is a line in its depends, and every
            // other file of it linking the same library is the same missing line
            out.push(Finding {
                pkg: me.to_string(),
                path: me.to_string(),
                what: What::Undeclared(
                    kind,
                    who.to_string(),
                    soname.to_string(),
                    elves[i].0.clone(),
                ),
            });
        }
    }
    out
}

fn check(root: &Path, target: &str, world: &World) -> Vec<Finding> {
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
    // every installed name of this target is in names, so this is the whole graph and
    // nothing has to go back to disk for a depends the scan already read
    let mut deps: HashMap<String, Vec<Dep>> = HashMap::new();

    for (name, one) in names.iter().zip(scans(root, target, &names)) {
        let (mut rec, seen) = match one {
            Ok(x) => x,
            Err(e) => die(e),
        };
        deps.insert(name.clone(), std::mem::take(&mut rec.depends));
        // the loader opens a path, so a symlink on the way to a library is part of
        // resolution. musl reaches its libc through one: usr/lib/libc.musl-x86_64.so.1
        // points at the loader itself
        index(&rec.manifest, &mut present, &mut links);

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

    out.extend(accounted(target, &deps, &elves, &owners, &needs));

    // the kernel will not start a script whose interpreter is not there, which is the
    // same failure DT_NEEDED describes and nothing was checking it
    let (anywhere, anylinks) = world;
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
            exists(anywhere, anylinks, &want)
        } else {
            ["usr/bin", "usr/sbin"]
                .iter()
                .any(|d| exists(anywhere, anylinks, &format!("{d}/{want}")))
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
        // two libraries out of one package sharing names is how that package was built,
        // not two answers to the same question. readline ships libhistory, nspr ships
        // three of them, and no one can act on being told so
        if owners[a] == owners[b] {
            continue;
        }
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
type World = (HashSet<String>, HashMap<String, String>);

// every path either target installs, for the interpreter check: a script names one file
// and nothing says which target owns it
fn everywhere(root: &Path) -> World {
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

fn show(args: &[String]) {
    let (root, _, rest) = opts(args);
    let [name] = &rest[..] else {
        die("show wants one package".into());
    };
    let dir = match resolve(&root, name) {
        Ok(d) => d,
        Err(e) => die(e),
    };
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod mounts {
    use super::*;

    const REAL: &str = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/mapper/cryptroot / btrfs rw,noatime,compress=zstd:1,ssd,subvolid=256,subvol=/@root-a 0 0
/dev/mapper/cryptroot /var btrfs rw,noatime,compress=zstd:1,ssd,subvolid=259,subvol=/@var 0 0
/dev/nvme0n1p1 /efi vfat rw,noatime,fmask=0077 0 0
";

    // efibootmgr with no -v, copied off this machine. the two headers start with Boot as
    // well, an entry that is not active prints a space where the star goes, and the device
    // path comes after the label separated by a tab even though -v was not asked for
    const EFI: &str = "\
BootCurrent: 0002
Timeout: 1 seconds
BootOrder: 0001,0000,0002
Boot0000* kiry 7.2.3\tHD(1,GPT,1cb30d84,0x800,0x100000)/\\EFI\\Linux\\kiry-7.2.3.efi
Boot0001* kiry @root-a\tHD(1,GPT,1cb30d84,0x800,0x100000)/\\EFI\\Linux\\kiry-a.efi
Boot0002  kiry @root-b\tHD(1,GPT,1cb30d84,0x800,0x100000)/\\EFI\\Linux\\kiry-b.efi
";

    #[test]
    fn a_header_that_starts_with_boot_is_not_an_entry() {
        let got: Vec<&str> = EFI.lines().filter_map(entry).map(|(n, _)| n).collect();
        assert_eq!(got, ["0000", "0001", "0002"]);
    }

    // the line a deleted subvolume leaves behind, copied from the machine this bit on
    // it carries no subvol=, so the reader that looks for one says nothing is mounted and
    // the snapshot goes over a subvolume the last transaction still has open
    #[test]
    fn a_root_left_mounted_from_the_last_transaction_is_seen() {
        let stale = "/dev/mapper/cryptroot /run/kiry/root btrfs \
rw,relatime,compress=zstd:1,ssd,space_cache=v2,subvolid=267 0 0\n";
        assert_eq!(mounted_at(stale, "/run/kiry/root"), None);
        assert!(is_mounted(stale, "/run/kiry/root"));
        assert!(!is_mounted(REAL, "/run/kiry/root"));
        // a path that is only a prefix of a real mount point is not that mount point
        assert!(!is_mounted(REAL, "/va"));
        assert!(is_mounted(REAL, "/var"));
    }

    // a live install onto a root the firmware does not boot first is undone by the next
    // reboot, and the only thing that notices is the person wondering where it went
    #[test]
    fn a_root_the_firmware_does_not_boot_first_is_named() {
        assert_eq!(unkept(REAL, EFI), None);
        let other = REAL.replace("subvol=/@root-a", "subvol=/@root-b");
        assert_eq!(unkept(&other, EFI).as_deref(), Some("@root-b"));
        // an entry the firmware has never heard of is not a claim this can make
        let third = REAL.replace("subvol=/@root-a", "subvol=/@root-c");
        assert_eq!(unkept(&third, EFI), None);
    }

    #[test]
    fn an_entry_is_found_by_the_subvolume_its_label_names() {
        assert_eq!(entry_for(EFI, "@root-a").as_deref(), Some("0001"));
        assert_eq!(entry_for(EFI, "@root-b").as_deref(), Some("0002"));
        assert_eq!(entry_for(EFI, "@root-c"), None);
    }

    #[test]
    fn an_inactive_entry_is_still_found() {
        let (num, name) = entry("Boot0002  kiry @root-b\tHD(1,GPT,x)/File").unwrap();
        assert_eq!((num, name), ("0002", "kiry @root-b"));
    }

    #[test]
    fn the_label_stops_at_the_device_path() {
        let (_, name) = entry(EFI.lines().nth(4).unwrap()).unwrap();
        assert_eq!(name, "kiry @root-a");
    }

    #[test]
    fn the_order_is_read_and_the_entry_moves_to_the_front_of_it_once() {
        let was = order(EFI);
        assert_eq!(was, ["0001", "0000", "0002"]);
        assert_eq!(reorder("0002", &was), ["0002", "0001", "0000"]);
        // already first, and the one that is already there does not get a second place
        assert_eq!(reorder("0001", &was), ["0001", "0000", "0002"]);
    }

    #[test]
    fn the_entry_that_was_booted_is_read_and_is_not_the_one_the_subvolume_names() {
        assert_eq!(current(EFI).as_deref(), Some("0002"));
        // / is on @root-a in REAL, whose entry is 0001, and this boot came from 0002
        let (_, now, _) = pair(REAL).unwrap();
        assert_eq!(entry_for(EFI, &now).as_deref(), Some("0001"));
        assert_ne!(current(EFI), entry_for(EFI, &now));
    }

    #[test]
    fn a_root_already_at_the_front_is_already_committed() {
        assert_eq!(order(EFI).first().map(String::as_str), Some("0001"));
        assert_eq!(entry_for(EFI, "@root-a").as_deref(), Some("0001"));
    }

    #[test]
    fn only_what_stops_an_elf_running_refuses_a_commit() {
        assert!(What::Unresolved("libfoo.so.1".into()).breaks());
        assert!(What::MissingSymbol("foo".into()).breaks());
        assert!(What::NoInterpreter("x".into()).breaks());
        assert!(!What::Duplicate(3, "usr/lib/libbar.so".into()).breaks());
        assert!(!What::Unowned.breaks());
        assert!(!What::Modified.breaks());
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-m-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    // a/b is decided for / and these rules never fire anywhere else, so they are reached
    // directly rather than through a command that would refuse first
    #[test]
    fn a_libc_or_the_kernel_is_reason_enough() {
        let at = scratch("substrate");
        for n in ["musl", "glibc", "linux"] {
            let why = why_ab(&at, &[(n.to_string(), "x86_64-musl".into())]);
            assert_eq!(why.as_deref(), Some(&*format!("{n} is a libc or the kernel")));
        }
        assert!(why_ab(&at, &[("foot".to_string(), "x86_64-musl".into())]).is_none());
    }

    // core is the base and the toolchain that builds it, so a package there is under
    // enough of the system that "nothing has it open" is not the question
    #[test]
    fn a_recipe_in_core_is_reason_enough() {
        let at = scratch("incore");
        let core = at.join("var/db/kiry/core/deep");
        let extra = at.join("var/db/kiry/extra/shallow");
        for d in [&core, &extra] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("build"), ":\n").unwrap();
        }
        let one = |n: &str| why_ab(&at, &[(n.to_string(), "x86_64-musl".into())]);
        assert_eq!(one("deep").as_deref(), Some("deep is in core"));
        assert!(one("shallow").is_none(), "extra is not core");
        assert!(one("nowhere").is_none(), "a name no repo has is not core");
    }

    #[test]
    fn the_running_root_comes_out_of_its_mount_options() {
        let (dev, sub) = mounted_at(REAL, "/").unwrap();
        assert_eq!(dev, "/dev/mapper/cryptroot");
        // subvol= carries a leading slash and a subvolume name does not
        assert_eq!(sub, "@root-a");
        assert_eq!(mounted_at(REAL, "/var").unwrap().1, "@var");
        assert!(mounted_at(REAL, "/nowhere").is_none());
    }

    // /proc is first in the file and is not btrfs, so a parser that took the first line
    // or the first field would answer with it
    #[test]
    fn a_mount_that_is_not_the_one_asked_for_is_skipped() {
        assert!(mounted_at("proc /proc proc rw 0 0\n", "/").is_none());
        assert!(mounted_at("/dev/sda1 / ext4 rw 0 0\n", "/").is_none());
    }

    #[test]
    fn the_other_half_of_the_pair_is_the_one_not_running() {
        let (dev, now, other) = pair(REAL).unwrap();
        assert_eq!(dev, "/dev/mapper/cryptroot");
        assert_eq!(now, "@root-a");
        assert_eq!(other, "@root-b");

        let flipped = REAL.replace("subvol=/@root-a", "subvol=/@root-b");
        let (_, now, other) = pair(&flipped).unwrap();
        assert_eq!((now.as_str(), other.as_str()), ("@root-b", "@root-a"));
    }

    // a root outside the pair has no other half, and naming one would name a subvolume
    // that means nothing on this machine
    #[test]
    fn a_root_outside_the_pair_is_refused_by_name() {
        let odd = REAL.replace("subvol=/@root-a", "subvol=/@something");
        let e = pair(&odd).unwrap_err();
        assert!(e.contains("@something"), "{e}");
        assert!(e.contains("@root-a") && e.contains("@root-b"), "{e}");

        let e = pair("proc /proc proc rw 0 0\n").unwrap_err();
        assert!(e.contains("which subvolume"), "{e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn hit(pat: &str, hay: &str) -> Option<Vec<String>> {
        Rx::new(pat).unwrap().find(hay)
    }

    // every pattern the shipped table uses, against a line it has to catch
    #[test]
    fn the_table_patterns_match_the_lines_they_are_for() {
        for (pat, line) in [
            (
                "unknown argument: '(-[fm][\\w=-]+)'",
                "clang: error: unknown argument: '-fno-semantic-interposition'",
            ),
            ("(out of memory|signal 9|exit status 137)", "make: *** [all] Error 137 exit status 137"),
            ("(Not a valid object file|invalid bitcode)", "ld.lld: error: Not a valid object file"),
            ("recompile with -fPIC", "relocation R_X86_64_32 can not be used; recompile with -fPIC"),
            ("undefined symbol: __\\w+_chk", "undefined symbol: __memcpy_chk"),
            ("error: instruction requires:", "error: instruction requires: AVX-512"),
            ("PLEASE submit a bug report", "PLEASE submit a bug report to https://llvm.org"),
            ("LLVM ERROR: out of memory.*lto", "LLVM ERROR: out of memory while running lto"),
        ] {
            assert!(hit(pat, line).is_some(), "{pat} missed {line}");
        }
    }

    #[test]
    fn a_capture_is_what_drop_substitutes() {
        let c = hit("unknown argument: '(-[fm][\\w=-]+)'", "error: unknown argument: '-march=znver9'").unwrap();
        assert_eq!(c, vec!["-march=znver9"]);
    }

    #[test]
    fn an_alternation_picks_the_branch_that_is_there() {
        let c = hit("(out of memory|signal 9)", "killed: signal 9").unwrap();
        assert_eq!(c, vec!["signal 9"]);
    }

    // .* is greedy and still has to give characters back for what follows it
    #[test]
    fn a_star_backs_off_until_the_rest_fits() {
        assert!(hit("a.*z", "abcz middle z").is_some());
        assert!(hit("a.*z", "abc").is_none());
    }

    #[test]
    fn a_line_that_is_not_the_failure_does_not_match() {
        for (pat, line) in [
            ("recompile with -fPIC", "compiled with -fPIC already"),
            ("undefined symbol: __\\w+_chk", "undefined symbol: __chk"),
            ("(Not a valid object file|invalid bitcode)", "a valid object file"),
        ] {
            assert!(hit(pat, line).is_none(), "{pat} wrongly caught {line}");
        }
    }

    // [\w=-] is how the unknown-argument row is written, so the - must be a literal
    // and not the tail of a range
    #[test]
    fn a_class_takes_a_trailing_hyphen_literally() {
        assert!(hit("x[\\w=-]+y", "xa=b-cy").is_some());
        assert!(hit("x[\\w=-]+y", "x!y").is_none());
        assert!(hit("[^a-z]", "Q").is_some());
        assert!(hit("[^a-z]", "q").is_none());
        assert!(Rx::new("[abc").is_err());
    }

    // rejected at parse time, because the matcher would otherwise be quietly wrong
    #[test]
    fn what_the_subset_refuses_it_refuses_out_loud() {
        assert!(Rx::new("(a|b)+").is_err());
        assert!(Rx::new("+x").is_err());
        assert!(Rx::new("(ab").is_err());
        assert!(Rx::new("a)").is_err());
    }

    // a corpus of real failures with the action each must produce. hermetic, no builds,
    // and the place a pattern that starts over-matching gets caught
    #[test]
    fn every_captured_failure_gets_the_action_it_should() {
        let at = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/failures");
        let rs = rules(Path::new("/nonexistent-root")).unwrap();
        let mut seen = 0;
        for e in fs::read_dir(&at).unwrap().flatten() {
            let log = e.path();
            if log.extension().is_none_or(|x| x != "log") {
                continue;
            }
            let want = fs::read_to_string(log.with_extension("want")).unwrap();
            let want = want.trim();
            let got = scan(&rs, &fs::read_to_string(&log).unwrap());
            let got = got.as_ref().map_or("", |f| f.act.as_str());
            assert_eq!(got, want, "{}", log.display());
            seen += 1;
        }
        assert!(seen >= 8, "only {seen} fixtures found");
    }

    // the row the whole design is for: every object compiled, only the link died, so
    // halving the jobs is a relink and not a rebuild
    #[test]
    fn only_the_oom_row_keeps_the_work_dir() {
        let rs = rules(Path::new("/nonexistent-root")).unwrap();
        let at = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/failures");
        let reuse = |n: &str| {
            scan(&rs, &fs::read_to_string(at.join(n)).unwrap())
                .map(|f| f.reuse)
                .unwrap()
        };
        assert!(reuse("oom-kill.log"));
        assert!(!reuse("lto-oom.log"));
        assert!(!reuse("bitcode.log"));
    }

    #[test]
    fn the_phase_is_recorded_even_though_it_decides_nothing() {
        let at = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/failures");
        let of = |n: &str| phase(&fs::read_to_string(at.join(n)).unwrap());
        assert_eq!(of("fpic.log"), "link");
        assert_eq!(of("isa.log"), "compile");
        assert_eq!(of("clean.log"), "check");
    }

    // most specific first, and the lto row has to win over the plain out-of-memory one
    #[test]
    fn the_first_row_that_matches_is_the_one_that_fires() {
        let rs = rules(Path::new("/nonexistent-root")).unwrap();
        let f = scan(&rs, "LLVM ERROR: out of memory while running lto backend").unwrap();
        assert_eq!(f.act, "set LTO thin");
    }

    fn scratch(n: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-rx-{}", std::process::id()))
            .join(n);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    // the shipped file is the table, and the built-in is only what a root without one
    // falls back to. this is also what checks the file kiry installs actually parses
    #[test]
    fn a_table_on_disk_replaces_the_built_in_one() {
        let root = scratch("ondisk");
        let at = root.join("usr/share/kiry");
        fs::create_dir_all(&at).unwrap();
        fs::write(at.join("failures"), FAILURES).unwrap();

        let rs = rules(&root).unwrap();
        assert_eq!(rs.len(), FAILURES.lines().filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()).count());
        assert_eq!(scan(&rs, "ld.lld: error: invalid bitcode").unwrap().act, "filter-lto");
    }

    // a local rule is consulted ahead of the shipped one it narrows
    #[test]
    fn a_rule_in_failures_d_is_tried_first() {
        let root = scratch("failuresd");
        let d = root.join("etc/kiry/failures.d");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("10-local"), "invalid bitcode  set LTO none  clean\n").unwrap();

        let rs = rules(&root).unwrap();
        assert_eq!(scan(&rs, "ld.lld: error: invalid bitcode").unwrap().act, "set LTO none");
    }

    #[test]
    fn a_row_missing_its_retry_column_is_refused() {
        let root = scratch("badrow");
        let d = root.join("etc/kiry/failures.d");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("10-bad"), "boom  set LTO none  sometimes\n").unwrap();
        assert!(rules(&root).is_err());
    }

    fn site() -> String {
        CONFIG_SITE
            .replace("@LIBDIR@", "/usr/lib64")
            .replace("@INCLUDEDIR@", "/usr/include")
            .replace("@DATADIR@", "/usr/share")
    }

    // autoconf parses the arguments before it sources the site file, so an unguarded
    // assignment beats --includedir. nspr asked for a subdirectory and got the default
    #[test]
    fn the_site_file_leaves_a_directory_the_recipe_chose() {
        let ask = |pre: &str| {
            let o = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "prefix=/usr; exec_prefix='${{prefix}}'; libdir='${{exec_prefix}}/lib'; \
                     includedir='${{prefix}}/include'; datarootdir='${{prefix}}/share'; {pre}\n{}\n\
                     echo \"$libdir $includedir\"",
                    site()
                ))
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };

        assert_eq!(ask(""), "/usr/lib64 /usr/include");
        assert_eq!(ask("includedir=/usr/include/nspr"), "/usr/lib64 /usr/include/nspr");
    }

    // autoconf 2.71 reads a non-zero exit from the site script as the script having
    // failed, so the last line has to succeed with every directory already set
    #[test]
    fn a_site_file_that_changes_nothing_still_succeeds() {
        let o = Command::new("sh")
            .arg("-c")
            .arg(format!("libdir=/a; includedir=/b; datarootdir=/c\n{}", site()))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod deps {
    use super::*;

    const T: &str = "x86_64-musl";

    fn rec(root: &Path, name: &str, deps: &[Dep]) {
        db::write(
            root,
            &db::Installed {
                name: name.into(),
                target: T.into(),
                version: pkg::Version::parse("1.0 1").unwrap(),
                depends: deps.to_vec(),
                manifest: Vec::new(),
                hash: String::new(),
                users: Vec::new(),
                flags: Vec::new(),
            },
        )
        .unwrap();
    }

    fn dep(name: &str, make: bool) -> Dep {
        Dep {
            name: name.into(),
            make,
            host: make,
            only: None,
        }
    }

    // a real binary with real DT_NEEDED entries, so what gets tested is the walk from a
    // soname to the member carrying it rather than a fixture's idea of what one is
    fn fixture(name: &str) -> (PathBuf, Vec<sandbox::Member>, PathBuf, String) {
        let d = std::env::temp_dir()
            .join(format!("kiry-undecl-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        let (root, dest) = (d.join("root"), d.join("dest"));
        fs::create_dir_all(&dest).unwrap();

        let me = std::env::current_exe().unwrap();
        fs::copy(&me, dest.join("thing")).unwrap();
        let want = elf::read(&me).unwrap().needed.first().cloned().unwrap();

        rec(&root, "carrier", &[]);
        db::write_provides(
            &root,
            T,
            "carrier",
            &[db::Provide {
                soname: want.clone(),
                versioned: false,
                path: "usr/lib/carrier.so".into(),
            }],
        )
        .unwrap();

        let members = vec![sandbox::Member {
            name: "carrier".into(),
            direct: false,
            target: T.into(),
        }];
        (root, members, dest, want)
    }

    #[test]
    fn a_library_a_declared_dep_carries_is_not_reported() {
        let (root, members, dest, _) = fixture("direct");
        let deps = [dep("carrier", false)];
        assert!(undeclared(&root, &members, &deps, T, &dest).is_empty());
    }

    // it is there because something else asked for it, and the day that something stops
    // asking this package stops linking with no line anywhere explaining why
    #[test]
    fn a_library_reached_through_a_declared_deps_own_deps_is_undeclared() {
        let (root, members, dest, want) = fixture("indirect");
        rec(&root, "parent", &[dep("carrier", false)]);
        let deps = [dep("parent", false)];
        let got = undeclared(&root, &members, &deps, T, &dest);
        assert!(
            got.iter()
                .any(|(s, who, k)| *s == want && who == "carrier" && *k == "undeclared"),
            "{got:?}"
        );
    }

    // the worse one. nothing installs a make dep because something links it, so this
    // builds here and the package does not run on a root that never built it
    #[test]
    fn a_library_only_a_make_dep_carries_is_build_only() {
        let (root, members, dest, want) = fixture("makeonly");
        let deps = [dep("carrier", true)];
        let got = undeclared(&root, &members, &deps, T, &dest);
        assert!(
            got.iter()
                .any(|(s, who, k)| *s == want && who == "carrier" && *k == "build-only"),
            "{got:?}"
        );
    }
}
