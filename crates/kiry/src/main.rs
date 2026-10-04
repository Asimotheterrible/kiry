mod convert;
mod sandbox;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use kiry_core::pkg::Dep;
use kiry_core::{db, elf, install, pkg};

macro_rules! say {
    ($($a:tt)*) => {
        line(&format!($($a)*), false)
    };
}

// a failure, which -q still prints
macro_rules! loud {
    ($($a:tt)*) => {
        line(&format!($($a)*), true)
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
        Some("bisect-flags") => bisect_cmd(&args[1..]),
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
    say!("                  --live skips the fallback snapshot a risky install takes first");
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
    say!("recipe. b and i take --target T, -v to stream the build, -q for failures only,");
    say!("--recover to let the failure table retry, and --force to override a conflict.");
    say!("");
    say!("keeping up");
    say!("  ahead [--net] [pkg]...  which versions upstream moved past");
    say!("  sync [-n] [--net]       bump those into testing/");
    say!("  promote <pkg>...        testing/ into the tree");
    say!("  rebuild [-n] [-q]       drain the soname rebuild queue");
    say!("  bisect-flags <pkg>...   take the ladder's rungs out and find what still needs one");
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
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).on.clear();
    for l in msg.lines() {
        complain(l);
    }
    batch_end(false);
    std::process::exit(1);
}

// -q: failures and errors only. the log is written the same either way
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// what is building right now, as one line a terminal keeps rewriting in place. a pipe
// never gets it: there every state change is its own line, and a \r in a log is garbage
struct Live {
    on: Vec<(String, String, Instant, Option<u64>)>,
    painted: bool,
    cols: usize,
}

static LIVE: std::sync::Mutex<Live> = std::sync::Mutex::new(Live { on: Vec::new(), painted: false, cols: 0 });

// every line kiry prints comes through here, under the same lock the ticker paints
// with, so a line and the live line never land in the middle of each other
fn line(s: &str, loud: bool) {
    if !loud && QUIET.load(Ordering::Relaxed) {
        return;
    }
    let g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let r = match g.painted {
        true => write!(out, "\r\x1b[K{s}\n{}", live_text(&g)).and_then(|_| out.flush()),
        false => writeln!(out, "{s}"),
    };
    if r.is_err() {
        std::process::exit(0);
    }
}

// stderr, with the live line out of the way first. the ticker puts it back within a second
fn complain(s: &str) {
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    if g.painted {
        print!("\r\x1b[K");
        let _ = std::io::stdout().flush();
        g.painted = false;
    }
    eprintln!("kiry: {s}");
}

// cut to the terminal so it never wraps: a wrapped line is one \r cannot take back
fn live_text(g: &Live) -> String {
    let Some((_, what, start, eta)) = g.on.first() else { return String::new() };
    let mut s = format!("{what}  building  {}", clock(start.elapsed().as_secs()));
    if let Some(e) = eta {
        s += &format!(" / ~{}", clock(*e));
    }
    if g.on.len() > 1 {
        s += &format!("  +{}", g.on.len() - 1);
    }
    s.chars().take(g.cols.saturating_sub(1).max(20)).collect()
}

fn paint(g: &mut Live) {
    let text = live_text(g);
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K{text}").and_then(|_| out.flush());
    g.painted = !text.is_empty();
}

// a thread that repaints once a second is the whole of the animation. it is started by
// the first build that wants it and dies with the process
fn live_start(key: &str, eta: Option<u64>) {
    static TICK: std::sync::Once = std::sync::Once::new();
    TICK.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            if !g.on.is_empty() {
                paint(&mut g);
            }
        });
    });
    let what = match batch_run(key) {
        Some((i, n)) => format!("[{i}/{n}] {key}"),
        None => key.to_string(),
    };
    let cols = columns();
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    g.cols = cols;
    g.on.push((key.to_string(), what, Instant::now(), eta));
    paint(&mut g);
}

fn live_stop(key: &str) {
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let before = g.on.len();
    g.on.retain(|(k, _, _, _)| k != key);
    // nothing left paints an empty line, which is the old one cleared
    if g.on.len() != before && g.painted {
        paint(&mut g);
    }
}

fn live_on() -> bool {
    !LIVE.lock().unwrap_or_else(|e| e.into_inner()).on.is_empty()
}

// ansi 0-15 only, so the colours are whatever the terminal's theme says they are. a pipe
// never gets them and NO_COLOR set to anything at all turns them off
fn hue(code: &str, s: &str) -> String {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    match *ON.get_or_init(|| tty() && std::env::var_os("NO_COLOR").is_none()) && !code.is_empty() {
        true => format!("\x1b[{code}m{s}\x1b[0m"),
        false => s.to_string(),
    }
}

// the shape a package's result line has. a pipe gets it single spaced, a terminal gets
// columns as wide as the batch's widest, and a name is never cut to fit. was is the
// version installed on that target, which is all the upgrade colouring needs
fn row(name: &str, ver: &str, was: Option<&str>, t: &str, status: &str, secs: Option<u64>) -> String {
    let time = secs.map(clock).unwrap_or_default();
    if !tty() {
        return match secs {
            Some(_) => format!("{name} {ver} {t} {status} {time}"),
            None => format!("{name} {ver} {t} {status}"),
        };
    }
    let w = *WIDE.lock().unwrap_or_else(|e| e.into_inner());
    let slow = BATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|b| secs.is_some_and(|s| slowest(b, &format!("{name} {ver} {t}"), s)));
    let target = match t {
        t if t.ends_with("musl") => "36",
        t if t.ends_with("gnu") => "35",
        _ => "",
    };
    let state = match status {
        "ok" => "32",
        "failed" => "31",
        "held" | "skip" => "33",
        "stuck" => "2;31",
        _ => "",
    };
    let head = format!(
        "{}  {}  {}  ",
        hue("97", &format!("{name:<w$}", w = w[0])),
        painted(ver, was, w[1]),
        hue(target, &format!("{t:<w$}", w = w[2])),
    );
    match secs {
        Some(_) => format!(
            "{head}{}  {}",
            hue(state, &format!("{status:<6}")),
            hue(if slow { "33" } else { "2" }, &format!("{time:>6}"))
        ),
        None => format!("{head}{}", hue(state, status)),
    }
}

// the components that moved since the installed version are bright and the rest dim,
// so 25.2.1 over 25.2.0 reads as a patch before the number is read. a first install has
// nothing to move from and stays dim
fn painted(ver: &str, was: Option<&str>, w: usize) -> String {
    let Some(was) = was.filter(|was| *was != ver) else {
        return hue("2", &format!("{ver:<w$}"));
    };
    let sep = |c: char| !c.is_ascii_alphanumeric();
    let old: Vec<&str> = was.split(sep).collect();
    // runs of one brightness, so a line carries an escape per change and not per character
    let mut runs: Vec<(bool, String)> = Vec::new();
    for (i, piece) in ver.split_inclusive(sep).enumerate() {
        let comp = piece.trim_end_matches(sep);
        for (bright, text) in [(old.get(i) != Some(&comp), comp), (false, &piece[comp.len()..])] {
            match runs.last_mut() {
                Some((b, s)) if *b == bright => s.push_str(text),
                _ if text.is_empty() => {}
                _ => runs.push((bright, text.to_string())),
            }
        }
    }
    let mut out: String = runs.iter().map(|(b, s)| hue(if *b { "1" } else { "2" }, s)).collect();
    out.push_str(&" ".repeat(w.saturating_sub(ver.len())));
    out
}

// the version file alone. a whole record is its manifest too, and llvm's is 3500 lines
// read to learn one word
fn installed_version(root: &Path, t: &str, name: &str) -> Option<String> {
    let text = fs::read_to_string(db::dir(root, t, name).join("version")).ok()?;
    pkg::Version::parse(text.trim()).ok().map(|v| v.upstream)
}

// the key a build is known by on the live line and to the batch counting it
fn key(p: &Package, t: &str) -> String {
    format!("{} {} {t}", p.name, p.version.upstream)
}

// what a terminal is told and a pipe is not: the package a long batch is on, as the
// window title so it shows from an unfocused window, and a bell when the batch ends --
// on failure always, on success only after a minute and unless KIRY_BELL is off
struct Batch {
    root: PathBuf,
    start: Instant,
    total: usize,
    seen: HashSet<String>,
    recovered: bool,
    // the live line's [i/n] counts builds, a package per target, and n is only those
    // the cache cannot answer. each is its key()
    builds: Vec<String>,
    runs: HashMap<String, usize>,
    // what history says each build takes, and what the finished ones took this time
    eta: HashMap<String, u64>,
    took: HashMap<String, u64>,
}

static BATCH: std::sync::Mutex<Option<Batch>> = std::sync::Mutex::new(None);

// the columns of every result line kiry is about to print, batch or not. an install
// that takes everything from the cache starts no batch and still lines up
static WIDE: std::sync::Mutex<[usize; 3]> = std::sync::Mutex::new([0; 3]);

fn widths(rows: &[[&str; 3]]) {
    let mut wide = [0; 3];
    for r in rows {
        for (w, c) in wide.iter_mut().zip(r) {
            *w = (*w).max(c.len());
        }
    }
    *WIDE.lock().unwrap_or_else(|e| e.into_inner()) = wide;
}

fn tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

// rows is every name, version and target the batch could print, for the widths, and
// builds is the ones of them that compile
fn batch_begin(root: &Path, total: usize, builds: &[[&str; 3]], rows: &[[&str; 3]]) {
    if !tty() || total == 0 {
        return;
    }
    let past = history(root);
    let mut eta = HashMap::new();
    for [n, v, t] in builds {
        if let Some(s) = past.get(&(n.to_string(), t.to_string())) {
            eta.insert(format!("{n} {v} {t}"), *s);
        }
    }
    widths(rows);
    // the title the terminal had goes on its stack, so the end can put it back
    print!("\x1b[22;0t");
    *BATCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(Batch {
        root: root.to_path_buf(),
        start: Instant::now(),
        total,
        seen: HashSet::new(),
        recovered: false,
        builds: builds.iter().map(|[n, v, t]| format!("{n} {v} {t}")).collect(),
        runs: HashMap::new(),
        eta,
        took: HashMap::new(),
    });
}

fn batch_took(key: &str, secs: u64) {
    if let Some(b) = BATCH.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        b.took.insert(key.to_string(), secs);
    }
}

// the batch's slowest tenth, rounded up: no more of its builds than that took as long,
// counting the ones still to finish at what they took last time. one known time is no
// batch to be slow in, so a lone build stays dim
fn slowest(b: &Batch, key: &str, secs: u64) -> bool {
    let known: Vec<u64> = b
        .builds
        .iter()
        .filter_map(|k| b.took.get(k).or_else(|| b.eta.get(k)).copied())
        .collect();
    b.took.get(key) == Some(&secs)
        && known.len() >= 2
        && known.iter().filter(|&&v| v >= secs).count() <= known.len().div_ceil(10)
}

// where a build falls in the batch. a retry is the same build and keeps its number
fn batch_run(key: &str) -> Option<(usize, usize)> {
    let mut g = BATCH.lock().unwrap_or_else(|e| e.into_inner());
    let b = g.as_mut()?;
    let next = b.runs.len() + 1;
    let i = *b.runs.entry(key.to_string()).or_insert(next);
    Some((i, b.builds.len().max(i)))
}

fn batch_title(pkg: &str) {
    let mut g = BATCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(b) = g.as_mut() else { return };
    b.seen.insert(pkg.to_string());
    print!("\x1b]2;kiry {}/{} {pkg}\x07", b.seen.len().min(b.total), b.total);
    let _ = std::io::stdout().flush();
}

fn batch_recovered() {
    if let Some(b) = BATCH.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        b.recovered = true;
    }
}

fn batch_end(ok: bool) {
    let Some(b) = BATCH.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    print!("\x1b[23;0t");
    let secs = b.start.elapsed().as_secs();
    if ok {
        let state = if b.recovered { "failed" } else { "ok" };
        let line = format!("{} built in {}", b.seen.len(), clock(secs));
        with_art(&b.root, state, &[line]);
    } else {
        with_art(&b.root, "stuck", &[]);
    }
    let quiet = setting(&b.root, "KIRY_BELL").is_some_and(|v| v == "off");
    if !ok || (secs >= 60 && !quiet) {
        print!("\x07");
    }
    let _ = std::io::stdout().flush();
}

// one KIRY_* line out of /etc/kiry/config
fn setting(root: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(root.join("etc/kiry/config")).ok()?;
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| l.split_once(char::is_whitespace))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v.trim().to_string())
        .last()
}

// art and a quote beside a block of lines, pfetch-style, for stats and the end of a
// batch and nowhere else. a tty 80 columns wide or more, KIRY_ART and KIRY_QUOTES not
// off. no file for the state means no art, which is how kiry ships
fn with_art(root: &Path, state: &str, lines: &[String]) {
    let show = tty() && columns() >= 80;
    let on = |k: &str| show && !setting(root, k).is_some_and(|v| v == "off");
    let art = match on("KIRY_ART") {
        true => fs::read_to_string(root.join("usr/share/kiry/art").join(state)).unwrap_or_default(),
        false => String::new(),
    };
    let art: Vec<&str> = art.lines().collect();
    let wide = art.iter().map(|l| cells(l)).max().unwrap_or(0);
    for i in 0..art.len().max(lines.len()) {
        let a = art.get(i).copied().unwrap_or("");
        let l = lines.get(i).map_or("", String::as_str);
        match (wide, a) {
            (0, _) => say!("{l}"),
            (_, "") => say!("{}  {l}", " ".repeat(wide)),
            _ => say!("{a}\x1b[0m{}  {l}", " ".repeat(wide - cells(a))),
        }
    }
    if !on("KIRY_QUOTES") {
        return;
    }
    let all = fs::read_to_string(root.join("usr/share/kiry/quotes")).unwrap_or_default();
    let quotes: Vec<&str> = all.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let pick = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as usize);
    if let Some(q) = quotes.get(pick % quotes.len().max(1)) {
        say!("\n  \"{q}\"");
    }
}

// what a line takes on screen: escape sequences take nothing, and the full width forms
// character art is made of take two
fn cells(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (true, c) => esc = !c.is_ascii_alphabetic(),
            (false, c) => {
                let u = c as u32;
                let two = matches!(u, 0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff
                    | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7a3 | 0xf900..=0xfaff
                    | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6);
                n += if two { 2 } else { 1 };
            }
        }
    }
    n
}

// the terminal's width off the terminal itself. stty rather than an ioctl: rustix's
// termios is a feature this crate does not build with, and this is the art path
fn columns() -> usize {
    let Ok(tty) = fs::File::open("/dev/tty") else { return 0 };
    Command::new("stty")
        .arg("size")
        .stdin(tty)
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
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

// a --root that is not there reads as a tree with nothing installed, and a read that
// answers nothing for a typo looks exactly like a clean one
fn there(root: &Path) {
    if !root.is_dir() {
        die(format!("{}: no such directory", root.display()));
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
            "-q" => QUIET.store(true, Ordering::Relaxed),
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
        let p = match load(&root, &at) {
            Ok(p) => p,
            Err(e) => die(e),
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
    let rows: Vec<[&str; 3]> = todo
        .iter()
        .flat_map(|(n, ts)| {
            let v = recipes[n].version.upstream.as_str();
            ts.iter().map(move |t| [n.as_str(), v, t.as_str()])
        })
        .collect();
    batch_begin(&root, todo.len(), &rows, &rows);
    for (name, targets) in &todo {
        let p = &recipes[name];
        let r = match fix {
            true => targets.iter().try_for_each(|t| recover(&root, p, t, verbose, 1)),
            false => build(&root, p, targets, verbose, false, 1).map(|_| ()),
        };
        if let Err(e) = r {
            die(e);
        }
    }

    say!("cached {}", root.join("var/kiry/cache").display());
    batch_end(true);
}

// one build of a package at a time. compile() starts by deleting the stage dir, and a
// second run doing that under a first still packing is how a kiry i ate a kiry b of llvm
// mid-tar. the dir is deleted and remade, so the lock is a file beside it, held until
// the build function returns
fn claim(root: &Path, p: &Package) -> Result<fs::File, String> {
    let at = root
        .join("var/kiry/stage")
        .join(format!("{}-{}-{}.lock", p.name, p.version.upstream, p.version.rev));
    if let Some(d) = at.parent() {
        mkdirs(d)?;
    }
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&at)
        .map_err(|e| format!("{}: {e}", at.display()))?;
    if rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        let pid = fs::read_to_string(&at).unwrap_or_default();
        return Err(format!("{} is being built already, by pid {}", p.name, pid.trim()));
    }
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    Ok(f)
}

// one writer to the installed db at a time. two installs both plan against the db before
// either applies, both pass the conflict check, and the second apply leaves the first a
// record for files it no longer owns. held around plan and apply, never a build, so
// builds of different packages still overlap. a second writer waits rather than fails
fn writer(root: &Path) -> fs::File {
    use rustix::fs::{flock, FlockOperation};
    let at = root.join("usr/lib/kiry/db/lock");
    if let Some(Err(e)) = at.parent().map(mkdirs) {
        die(e);
    }
    let mut f = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&at)
    {
        Ok(f) => f,
        Err(e) => die(format!("{}: {e}", at.display())),
    };
    if flock(&f, FlockOperation::NonBlockingLockExclusive).is_err() {
        let pid = fs::read_to_string(&at).unwrap_or_default();
        match pid.trim() {
            "" => say!("waiting for another kiry writing {}", root.display()),
            pid => say!("waiting for pid {pid}, which is writing {}", root.display()),
        }
        loop {
            match flock(&f, FlockOperation::LockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => die(format!("{}: {e}", at.display())),
            }
        }
    }
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    f
}

// whether a build holds this lock right now. a lock nobody holds is a file like any other
fn locked(lock: &Path) -> bool {
    fs::File::open(lock).is_ok_and(|f| {
        rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockShared).is_err()
    })
}

fn build(
    root: &Path,
    p: &Package,
    targets: &[String],
    verbose: bool,
    boot: bool,
    at_once: usize,
) -> Result<Vec<PathBuf>, String> {
    let _lock = claim(root, p)?;
    batch_title(&p.name);
    let srcs = sources(root, p)?;
    let hash = recipe_hash(p, &srcs)?;
    let f = flags(root, &p.name, Some(&p.dir))?;

    let past = history(root);
    let mut built = Vec::new();
    for t in targets {
        let eta = past.get(&(p.name.clone(), t.clone())).copied();
        // -v has the build itself on the terminal, and a line rewriting itself under
        // that is two things fighting over one cursor
        let live = tty() && !verbose && !QUIET.load(Ordering::Relaxed);
        match (live, eta) {
            (true, _) => live_start(&key(p, t), eta),
            (false, Some(s)) => say!("{} building ~{}", key(p, t), clock(s)),
            (false, None) => say!("{} building -", key(p, t)),
        }
        let start = Instant::now();
        let r = compile(root, p, t, &srcs, &f, verbose, boot, at_once);
        live_stop(&key(p, t));
        let (work, linked) = r?;
        let secs = start.elapsed().as_secs();
        let gap = if tty() { "  " } else { " " };
        let note: String = [cache_rate(root, p, t, &work), again(root, p, t, &work, verbose)]
            .into_iter()
            .flatten()
            .map(|n| format!("{gap}{n}"))
            .collect();
        let was = installed_version(root, t, &p.name);
        batch_took(&key(p, t), secs);
        say!("{}{note}", row(&p.name, &p.version.upstream, was.as_deref(), t, "ok", Some(secs)));
        let cpu = fs::read_to_string(work.join("cpu")).ok().and_then(|c| c.trim().parse().ok());
        keep_time(root, p, t, secs, cpu);
        built.push((t.clone(), work, linked));
    }

    let mut ready = Vec::new();
    for (t, work, linked) in &built {
        ready.push((t.clone(), pack(root, p, t, work)?, linked.clone()));
    }

    // sidecar lands after the rename. ahead of it, a target failing to tar leaves a
    // .meta for an artifact that never arrives
    for (t, (art, part), linked) in &ready {
        fs::rename(part, art).map_err(|e| format!("{}: {e}", art.display()))?;
        meta(root, p, t, &hash, &f, art, linked)?;
        if let Some((_, work, _)) = built.iter().find(|(bt, _, _)| bt == t) {
            let mut d = art.as_os_str().to_owned();
            d.push(".meta/layout");
            let _ = fs::copy(work.join("layout"), PathBuf::from(d));
        }
    }
    for (_, work, _) in &built {
        let _ = fs::remove_dir_all(work);
    }
    Ok(ready.into_iter().map(|(_, (art, _), _)| art).collect())
}

// b of a package the cache already holds under the same key -- recipe, sources, flags,
// profile -- is a reproducibility check for free. the staged tree is set against the
// artifact it is about to replace, and a file that moved got that way from outside the
// key: the toolchain, the closure, or a build that does not reproduce. the count goes on
// the ok line and the paths in the log. nothing comparable in the cache says nothing
fn again(root: &Path, p: &Package, t: &str, work: &Path, verbose: bool) -> Option<String> {
    let art = artifact(root, p, t);
    // stale() lets a sidecar from before a field was recorded count as a hit. one that
    // does not say what it was built from cannot say it was this build, and no sidecar
    // at all is no artifact
    let meta = PathBuf::from(format!("{}.meta", art.display()));
    let says = ["recipe", "flags", "profile"].iter().all(|n| meta.join(n).is_file());
    if !says || stale(root, p, t, &art).is_some() {
        return None;
    }

    // extract() is the reader install trusts and it hashes on the way through. what it
    // writes is thrown away, the manifest it hands back is the point
    let was = work.join("was");
    let _ = fs::remove_dir_all(&was);
    mkdirs(&was).ok()?;
    let old = kiry_core::archive::extract(&was, &art, &[]);
    let _ = fs::remove_dir_all(&was);
    let old = old.ok()?;
    // a hardlink is its target's bytes. which of two names tar keeps as the file is down
    // to readdir order, and a walk of the staged tree cannot tell the two apart at all
    let sums: HashMap<&str, &str> = old
        .iter()
        .filter_map(|e| match &e.kind {
            db::Kind::File(s) => Some((e.path.as_str(), s.as_str())),
            _ => None,
        })
        .collect();
    let before: BTreeMap<String, (char, u32, String)> = old
        .iter()
        .map(|e| {
            let v = match &e.kind {
                db::Kind::File(s) => ('f', e.mode, s.clone()),
                db::Kind::Hard(to) => ('f', e.mode, sums.get(to.as_str()).unwrap_or(&"").to_string()),
                db::Kind::Link(to) => ('l', 0, to.clone()),
                db::Kind::Dir => ('d', e.mode, String::new()),
            };
            (e.path.clone(), v)
        })
        .collect();

    let dest = work.join("dest");
    let mut all = Vec::new();
    everything(&dest, &mut all);
    // setuid comes off both sides, the way extract() takes it off without a setuid list
    let now: BTreeMap<String, (char, u32, String)> = each(all.len(), |i| {
        let at = &all[i];
        let m = fs::symlink_metadata(at).ok()?;
        let mode = m.permissions().mode() & 0o1777;
        let v = match m.file_type() {
            k if k.is_symlink() => ('l', 0, fs::read_link(at).ok()?.to_string_lossy().into_owned()),
            k if k.is_dir() => ('d', mode, String::new()),
            _ => ('f', mode, kiry_core::sha256(fs::File::open(at).ok()?).ok()?),
        };
        Some((at.strip_prefix(&dest).ok()?.to_string_lossy().into_owned(), v))
    })
    .into_iter()
    .flatten()
    .collect();

    let mut moved: Vec<(&str, &str)> = Vec::new();
    for (path, a) in &before {
        let why = match now.get(path) {
            None => "only in old",
            Some(b) if a.0 != b.0 => "type",
            Some(b) if a.2 != b.2 && a.0 == 'l' => "link target",
            Some(b) if a.2 != b.2 => "content",
            Some(b) if a.1 != b.1 => "mode",
            Some(_) => continue,
        };
        moved.push((why, path.as_str()));
    }
    for path in now.keys().filter(|k| !before.contains_key(*k)) {
        moved.push(("only in new", path.as_str()));
    }
    if moved.is_empty() {
        return Some("same bytes".into());
    }
    moved.sort_by_key(|m| m.1);
    let note = match moved.len() {
        1 => "1 file differs".to_string(),
        n => format!("{n} files differ"),
    };
    let mut text = format!("kiry: {note} from the artifact this build replaces\n");
    for (why, path) in &moved {
        text += &format!("kiry:   {why:<11}  {path}\n");
    }
    match verbose {
        // -v wrote no log, the terminal was the log
        true => text.lines().for_each(|l| say!("{l}")),
        false => {
            let _ = fs::OpenOptions::new()
                .append(true)
                .open(logpath(root, p, t))
                .and_then(|mut f| f.write_all(text.as_bytes()));
        }
    }
    Some(note)
}

// every path under a staged tree, directories and links with the files, the top itself not
fn everything(at: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(at) else { return };
    for e in rd.flatten() {
        out.push(e.path());
        if e.file_type().is_ok_and(|k| k.is_dir()) {
            everything(&e.path(), out);
        }
    }
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
    // nitro is the init, so an openrc service has no openrc-run to start it. alpine's
    // recipes install them and so do plenty of make installs
    let openrc: Vec<&str> = ["etc/init.d", "etc/conf.d"]
        .into_iter()
        .filter(|d| dest.join(d).exists())
        .collect();
    for d in &openrc {
        let at = dest.join(d);
        fs::remove_dir_all(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    if !openrc.is_empty() {
        say!("{name} {t} dropped {}", openrc.join(" "));
    }

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
                if let Err(e) = grab(from, &dst) {
                    match p.checksums.get(i).filter(|c| c.len() == 128) {
                        Some(_) => mirrored(name, &dst).map_err(|m| format!("{e}\n{m}"))?,
                        None => return Err(e),
                    }
                }
            }
            dst
        } else {
            p.dir.join(from)
        };

        let sum = sha(&path)?;
        // the length says which one it is. a converted recipe carries alpine's sha512 for
        // a tarball it never fetched, and everything kiry hashed itself is sha256
        match p.checksums.get(i) {
            Some(want) => {
                let got = match want.len() {
                    128 => fs::File::open(&path)
                        .and_then(kiry_core::sha512)
                        .map_err(|e| format!("{}: {e}", path.display()))?,
                    _ => sum.clone(),
                };
                if want != &got {
                    return Err(format!(
                        "{}: checksum is {got}, recipe says {want}",
                        path.display()
                    ));
                }
            }
            None => complain(&format!("{}: no checksum, sha256 is {sum}", path.display())),
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
    fetch(url, dst, false)
}

fn fetch(url: &str, dst: &Path, quiet: bool) -> Result<(), String> {
    // one partial file per fetch. two builds of a level can go after the same tarball at
    // once, and a shared .part is two downloads interleaved into one file
    static FETCH: AtomicUsize = AtomicUsize::new(0);
    let mut part = dst.as_os_str().to_owned();
    part.push(format!(".part{}", FETCH.fetch_add(1, Ordering::Relaxed)));
    let part = PathBuf::from(part);

    let mut c = fetcher(url, &part)?;
    if quiet {
        c.stdout(Stdio::null()).stderr(Stdio::null());
    }
    if let Err(e) = run(&mut c, url) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }

    fs::rename(&part, dst).map_err(|e| format!("{}: {e}", dst.display()))
}

const DISTFILES: &str = "https://distfiles.alpinelinux.org/distfiles/";

// alpine keeps every source it has built under the file's own name, and a converted
// recipe's sha512 names exactly those bytes, so an upstream that moved or died costs a
// second fetch rather than a stuck build. xmlto's pagure url and rsync's samba one went
// in the same afternoon. KIRY_MIRROR is the list to try instead, for a test or a nearer
// copy; unset, it is every branch alpine's index lists, edge and then newest first
fn mirrored(name: &str, dst: &Path) -> Result<(), String> {
    let bases: Vec<String> = match std::env::var("KIRY_MIRROR") {
        Ok(v) => v.split_whitespace().map(String::from).collect(),
        Err(_) => branches(dst)?,
    };
    for b in &bases {
        let url = format!("{}/{name}", b.trim_end_matches('/'));
        if fetch(&url, dst, true).is_ok() {
            say!("{name} from {url}");
            return Ok(());
        }
    }
    Err(format!("{name}: not at {} either", bases.join(" ")))
}

fn branches(near: &Path) -> Result<Vec<String>, String> {
    let at = near.with_file_name(".distfiles-index");
    fetch(DISTFILES, &at, true)?;
    let text = fs::read_to_string(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    let _ = fs::remove_file(&at);
    let mut v: Vec<(u32, u32, String)> = text
        .split("href=\"")
        .filter_map(|h| h.split('"').next())
        .filter_map(|h| {
            let b = h.strip_suffix('/')?;
            if b == "edge" {
                return Some((u32::MAX, 0, b.to_string()));
            }
            let (x, y) = b.strip_prefix('v')?.split_once('.')?;
            Some((x.parse().ok()?, y.parse().ok()?, b.to_string()))
        })
        .collect();
    v.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    Ok(v.into_iter().map(|(_, _, b)| format!("{DISTFILES}{b}")).collect())
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
    // curl's meter is a line it keeps rewriting with \r on stderr. in a pipe that is
    // noise in whatever reads kiry's, and on a terminal it fights the live line. -S keeps
    // curl's own error when it fails
    use std::io::IsTerminal;
    if Path::new(prog).file_name().is_some_and(|n| n == "curl") && (!std::io::stderr().is_terminal() || live_on()) {
        c.arg("-sS");
    }
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
//
// no LTO none: filter-lto has already taken -flto and the jobs cap out of all three
// lists by then, so it resolves to the same flags every time and could only spend a
// build. the last rung is the smallest set strip-flags leaves standing rather than
// -pipe, which moves where the compiler keeps its temporaries and nothing in the code
//
// CC_WRAPPER walks the same rungs in place when clang crashes, so a change here is a
// change there too
//
// -march comes off as a filter rather than a CFLAGS_MARCH line with nothing after it,
// which reads as a setting someone forgot to finish, and would take rust's target-cpu
// with it
const LADDER: &[(bool, &str)] = &[
    (true, "filter-lto"),
    (false, "OPT -O2"),
    (true, "filter-flags -march=*"),
    (true, "strip-flags"),
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
        .map_err(|e| format!("{}: {e}", at.display()))
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

// the line to put in front of someone: the first real complaint. the last line is
// usually not it -- mozbuild signs off with where it wrote a profile, four lines under the
// ValueError that stopped it -- and make, ninja, clang and collect2 all sign off with a
// line that only says something above it failed. not found is a weaker kind, since a
// build script probing for which or git says it on the way to working. a warning is never
// it, and past all of those the line make's first failure comes right after is: rrdtool's
// was `The setup requires setuptools.`, with no word in it to look for
fn blame(log: &str) -> String {
    let lines: Vec<String> = log
        .lines()
        .map(|l| unescaped(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let summary = |l: &str| {
        let w: Vec<&str> = l.split_whitespace().collect();
        matches!(w[..], [.., "Error", n] | [.., "Error", n, "(ignored)"] if n.parse::<u32>().is_ok())
            || (l.ends_with("generated.") && l.contains(" error"))
            || l.starts_with("ninja: build stopped")
            || l.contains("ld returned 1 exit status")
            || l.contains("linker command failed with exit code")
    };
    let hit = |words: &[&str]| {
        lines.iter().find(|l| {
            let s = l.to_lowercase();
            !summary(l)
                && !s.starts_with("checking ")
                && !s.contains("warning:")
                && words.iter().any(|w| s.contains(w))
        })
    };
    let stopped = lines.iter().position(|l| summary(l) && !l.ends_with("(ignored)"));
    hit(&["error:", "fatal error", "undefined reference", "undefined symbol"])
        .or_else(|| hit(&["not found", "no such file"]))
        .or_else(|| lines[..stopped?].iter().rev().find(|l| !l.to_lowercase().contains("warning:")))
        .or_else(|| lines.iter().rev().find(|l| !summary(l) && l.to_lowercase().contains("error")))
        .or_else(|| lines.last())
        .map_or("the log is empty".to_string(), String::clone)
}

// a compiler that colours its diagnostics colours them in the log too
fn unescaped(s: &str) -> String {
    let mut esc = false;
    s.chars()
        .filter(|&c| {
            let keep = !esc && c != '\x1b';
            esc = match (esc, c) {
                (false, c) => c == '\x1b',
                (true, c) => !c.is_ascii_alphabetic(),
            };
            keep
        })
        .collect()
}

// the failed line and what to read next under it: where it died, whether the table knows
// the failure, and the first real complaint rather than the log. -v had the log on the
// terminal already and wrote none
fn failed(root: &Path, p: &Package, t: &str, log: Option<&Path>, secs: u64) {
    SHOWN.with(|s| s.set(true));
    live_stop(&key(p, t));
    let was = installed_version(root, t, &p.name);
    batch_took(&key(p, t), secs);
    loud!("{}", row(&p.name, &p.version.upstream, was.as_deref(), t, "failed", Some(secs)));
    let Some(log) = log else { return };
    let text = fs::read_to_string(log).unwrap_or_default();
    let rule = rules(root).ok().and_then(|rs| scan(&rs, &text));
    loud!("  phase  {}", phase(&text));
    loud!("  rule   {}", rule.map_or("none matched".to_string(), |f| format!("{} -> {}", f.rule, f.act)));
    loud!("  log    {}", log.display());
    let first = blame(&text);
    match (tty(), columns()) {
        (true, c) if c > 2 => loud!("  {}", first.chars().take(c - 2).collect::<String>()),
        _ => loud!("  {first}"),
    }
}

thread_local! {
    // failed() printed its block for the build that just ended on this thread, so the
    // error that comes back after it says nothing the block did not
    static SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// build fails, read the log, fix it, build again. three signature retries and never the
// same action twice, then the ladder, then it is stuck and says why
fn recover(root: &Path, p: &Package, t: &str, verbose: bool, at_once: usize) -> Result<(), String> {
    let rs = rules(root)?;
    let one = [t.to_string()];
    let mut tried: Vec<String> = Vec::new();
    let mut rung = 0;
    // so a first.log that is there is always this run's
    let first = firstlog(root, p, t);
    let _ = fs::remove_file(&first);
    // a run that ends stuck proved none of what it wrote fixes anything, and left behind
    // it reads as a fix. retroarch kept filter-lto and a march rung that way
    let files = [p.dir.join("filter"), root.join("etc/kiry/pkg").join(&p.name)];
    let had: Vec<Option<String>> = files.iter().map(|f| fs::read_to_string(f).ok()).collect();
    let undo = || {
        for (f, h) in files.iter().zip(&had) {
            let _ = match h {
                Some(h) => fs::write(f, h),
                None => fs::remove_file(f),
            };
        }
    };

    loop {
        SHOWN.with(|s| s.set(false));
        match build(root, p, &one, verbose, false, at_once) {
            // first.log is only there once this run has failed
            Ok(_) if first.exists() => {
                batch_recovered();
                return Ok(());
            }
            Ok(_) => return Ok(()),
            Err(_) if SHOWN.with(|s| s.get()) => {}
            Err(e) => loud!("{e}"),
        }
        let log = fs::read_to_string(logpath(root, p, t)).unwrap_or_default();
        // every retry truncates the log, so without this the failure that started it all
        // is gone by the time anyone reads it and the rung that fixed it cannot be judged
        if !first.exists() {
            let _ = fs::copy(logpath(root, p, t), &first);
        }

        // the wrapper already walked the ladder in place on the file that crashed, and
        // clang crashed on every rung. the table's crash row and the ladder here would
        // walk the same rungs again, one whole build each
        if let Some(l) = log.lines().find(|l| l.starts_with("kirycc: ") && l.ends_with("at every rung")) {
            loud!("first failure is {}", first.display());
            undo();
            return Err(stuck(p, t, &l["kirycc: ".len()..]));
        }

        if tried.len() < 3 {
            if let Some(f) = scan(&rs, &log) {
                if let Some(why) = f.act.strip_prefix("notaflag ") {
                    undo();
                    return Err(stuck(p, t, why));
                }
                if !tried.contains(&f.act) {
                    tried.push(f.act.clone());
                    if let Some(wrote) = write_fix(root, &p.dir, &p.name, &f, &log)? {
                        loud!("  retry  {}/3  {wrote}", tried.len());
                        continue;
                    }
                }
            }
        }

        if !compiled(&log) {
            // no note anywhere: the recipe and the settings are both innocent here
            loud!("first failure is {}", first.display());
            undo();
            return Err(format!(
                "{} {t}: stuck, nothing compiled: {}",
                p.name,
                blame(&log)
            ));
        }

        // a rung that resolves to the flags already in force rebuilds into a result
        // already known. OPT -O2 on a box whose config says -O2 is that, and so is any
        // rung whose flag an earlier fix already took out
        let now = flags(root, &p.name, Some(&p.dir))?;
        let (filter, line) = loop {
            let Some(r) = LADDER.get(rung) else {
                loud!("first failure is {}", first.display());
                undo();
                return Err(stuck(p, t, "the ladder ran out"));
            };
            rung += 1;
            let then = flags_with(root, &p.name, Some(&p.dir), Some(r))?;
            if then.record() != now.record() || then.env != now.env {
                break r;
            }
        };
        let at = match filter {
            true => p.dir.join("filter"),
            false => root.join("etc/kiry/pkg").join(&p.name),
        };
        // the line that failed goes beside the rung, as it does beside a table fix. a
        // rung with only its phase on it cannot be judged once the log is overwritten
        let why = format!("rung {rung} after {} failed on {t}\n#   {}", phase(&log), blame(&log));
        note(&at, line, &why)?;
        loud!("  retry  rung {rung}/{}  {line} in {}", LADDER.len(), at.display());
    }
}

// the ladder only walks down, and what it wrote was true of the toolchain, the sysroot
// and the recipe on the day. all three move. this takes the rungs a package carries back
// out and recovers from the full flags, so a rung still needed comes back with a fresh
// note and a stale one stays gone. the table's fixes stay: each one names the line that
// needed it, and a rung only ever named a phase
fn bisect_cmd(args: &[String]) {
    let (root, _, rest) = opts(args);
    let names = asked(rest, &[]);
    writes(&root);
    if names.is_empty() {
        die("bisect-flags wants a package".into());
    }
    for n in expand(&root, names) {
        let p = match resolve(&root, &n).and_then(|at| load(&root, &at)) {
            Ok(p) => p,
            Err(e) => die(e),
        };
        let files = [p.dir.join("filter"), root.join("etc/kiry/pkg").join(&p.name)];
        let had: Vec<Option<String>> = files.iter().map(|f| fs::read_to_string(f).ok()).collect();
        let mut any = false;
        for (f, h) in files.iter().zip(&had) {
            let Some(h) = h else { continue };
            let left = unrung(h);
            if left == *h {
                continue;
            }
            any = true;
            let r = match left.trim().is_empty() {
                true => fs::remove_file(f),
                false => fs::write(f, left),
            };
            if let Err(e) = r {
                die(format!("{}: {e}", f.display()));
            }
        }
        if !any {
            say!("{} carries no rung", p.name);
            continue;
        }
        if let Err(e) = p.targets.iter().try_for_each(|t| recover(&root, &p, t, false, 1)) {
            // stuck from the full flags says nothing about the rungs it had, so the
            // package goes back to exactly what it built with before
            for (f, h) in files.iter().zip(&had) {
                if let Some(h) = h {
                    let _ = fs::write(f, h);
                }
            }
            die(e);
        }
    }
}

// a filter or settings file with the ladder's lines taken out. note() heads each one
// with "# <date> rung", and only the dated header nearest the line decides whose it is:
// comments above that belong to whoever wrote them
fn unrung(text: &str) -> String {
    let dated = |c: &&str| {
        c.split_whitespace()
            .nth(1)
            .is_some_and(|d| d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-')
    };
    let mut out: Vec<&str> = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for l in text.lines() {
        if l.trim_start().starts_with('#') {
            block.push(l);
            continue;
        }
        match block.iter().rposition(dated) {
            Some(at) if block[at].split_whitespace().nth(2) == Some("rung") => {
                out.extend(&block[..at]);
            }
            _ => {
                out.extend(&block);
                out.push(l);
            }
        }
        block.clear();
    }
    out.extend(&block);
    match out.is_empty() {
        true => String::new(),
        false => out.join("\n") + "\n",
    }
}

fn artifact(root: &Path, p: &Package, t: &str) -> PathBuf {
    root.join("var/kiry/cache").join(format!(
        "{}-{}-{}.{t}.tar.zst",
        p.name, p.version.upstream, p.version.rev
    ))
}

// cached() without the line it prints, for planning, which asks before anything runs
fn fresh(root: &Path, p: &Package, t: &str) -> bool {
    let a = artifact(root, p, t);
    a.is_file() && stale(root, p, t, &a).is_none()
}

fn cached(root: &Path, p: &Package, t: &str) -> Option<PathBuf> {
    let a = artifact(root, p, t);
    if !a.is_file() {
        return None;
    }
    match stale(root, p, t, &a) {
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
// that changes a build without moving any of them: the recipe, the flags it compiles
// with, and the stored pgo profile. the dependency closure is deliberately not in here --
// see TODO
//
// a sidecar written before kiry recorded one of these says nothing rather than no. the
// check would otherwise throw away a cache that is mostly still good on the day it lands
fn stale(root: &Path, p: &Package, t: &str, art: &Path) -> Option<&'static str> {
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
    // a profile rm -r'd to retrain reads as none here, and that is what sends the
    // artifact built on it back through a build that trains again
    if let Ok(was) = fs::read_to_string(d.join("profile")) {
        if was.trim() != profile_sum(&profile(root, p, t)) {
            return Some("profile changed");
        }
    }
    None
}

// a trained profile outlives the build that made it, which is what spares the next build
// its training run. per target, because musl and gnu compile different code
fn profile(root: &Path, p: &Package, t: &str) -> PathBuf {
    root.join("var/kiry/profiles")
        .join(&p.name)
        .join(t)
        .join("merged.profdata")
}

fn profile_sum(at: &Path) -> String {
    fs::File::open(at)
        .ok()
        .and_then(|f| kiry_core::sha256(f).ok())
        .unwrap_or_else(|| "none".into())
}

// whole or not at all, because the next build is fed whatever sits at the name.
// generated-for is for whoever wonders how old it is: an indexed profile only reads
// forward, so after a clang bump the version it was written by is the first question.
// its last word is how many functions the training build itself discarded
fn keep_profile(
    p: &Package,
    out: &Path,
    stored: &Path,
    missed: Option<usize>,
) -> Result<(), String> {
    let dir = stored.parent().unwrap_or(stored);
    mkdirs(dir)?;
    let part = dir.join("merged.profdata.part");
    fs::copy(out, &part).map_err(|e| format!("{}: {e}", part.display()))?;
    fs::rename(&part, stored).map_err(|e| format!("{}: {e}", stored.display()))?;
    let clang = Command::new("clang")
        .arg("-dumpversion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "?".into(), |s| s.trim().to_string());
    put(
        &dir.join("generated-for"),
        &format!(
            "{} {} clang {clang} {} discarded {}\n",
            p.version.upstream,
            p.version.rev,
            today(),
            missed.map_or_else(|| "?".into(), |n| n.to_string())
        ),
    )
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
    at_once: usize,
) -> Result<(PathBuf, Vec<String>), String> {
    let start = Instant::now();
    let work = root.join("var/kiry/stage").join(format!(
        "{}-{}-{}.{t}",
        p.name, p.version.upstream, p.version.rev
    ));
    let src = work.join("src");
    let dest = work.join("dest");

    let _ = fs::remove_dir_all(&work);
    mkdirs(&src)?;
    let _ = fs::remove_dir_all(&dest);
    mkdirs(&dest)?;

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

    // abuild's split: a recipe with an unpack() of its own is handed the sources as they
    // arrived and extracts what it wants, which is how libyuv hashes the tree it untars
    let text = fs::read_to_string(&script).unwrap_or_default();
    let own_unpack = text.contains("\nunpack()");
    for (name, path, _) in srcs {
        let name = name.as_str();
        if tarball(name) && !own_unpack {
            untar(path, &src, name)?;
            continue;
        }
        fs::copy(path, src.join(name)).map_err(|e| format!("{name}: {e}"))?;
        if name.ends_with(".zip") && !own_unpack {
            run(Command::new("unzip").arg("-qo").arg(path).arg("-d").arg(&src), name)?;
        }
    }

    let sysroot = work.join("sysroot");
    let deps: Vec<Dep> = p.depends.iter().filter(|d| d.applies(t)).cloned().collect();
    let members = sandbox::closure(root, t, &deps)?;
    sandbox::assemble(root, t, &members, &sysroot)?;
    // this and the crates and modules below are what a build writes that outlives it,
    // mounted rather than kept in the stage dir, which the next build of it deletes
    let lto = lto_cache(root, &p.name);
    mkdirs(&lto)?;
    mkdirs(&sysroot.join(sandbox::LTO_AT))?;

    // the sandbox has no network and 363 recipes cargo fetch in prepare, so the fetch
    // happens out here. Cargo.lock pins every crate by sha256 the way checksums pins a
    // tarball, and the recipe's own fetch then finds them all in CARGO_HOME. a patch
    // that moves Cargo.lock in prepare comes after this and still fails offline.
    // only a recipe that fetches with that tool gets it: go's own tree and firefox's
    // carry locks and build from what they ship, and a go.sum inside a vendored crate is
    // no go build. options net alone is every npm and pip recipe too
    let net = text.lines().any(|l| {
        l.strip_prefix("options=")
            .is_some_and(|o| o.trim_matches(['"', '\'']).split_whitespace().any(|w| w == "net"))
    });
    let cargo = root.join("var/kiry/cargo");
    mkdirs(&cargo)?;
    mkdirs(&sysroot.join(sandbox::CARGO_AT))?;
    let patches: Vec<PathBuf> = srcs
        .iter()
        .map(|(name, ..)| src.join(name))
        .filter(|f| f.extension().is_some_and(|e| e == "patch") && edits_lock(f))
        .collect();
    let fetches = text.contains("cargo fetch") || net && names(&text, "cargo");
    for lock in shallowest(&src, "Cargo.lock", 3).into_iter().filter(|_| fetches) {
        let mut c = Command::new("cargo");
        c.arg("fetch")
            .arg("--locked")
            .arg("--manifest-path")
            .arg(lock.with_file_name("Cargo.toml"))
            .env("CARGO_HOME", abs(&cargo)?);
        prefetch(&mut c, &lock, &patches)?;
    }
    // go's modules the same way, go.sum being its lock. GOTOOLCHAIN=local on both sides,
    // or a go.mod asking for a newer go downloads one
    let gomod = root.join("var/kiry/gomod");
    mkdirs(&gomod)?;
    mkdirs(&sysroot.join(sandbox::GOMOD_AT))?;
    let downloads = text.contains("go mod download") || net && names(&text, "go");
    for sum in shallowest(&src, "go.sum", 3).into_iter().filter(|_| downloads) {
        if sum.with_file_name("vendor/modules.txt").is_file() {
            continue;
        }
        let mut c = Command::new("go");
        c.arg("mod")
            .arg("download")
            .current_dir(sum.parent().unwrap_or(&src))
            .env("GOMODCACHE", abs(&gomod)?)
            .env("GOFLAGS", "-modcacherw")
            .env("GOTOOLCHAIN", "local");
        prefetch(&mut c, &sum, &patches)?;
    }

    fs::copy(&script, sysroot.join("build")).map_err(|e| format!("build script: {e}"))?;
    let share = sysroot.join("usr/share/kiry");
    mkdirs(&share)?;
    fs::write(share.join("lib.sh"), LIB_SH).map_err(|e| format!("lib.sh: {e}"))?;
    // copied in rather than mounted, so the build cannot write over the one kiry keeps.
    // a recipe opts in by reading KIRY_PROFILE and leaving one at KIRY_PROFILE_OUT
    let stored = profile(root, p, t);
    let fed = stored.is_file();
    if fed {
        fs::copy(&stored, share.join("profile.profdata"))
            .map_err(|e| format!("{}: {e}", stored.display()))?;
    }
    // the automake in the root carries the config.sub that knows musl, and staging it
    // here is what gets it to the callers with no automake dep of their own
    let automake = fs::read_dir(root.join("usr/share"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.file_name().is_some_and(|n| n.to_string_lossy().starts_with("automake-")))
        .map(|d| d.join("config.sub"))
        .find(|p| p.is_file());
    if let Some(sub) = automake {
        fs::copy(&sub, share.join("config.sub")).map_err(|e| format!("config.sub: {e}"))?;
    }
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
        .replace("@SYSROOT@", sysroot_arg)
        .replace("@LTO@", LTO_CACHE);
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
        ("kirycc", cc.replace("@REAL@", "clang").replace("@EXTRA@", "")),
        (
            "kiryc++",
            cc.replace("@REAL@", "clang++").replace("@EXTRA@", &cxx_extra),
        ),
    ] {
        let at = bin.join(n);
        fs::write(&at, &body).map_err(|e| format!("{n}: {e}"))?;
        fs::set_permissions(&at, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{n}: {e}"))?;
    }

    // not current_exe(): once an install renames a new kiry over this one, that reads back
    // `/usr/bin/kiry (deleted)` and every later build of the batch fails to spawn. the
    // magic link still execs the image that is running
    let mut c = Command::new("/proc/self/exe");
    sandbox::tied(&mut c)
        .arg("sandbox")
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
        .env("MAKEFLAGS", makeflags(jobs(), at_once))
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
        .env("CC", "kirycc")
        .env("CXX", "kiryc++")
        .env("KIRY_SRCDIR", "/src")
        .env("KIRY_TARGET", t)
        .env("KIRY_NAME", &p.name)
        .env("KIRY_VERSION", &p.version.upstream)
        .env("KIRY_REV", p.version.rev.to_string())
        .env(
            "KIRY_PROFILE",
            if fed { "/usr/share/kiry/profile.profdata" } else { "" },
        )
        // /src because /tmp dies with the namespace and /dest gets packaged
        .env("KIRY_PROFILE_OUT", "/src/.kiry-profile.profdata")
        .env("CFLAGS", &f.cflags)
        .env("CXXFLAGS", &f.cxxflags)
        .env("LDFLAGS", &f.ldflags)
        .env("RUSTFLAGS", &f.rustflags)
        .env("KIRY_FLAGS", f.use_flags.join(" "))
        .env("KIRY_MESON_ARGS", meson_args(&src, &f.use_flags));
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
    // the build is its own process, so the number of them running at once is something
    // only this side knows and has to say
    c.env("KIRY_SHARE", at_once.to_string());
    c.env("KIRY_LTO_CACHE", abs(&lto)?);
    c.env("KIRY_CARGO", abs(&cargo)?);
    c.env("CARGO_HOME", format!("/{}", sandbox::CARGO_AT));
    c.env("CARGO_NET_OFFLINE", "true");
    c.env("KIRY_GOMOD", abs(&gomod)?);
    c.env("GOMODCACHE", format!("/{}", sandbox::GOMOD_AT));
    c.env("GOPROXY", "off");
    c.env("GOFLAGS", "-modcacherw");
    c.env("GOTOOLCHAIN", "local");
    let prune = p.dir.join("symbol-prune").is_file() && !t.ends_with("gnu");
    if prune {
        c.env("KIRY_PRUNE", "1");
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

    let watching = watch(&lto);
    let status = c.spawn().and_then(|mut ch| {
        if let Some(cpu) = cpu_of(&ch) {
            let _ = fs::write(work.join("cpu"), format!("{cpu}\n"));
        }
        ch.wait()
    });
    // stopped ahead of prune_pass, whose relinks take a version script and miss on purpose
    if let Some((hits, misses)) = watching.and_then(Watch::stop).filter(|(h, m)| h + m > 0) {
        let _ = fs::write(work.join("thinlto"), format!("{hits} {misses}\n"));
    }
    match status {
        Ok(s) if s.success() => {
            // a file the wrapper stepped down built with less than the record says, and
            // the log is the only other place that knows
            if !verbose {
                for l in fs::read_to_string(&log).unwrap_or_default().lines() {
                    if let Some(r) = l.strip_prefix("kirycc: ") {
                        say!("{} {t} {r}", p.name);
                    }
                }
            }
            // ahead of trim, which drops on policy rather than on the build having
            // failed, and would otherwise make the two indistinguishable
            if staged(&dest) == 0 {
                return Err(format!("{} {t}: built and staged nothing", p.name));
            }
            // DESTDIR said twice: `make PREFIX="$DESTDIR"/usr` into a Makefile that also
            // honours $(DESTDIR). pciutils installed to /dest/usr on the live root that way
            if dest.join("dest").exists() {
                return Err(format!(
                    "{} {t}: staged under dest/, so DESTDIR went in twice. a PREFIX or \
                     prefix naming $DESTDIR on a make that honours DESTDIR too",
                    p.name
                ));
            }
            // clang drops the counts of a function whose cfg moved and compiles it cold,
            // so an old profile costs speed, never correctness. retraining is hours for
            // some packages, which is why this says so and does nothing. the count the
            // training build itself logged is subtracted: two binaries of one package
            // that each have a main collide by name in a profile and never match
            let missed = match verbose {
                true => None,
                false => Some(
                    fs::read_to_string(&log)
                        .unwrap_or_default()
                        .matches("(hash mismatch)")
                        .count(),
                ),
            };
            let out = src.join(".kiry-profile.profdata");
            if out.is_file() {
                keep_profile(p, &out, &stored, missed)?;
            } else if let (true, Some(n)) = (fed, missed) {
                let base = fs::read_to_string(stored.with_file_name("generated-for"))
                    .ok()
                    .and_then(|g| g.split_whitespace().last()?.parse().ok())
                    .unwrap_or(0);
                if n > base {
                    say!(
                        "{} {t} profile out of date for {} functions, rm -r {} retrains it",
                        p.name,
                        n - base,
                        root.join("var/kiry/profiles").join(&p.name).display()
                    );
                }
            }
            if prune {
                if let Err(e) = prune_pass(root, p, t, &work, &mut c, &log, verbose) {
                    say!("{} {t} not pruned: {e}", p.name);
                }
            }
            if let Err(e) = probe_layouts(p, t, &work, &mut c, &log, verbose) {
                say!("{} {t} no layout recorded: {e}", p.name);
            }
            trim(&dest, t, &p.name)?;
            for (dep, so) in unlinked(root, p, f, t, &dest) {
                say!("{} {t} flag brought in {dep} and nothing links {so}", p.name);
            }
            // after trim, so a gnu binary that gets dropped does not argue for a dep
            let mut linked = BTreeSet::new();
            for (soname, who, kind) in undeclared(root, &members, &deps, t, &dest) {
                say!("{} {t} {kind} {who}  {soname}", p.name);
                linked.insert(who);
            }
            Ok((work, linked.into_iter().collect()))
        }
        Ok(_) if verbose => {
            failed(root, p, t, None, start.elapsed().as_secs());
            Err(format!("{} {t}: build failed", p.name))
        }
        Ok(_) => {
            failed(root, p, t, Some(&log), start.elapsed().as_secs());
            Err(format!("{} {t}: build failed, log is {}", p.name, log.display()))
        }
        Err(e) => Err(format!("{} {t}: {e}", p.name)),
    }
}

// one top directory and nothing else means the build starts inside it, which is
// what every recipe expects
// the abuild helpers a converted body still calls. generated rather than packaged so it
// cannot drift from the binary that writes it
const LIB_SH: &str = "\
default_prepare() {
\t[ -d \"$builddir\" ] && cd \"$builddir\"
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
msg2() { echo \"    $*\"; }
warning() { echo \">>> WARNING: $*\" >&2; }
error() { echo \">>> ERROR: $*\" >&2; }
die() { error \"$@\"; exit 1; }

pkgdir=$DESTDIR

# a tarball's own config.sub is left alone when it already answers to $CHOST -- it can
# be the newer of the two, and cdparanoia 10.2 is the case where it is not
update_config_sub() {
\tfind . -name config.sub | while read -r _f; do
\t\tsh \"$_f\" \"$CHOST\" >/dev/null 2>&1 && continue
\t\tcp /usr/share/kiry/config.sub \"$_f\"
\tdone
}
update_config_guess() { :; }

default_unpack() {
\tfor _s in $source; do
\t\t_f=${_s%%::*}
\t\t_f=${_f##*/}
\t\tcase \"$_f\" in
\t\t*.tar*|*.tgz|*.tbz2|*.txz|*.tzst) tar -C \"$srcdir\" -xf \"$srcdir/$_f\" || return 1 ;;
\t\t*.zip) unzip -qo \"$srcdir/$_f\" -d \"$srcdir\" || return 1 ;;
\t\tesac
\tdone
}

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
// autoconf 2.71 treats as the site script having failed. only for --prefix=/usr: a
// configure nested in a build with a prefix of its own, jemalloc under a cargo build
// script, keeps its defaults or installs into the read-only /usr
const CONFIG_SITE: &str = "\
if test \"$prefix\" = /usr; then
if test \"$libdir\" = '${exec_prefix}/lib'; then libdir=@LIBDIR@; fi
if test \"$includedir\" = '${prefix}/include'; then includedir=@INCLUDEDIR@; fi
if test \"$datarootdir\" = '${prefix}/share'; then datarootdir=@DATADIR@; fi
fi
";

// lld keys an entry on the module, the link's settings and llvm's version string, so
// what comes back is what this link would have made. a gig per package and a month,
// pruned by lld at the end of a link. noatime here, so the month runs from when an
// entry was written rather than from when it was last read
const LTO_CACHE: &str = " --start-no-unused-arguments -Wl,--thinlto-cache-dir=/var/cache/kiry-lto \
-Wl,--thinlto-cache-policy=cache_size_bytes=1g:prune_after=720h --end-no-unused-arguments";

// llvm's version string survives a rebuild of llvm, and a patched one would hand back
// objects its predecessor compiled. the package that ships clang and lld names the
// directory instead, and gc drops the others
fn lto_tag(root: &Path) -> String {
    db::read(root, &sandbox::host(), "llvm")
        .map_or_else(|_| "none".into(), |r| format!("{}-{}", r.version.upstream, r.version.rev))
}

fn lto_cache(root: &Path, name: &str) -> PathBuf {
    root.join("var/kiry/lto").join(name).join(lto_tag(root))
}

// lld says nothing about its cache and a hit leaves no mark on the file, so the dir is
// watched while the build runs: an entry opened that the build did not rename in is a
// hit, one renamed in is a miss. an overflowed queue is no number rather than a wrong one
struct Watch {
    fd: std::sync::Arc<rustix::fd::OwnedFd>,
    wd: i32,
    seen: std::thread::JoinHandle<Option<(usize, usize)>>,
}

fn watch(dir: &Path) -> Option<Watch> {
    use rustix::fs::inotify;
    let fd = std::sync::Arc::new(inotify::init(inotify::CreateFlags::CLOEXEC).ok()?);
    let wd = inotify::add_watch(&*fd, dir, inotify::WatchFlags::OPEN | inotify::WatchFlags::MOVED_TO).ok()?;
    let r = fd.clone();
    let seen = std::thread::spawn(move || {
        let mut buf = [std::mem::MaybeUninit::uninit(); 8192];
        let mut events = inotify::Reader::new(&*r, &mut buf);
        let (mut opened, mut made) = (HashSet::new(), HashSet::new());
        loop {
            let e = match events.next() {
                Ok(e) => e,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return None,
            };
            let f = e.events();
            if f.contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
                return None;
            }
            // removing the watch is how stop() says the build is over
            if f.contains(inotify::ReadFlags::IGNORED) {
                break;
            }
            let Some(n) = e.file_name().and_then(|n| n.to_str().ok()) else { continue };
            if !n.starts_with("llvmcache-") {
                continue;
            }
            match f.contains(inotify::ReadFlags::MOVED_TO) {
                true => made.insert(n.to_string()),
                false => opened.insert(n.to_string()),
            };
        }
        Some((opened.difference(&made).count(), made.len()))
    });
    Some(Watch { fd, wd, seen })
}

impl Watch {
    fn stop(self) -> Option<(usize, usize)> {
        let _ = rustix::fs::inotify::remove_watch(&*self.fd, self.wd);
        self.seen.join().ok().flatten()
    }
}

// the ok row's note, and a line in log/thinlto for stats. green past half, the point
// where the cache hands back more than it had to write
fn cache_rate(root: &Path, p: &Package, t: &str, work: &Path) -> Option<String> {
    let got = fs::read_to_string(work.join("thinlto")).ok()?;
    let mut w = got.split_whitespace().map(|n| n.parse::<usize>().ok());
    let (hits, misses) = (w.next()??, w.next()??);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = mkdirs(&root.join("var/kiry/log"));
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("var/kiry/log/thinlto"))
        .and_then(|mut f| writeln!(f, "{} {t} {hits} {misses} {now}", p.name));
    let pct = hits * 100 / (hits + misses).max(1);
    Some(hue(if pct > 50 { "32" } else { "2" }, &format!("thinlto {pct}%")))
}

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
// a file rather than a shell function so $CC being split on whitespace behaves
//
// a crash is answered here, on the one command it happened to, instead of by a restart.
// the build system never sees it fail, so nothing that already compiled compiles again
// -- a rung through recover() costs a whole build, and for mesa that is hours per rung.
// the rungs are LADDER's, in its order, applied to this command line only. clang exits
// 1 for a crash and for a typo alike, which is why stderr is held back and read
//
// nothing gets written down. a filter line would take -O3 off the whole package for
// one file's sake, and the next build steps the same file down the same way for the
// price of a few failed compiles. what happened is in the log as kirycc: lines, which
// compile() repeats and recover() reads
//
// a kill is the machine running out rather than the compiler, and fewer thinlto jobs
// link the same code, so that one goes again with half and nothing else changes. a
// compile fed on stdin gets one go: there is nothing left to read the second time
//
// under KIRY_PRUNE every shared link is written down as it was run, so prune_pass can
// run it again with a version script once the build is over
//
// clang hands a language it has no frontend for (ada, fortran) to `gcc`, and toollinks
// makes gcc a link to clang, so gdb's configure asking whether the driver knows ada
// forked about 1700 clangs in 25s. -ccc-gcc-name points the handoff at a name nothing
// has, so that compile just fails and configure says no
const CC_WRAPPER: &str = "\
#!/bin/sh
run() { @REAL@ --target=@TRIPLE@ -ccc-gcc-name kiry-no-gcc@SYSROOT@@EXTRA@@LTO@ \"$@\"; }
case \" $* \" in *\" - \"*) run \"$@\"; exit ;; esac
if [ -n \"$KIRY_PRUNE\" ]; then
\tcase \" $* \" in *\" -shared \"*) { printf '%s\\0' \"$PWD\" \"$PATH\" \"${0##*/}\" \"$@\"; printf '\\n'; } >>/src/.kiry-links ;; esac
fi
e=/tmp/.kirycc.$$
trap 'rm -f \"$e\"' EXIT
cc() { run \"$@\" 2>\"$e\"; s=$?; [ -s \"$e\" ] && cat \"$e\" >&2; return $s; }
cc \"$@\" && exit
s=$?
o= p=
for a do [ \"$p\" = -o ] && o=$a; p=$a; done
o=${o:-$p}
if [ $s -eq 137 ] || grep -q -e 'command: Killed' -e 'out of memory' \"$e\"; then
\tfor a do
\t\tshift
\t\tcase $a in -Wl,--thinlto-jobs=*) n=${a#*=}; a=-Wl,--thinlto-jobs=$((n > 1 ? n / 2 : 1)) ;; esac
\t\tset -- \"$@\" \"$a\"
\tdone
\techo \"kirycc: $o was killed, again with half the lto jobs\" >&2
\tcc \"$@\"
\texit
fi
grep -q -e 'PLEASE submit a bug report' -e 'failed due to signal' \"$e\" || exit $s
for r in filter-lto 'OPT -O2' CFLAGS_MARCH strip-flags; do
\twas=$*
\tfor a do
\t\tshift
\t\tcase $r in
\t\tfilter-lto) case $a in -flto*|-Wl,--thinlto-jobs=*) continue ;; esac ;;
\t\t'OPT -O2') case $a in -O3|-O4|-Ofast) a=-O2 ;; esac ;;
\t\tCFLAGS_MARCH) case $a in -march=*) continue ;; esac ;;
\t\tstrip-flags)
\t\t\tcase \" $CFLAGS $CXXFLAGS \" in *\" $a \"*)
\t\t\t\tcase $a in -O*|-march=*|-mtune=*|-mcpu=*|-pipe|-g*) ;; *) continue ;; esac ;;
\t\t\tesac ;;
\t\tesac
\t\tset -- \"$@\" \"$a\"
\tdone
\t[ \"$*\" = \"$was\" ] && continue
\techo \"kirycc: $o crashed clang, again at $r\" >&2
\tcc \"$@\" && exit
\ts=$?
\tgrep -q -e 'PLEASE submit a bug report' -e 'failed due to signal' \"$e\" || exit $s
done
echo \"kirycc: $o crashed clang at every rung\" >&2
exit $s
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
//
// an object library gets no -fPIC from cmake while a shared library beside it does, so
// one translation unit comes out -fPIE. PIE Level is a Max-merged module flag, so under
// thinlto the whole link reads as pie, tls drops to local-exec and lld refuses the
// TPOFF32 in a -shared output -- libobs, one object out of 91. position independent code
// gives a library and an object library -fPIC and an executable -fPIE, which is why this
// is here and not -fPIC in the global CFLAGS, where every executable would pay for it
//
// CTest defaults BUILD_TESTING on and nothing here runs a suite -- want_check is always
// false -- so a cmake recipe that does not say OFF itself compiles its tests for nothing
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
set(CMAKE_POSITION_INDEPENDENT_CODE ON CACHE BOOL \"\" FORCE)
set(BUILD_TESTING OFF CACHE BOOL \"\" FORCE)
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
\t$KIRY_MESON_ARGS \\
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

// how many builds of one level run at once. each is already at -j nproc, so what this
// buys is the serial stretches -- configure, the final link, one slow codegen unit --
// where a single build leaves most of the machine idle. one unless KIRY_PARALLEL says
// more: navi has no memcg and no swap, and two at once froze it during the app campaign
fn width() -> usize {
    match std::env::var("KIRY_PARALLEL").ok().and_then(|v| v.parse().ok()) {
        Some(n) if n > 0 => n,
        _ => 1,
    }
}

// -j is the whole of what bmake and gnu make both read out of here. bmake has no -l at
// all and answers one with its usage line, so lowdown dies whenever anything builds
// beside it -- and samu refuses -l too, it just never reads MAKEFLAGS to find out. what
// -l would do is arithmetic here: the machine divided by the builds on it, which is the
// call the memory cap already makes
fn makeflags(jobs: usize, at_once: usize) -> String {
    format!("-j{}", (jobs / at_once.max(1)).max(1))
}

// f over items, width at a time, started in the order given -- longest first by the time
// a level gets here, which is the first point that ordering changes anything. the first
// failure stops anything new starting and waits out what is already running, because a
// build killed halfway leaves a stage directory behind that nothing claims
fn parallel<T: Sync>(
    items: &[T],
    width: usize,
    f: impl Fn(&T) -> Result<(), String> + Sync,
) -> Result<(), String> {
    let next = AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..width.clamp(1, items.len().max(1)) {
            s.spawn(|| loop {
                if failed.lock().map_or(true, |g| g.is_some()) {
                    return;
                }
                let Some(it) = items.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    return;
                };
                if let Err(e) = f(it) {
                    if let Ok(mut g) = failed.lock() {
                        g.get_or_insert(e);
                    }
                    return;
                }
            });
        }
    });
    match failed.into_inner() {
        Ok(Some(e)) => Err(e),
        _ => Ok(()),
    }
}

fn jobs() -> usize {
    match std::env::var("KIRY_JOBS").ok().and_then(|v| v.parse().ok()) {
        Some(n) if n > 0 => n,
        _ => std::thread::available_parallelism().map_or(1, |n| n.get()),
    }
}

// the shallowest lock of each tree only. a workspace has one at its top, and one below
// it (fuzz/, a vendored crate, a go example) is often stale, which a locked fetch would
// fail the build on
fn shallowest(dir: &Path, name: &str, depth: usize) -> Vec<PathBuf> {
    let here = dir.join(name);
    if here.is_file() {
        return vec![here];
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if depth > 0 && e.file_type().is_ok_and(|k| k.is_dir()) {
            out.extend(shallowest(&e.path(), name, depth - 1));
        }
    }
    out
}

fn edits_lock(patch: &Path) -> bool {
    let Ok(text) = fs::read_to_string(patch) else {
        return false;
    };
    text.lines().filter_map(|l| l.strip_prefix("+++ ")).any(|l| {
        let f = l.split_whitespace().next().unwrap_or("");
        ["Cargo.lock", "Cargo.toml", "go.sum", "go.mod"].iter().any(|n| f.ends_with(n))
    })
}

// the tool as a word of its own, so CARGO_HOME and cargo-home are not cargo
fn names(text: &str, tool: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "_-./$".contains(c))).any(|w| w == tool)
}

// a fetch the sandbox cannot do, run out here. its last line of stderr is the error.
// a patch that edits the lock changes what there is to fetch, and default_prepare only
// applies it inside the sandbox, so it goes on for the fetch and comes back off after,
// leaving prepare the tree it expects. one that will not apply here is skipped and the
// build fails offline the way it would have
fn prefetch(c: &mut Command, lock: &Path, patches: &[PathBuf]) -> Result<(), String> {
    let what = c.get_program().to_string_lossy().into_owned();
    let dir = lock.parent().unwrap_or(lock);
    let patch = |p: &Path, flag: Option<&str>| {
        Command::new("patch")
            .args(["-p1", "-s", "--no-backup-if-mismatch", "-d"])
            .arg(dir)
            .arg("-i")
            .arg(p)
            .args(flag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let on: Vec<&PathBuf> =
        patches.iter().filter(|p| patch(p, Some("--dry-run")) && patch(p, None)).collect();
    let o = c.stdin(Stdio::null()).output();
    for p in on.iter().rev() {
        if !patch(p, Some("-R")) {
            return Err(format!("{}: would not come back off {}", p.display(), dir.display()));
        }
    }
    let o = o.map_err(|e| format!("{what} for {}: {e}", lock.display()))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    Err(format!("{what} for {}: {last}", lock.display()))
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

// a build system handed an optional component it cannot satisfy configures, compiles
// nothing and exits 0. qt marks Gui optional, so qt_build_repo() can stage nothing for
// qt6-qtwayland and the line says ok in two seconds -- the tar that follows is a valid
// archive of nothing and the install a no-op nobody hears about
fn staged(dest: &Path) -> usize {
    let Ok(rd) = fs::read_dir(dest) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        // a symlink to a directory is content, not somewhere to descend
        match e.file_type() {
            Ok(k) if k.is_dir() => n += staged(&e.path()),
            Ok(_) => n += 1,
            Err(_) => {}
        }
    }
    n
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

    let mut c = sandbox::tied(&mut Command::new("tar"))
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

    let z = sandbox::tied(&mut Command::new("zstd"))
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

fn meta(
    root: &Path,
    p: &Package,
    t: &str,
    hash: &str,
    f: &Flags,
    art: &Path,
    linked: &[String],
) -> Result<(), String> {
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
    // the profile at its name now is the one this build used, fed or just trained
    put(&d.join("profile"), &format!("{}\n", profile_sum(&profile(root, p, t))))?;

    // one line per dep that applies to this target, the same narrowing targets gets --
    // an artifact is built for one target and carries what that build actually used
    //
    // a converted recipe declares what alpine's makedepends said and nothing about what
    // the result links, because alpine derives that from sonames when it packages. so
    // does this, at the one moment it is known: a library in DT_NEEDED that the recipe
    // cannot account for is a runtime edge whether anybody wrote it down or not. the
    // recipe stays what somebody typed and the sidecar carries what was true
    //
    // that includes a dep the recipe calls build-only. makedepends names the -dev half
    // of what gets linked, so imlib2 says libpng make and links libpng16.so.16, and a
    // record that kept the make would plan a cached imlib2 without libpng
    let mut deps = String::new();
    let mut said: HashSet<&str> = HashSet::new();
    for x in p.depends.iter().filter(|x| x.applies(t)) {
        if x.make && linked.contains(&x.name) {
            let run = Dep {
                make: false,
                host: false,
                ..x.clone()
            };
            deps.push_str(&format!("{run}\n"));
        } else {
            deps.push_str(&format!("{x}\n"));
        }
        said.insert(&x.name);
    }
    for n in linked.iter().filter(|n| !said.contains(n.as_str())) {
        deps.push_str(&format!("{n}\n"));
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
    // gentoo acts on a build only through flags someone set, and those are in the flags
    // record already. hashed, the first sync that fetches entries would have made two
    // thousand cached artifacts read as changed recipes
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n != "gentoo")
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
    flags_with(root, name, dir, None)
}

// the same resolve with one ladder rung on top as though it had been written, so whether
// a rung changes anything is known before a build is spent on it and nothing touches disk
fn flags_with(
    root: &Path,
    name: &str,
    dir: Option<&Path>,
    rung: Option<&(bool, &str)>,
) -> Result<Flags, String> {
    let entry = |l: &str| {
        let (k, v) = l.split_once(' ').unwrap_or((l, ""));
        ("ladder".to_string(), k.to_string(), v.to_string())
    };
    let mut from = settings(root, name)?;
    if let Some((false, l)) = rung {
        from.push(entry(l));
    }
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
            // without restating it. kept as -name rather than dropped: off is a choice,
            // and it is the one that takes a dependency out of the closure
            "flags" => {
                for t in v.split_whitespace() {
                    let (on, n) = match t.strip_prefix('-') {
                        Some(n) => (false, n),
                        None => (true, t.strip_prefix('+').unwrap_or(t)),
                    };
                    use_flags.retain(|x| x.trim_start_matches('-') != n);
                    use_flags.push(if on { n.to_string() } else { format!("-{n}") });
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

    if let Some(d) = dir {
        let mut f = filters(d)?;
        if let Some((true, l)) = rung {
            f.push(entry(l));
        }
        filtered(&f, &mut cf, &mut cxf, &mut ldf)?;
        from.extend(f);
    }

    // a flag means something only where this version's gentoo entry declares it, the way
    // portage reads USE against IUSE. without that a global -gtk would change the record
    // of every package installed and queue all of them for a rebuild that changes nothing
    let declared: HashSet<String> = dir
        .and_then(gentoo)
        .map(|g| g.iuse.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default();
    use_flags.retain(|f| declared.contains(f.trim_start_matches('-')));
    use_flags.sort();
    Ok(Flags {
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

use sandbox::PRUNED;

// a package with more headers than this gets no fingerprint rather than a build that
// spends its last ten minutes parsing boost one header at a time
const MOST_HEADERS: usize = 2000;

// the compile-time half of an abi, which no elf shows: a struct in a public header
// grows, the library rebuilds consistently, its exports do not move, and every consumer
// compiled against the old header passes a wrong-sized struct. clang is the oracle --
// each installed header compiled alone, since no package's set compiles as one tu and
// about one in seven does not compile alone either, which is counted and said
fn probe_layouts(
    p: &Package,
    t: &str,
    work: &Path,
    c: &mut Command,
    log: &Path,
    verbose: bool,
) -> Result<(), String> {
    let inc = format!("/dest{}", incdir(t));
    if !work.join("dest").join(incdir(t).trim_start_matches('/')).is_dir() {
        return Ok(());
    }
    let script = LAYOUT_PROBE.replace("@INC@", &inc).replace("@MOST@", &MOST_HEADERS.to_string());
    fs::write(work.join("sysroot/build"), script).map_err(|e| format!("probe script: {e}"))?;
    c.env("KIRY_PRUNE", "");
    if !verbose {
        let out = fs::OpenOptions::new()
            .append(true)
            .open(log)
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| format!("{}: {e}", log.display()))?;
        c.stdout(out).stderr(err);
    }
    run(c, "the header probes")?;

    let out = work.join("src/.kiry-layout");
    let list = fs::read_to_string(out.join("list")).unwrap_or_default();
    let heads: Vec<&str> = list.lines().collect();
    if heads.is_empty() {
        return Ok(());
    }
    if heads.len() > MOST_HEADERS {
        return Err(format!("{} headers is more than {MOST_HEADERS} to probe", heads.len()));
    }
    let own = work.join("dest").join(incdir(t).trim_start_matches('/'));
    let mut ids: HashSet<String> = HashSet::new();
    for h in &heads {
        let text = fs::read_to_string(own.join(h)).unwrap_or_default();
        ids.extend(text.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).map(String::from));
    }
    let mut all: BTreeMap<String, String> = BTreeMap::new();
    let mut bad = 0;
    for h in &heads {
        let f = h.replace('/', "%");
        if !out.join(format!("{f}.ok")).is_file() {
            bad += 1;
            continue;
        }
        let dump = fs::read_to_string(out.join(format!("{f}.dump"))).unwrap_or_default();
        for (k, v) in records(&dump, &ids) {
            all.entry(k).or_insert(v);
        }
    }
    let body: String = all.iter().map(|(k, v)| format!("{v} {k}\n")).collect();
    fs::write(work.join("layout"), body).map_err(|e| format!("layout: {e}"))?;
    say!(
        "{} {t} layout {} records from {} headers, {bad} would not compile alone",
        p.name,
        all.len(),
        heads.len() - bad
    );
    Ok(())
}

// each installed header compiled alone, in parallel, c first and c++ when that fails.
// every first-level include directory goes on the path because a header includes its
// siblings the way its .pc says to, and the package is not installed in its own sysroot
const LAYOUT_PROBE: &str = "\
inc=@INC@
out=/src/.kiry-layout
mkdir -p \"$out\"
cd \"$inc\"
flags=\"-I$inc\"
for d in */; do
\t[ -d \"$d\" ] && flags=\"$flags -I$inc/${d%/}\"
done
for pc in /dest/usr/lib/pkgconfig/*.pc /dest/usr/lib64/pkgconfig/*.pc /dest/usr/share/pkgconfig/*.pc; do
\t[ -f \"$pc\" ] || continue
\tn=${pc##*/}
\tflags=\"$flags $(PKG_CONFIG_PATH=${pc%/*} pkgconf --cflags \"${n%.pc}\" 2>/dev/null)\"
done
export inc flags out
find . -type f \\( -name '*.h' -o -name '*.hh' -o -name '*.hpp' -o -name '*.hxx' \\) | sed 's#^\\./##' | sort >\"$out/list\"
[ \"$(wc -l <\"$out/list\")\" -le @MOST@ ] || exit 0
xargs -P \"$(nproc)\" -n 1 sh -c '
h=$1
f=$(printf %s \"$h\" | tr / %)
case $h in *.hh|*.hpp|*.hxx) xs=c++ ;; *) xs=\"c c++\" ;; esac
for x in $xs; do
\tcc=kirycc
\t[ $x = c++ ] && cc=kiryc++
\tif $cc -x $x -fsyntax-only -Xclang -fdump-record-layouts-complete $flags -include \"$inc/$h\" /dev/null >\"$out/$f.dump\" 2>\"$out/$f.err\"; then
\t\techo $x >\"$out/$f.ok\"
\t\texit 0
\tfi
done
exit 0' _ <\"$out/list\"
";

// clang's layout dump as record -> hash, for the records this package's headers name.
// a named record counts when its name is a word in one of them -- struct timespec is
// musl's, and only matters to a package whose headers mention it. an anonymous record
// counts when clang places it in this package's tree, and is keyed by the file that
// declares it and its order there, because its own name is a file:line:col that a
// comment would move
fn records(dump: &str, ids: &HashSet<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut anon: HashMap<String, usize> = HashMap::new();
    for block in dump.split("*** Dumping AST Record Layout").skip(1) {
        let block = block.split("\n\n").next().unwrap_or(block);
        let Some(first) = block.lines().find_map(|l| l.split_once('|').map(|(_, r)| r.trim())) else {
            continue;
        };
        let key = match first.find(" at /") {
            Some(i) => {
                let Some(at) = first[i + 4..].strip_prefix("/dest") else { continue };
                let file = at.split(':').next().unwrap_or(at).to_string();
                let n = anon.entry(file.clone()).or_default();
                *n += 1;
                format!("{file}#{n}")
            }
            None => {
                let last = first.rsplit([' ', ':']).next().unwrap_or(first);
                let word = last.split('<').next().unwrap_or(last);
                if !ids.contains(word) {
                    continue;
                }
                first.to_string()
            }
        };
        let hash = kiry_core::sha256(unplaced(block).as_bytes()).unwrap_or_default();
        out.push((key, hash[..16.min(hash.len())].to_string()));
    }
    out
}

// every " at /path:line:col" taken out of a block, which is where a comment moving a
// line would otherwise show up as a layout change
fn unplaced(block: &str) -> String {
    let mut out = String::new();
    let mut rest = block;
    while let Some(i) = rest.find(" at /") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 4..];
        let end = tail.find([')', '\n']).unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

// the records whose layout moved between what is installed and what is about to be,
// read off the two sidecars. no fingerprint on either side is no answer, not a change
fn layouts_moved(root: &Path, jobs: &[install::Job]) -> Vec<(String, String, Vec<String>)> {
    let read = |a: &Path| -> Option<BTreeMap<String, String>> {
        let mut d = a.as_os_str().to_owned();
        d.push(".meta/layout");
        let text = fs::read_to_string(PathBuf::from(d)).ok()?;
        Some(
            text.lines()
                .filter_map(|l| l.split_once(' ').map(|(h, k)| (k.to_string(), h.to_string())))
                .collect(),
        )
    };
    let mut out = Vec::new();
    for j in jobs {
        let Ok(rec) = db::read(root, &j.target, &j.name) else { continue };
        let old = root.join("var/kiry/cache").join(format!(
            "{}-{}-{}.{}.tar.zst",
            j.name, rec.version.upstream, rec.version.rev, j.target
        ));
        let (Some(was), Some(now)) = (read(&old), read(&j.archive)) else { continue };
        let moved: Vec<String> = was
            .iter()
            .filter(|(k, h)| now.get(*k) != Some(*h))
            .map(|(k, _)| k.clone())
            .collect();
        if !moved.is_empty() {
            out.push((j.target.clone(), j.name.clone(), moved));
        }
    }
    out
}

// everything installed that builds against a package whose layouts moved, queued the way
// a soname break queues its consumers. what this batch just built against the new
// headers is left out -- it already has them
fn queue_layouts(root: &Path, moved: &[(String, String, Vec<String>)], just: &HashSet<(String, String)>) {
    if moved.is_empty() {
        return;
    }
    let mut rows = db::read_queue(root).unwrap_or_default();
    for (t, name, recs) in moved {
        let mut who = Vec::new();
        for q in db::installed(root, t).unwrap_or_default() {
            if q == *name || just.contains(&(t.clone(), q.clone())) {
                continue;
            }
            let Ok(rec) = db::read(root, t, &q) else { continue };
            if rec.depends.iter().any(|d| d.name == *name) {
                rows.push(db::Queued {
                    target: t.clone(),
                    name: q.clone(),
                    soname: "layout".into(),
                    changed: recs.clone(),
                });
                who.push(q);
            }
        }
        let shown: Vec<&str> = recs.iter().take(3).map(String::as_str).collect();
        say!(
            "{name} {t} layout moved for {} records ({}{}), queued {} that build against it",
            recs.len(),
            shown.join(", "),
            if recs.len() > 3 { ", ..." } else { "" },
            who.len()
        );
    }
    rows.sort();
    rows.dedup();
    let _ = db::write_queue(root, &rows);
}

// what a binary imports when it can reach a library by name at runtime
const DL: [&str; 4] = ["dlopen", "dlsym", "dlvsym", "dlmopen"];

// what a pruned library has to go on exporting: every symbol something installed asks it
// for, this build's own files included. a package installed since the last prune that
// wanted more is installed by the time the next one runs, so it is counted then -- the
// queue row skew writes is what gets that prune run. an Err is why pruning is not safe
// to decide here, which leaves the library as it linked
//
// 11a is the gate. dlsym resolves a name at runtime and no index sees it, so a library
// in the image of anything that can dlopen -- imports one of DL, or is itself loaded
// that way because nothing links it -- is not this function's to prune
fn keep_list(
    root: &Path,
    p: &Package,
    t: &str,
    dest: &Path,
    ours: &HashSet<String>,
) -> Result<BTreeSet<String>, String> {
    let was = db::read_provides(root, t, &p.name).unwrap_or_default();
    if was.is_empty() {
        return Err("not installed yet, so nothing says what uses it".into());
    }
    // an anonymous version tag is scope and nothing else, and lld refuses one beside named
    // versions. a library that versions its symbols has already said what its api is
    if was.iter().any(|x| x.versioned) {
        return Err("it versions its symbols, and an anonymous version script cannot sit beside them".into());
    }

    let mut elves: Vec<(String, elf::Elf)> = Vec::new();
    let mut files = Vec::new();
    under(dest, &mut files);
    for f in files.iter().filter(|f| !f.to_string_lossy().contains(PRUNED)) {
        if let Ok(o) = elf::read(f) {
            elves.push((f.display().to_string(), o));
        }
    }
    let others: Vec<String> = db::installed(root, t)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| *n != p.name)
        .collect();
    for (rec, seen) in scans(root, t, &others).into_iter().flatten() {
        for (path, what) in seen {
            if let install::Seen::Elf(o) = what {
                elves.push((format!("{} {path}", rec.name), o));
            }
        }
    }

    let mut by: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, o)) in elves.iter().enumerate() {
        if let Some(s) = &o.soname {
            by.entry(s.as_str()).or_default().push(i);
        }
    }
    let needs = |i: usize| -> Vec<usize> {
        elves[i].1.needed.iter().flat_map(|n| by.get(n.as_str()).cloned().unwrap_or_default()).collect()
    };
    let linked: HashSet<usize> = (0..elves.len()).flat_map(needs).collect();
    for c in 0..elves.len() {
        let o = &elves[c].1;
        let can = o.undefined.iter().any(|u| DL.contains(&u.name.as_str())) || (!o.interp && !linked.contains(&c));
        if !can {
            continue;
        }
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stack = vec![c];
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            if let Some(s) = elves[i].1.soname.as_ref().filter(|s| ours.contains(*s)) {
                return Err(format!("{} can dlopen, and {s} is in what it loads", elves[c].0));
            }
            stack.extend(needs(i));
        }
    }

    Ok(elves
        .iter()
        .filter(|(_, o)| o.needed.iter().any(|n| ours.contains(n)))
        .flat_map(|(_, o)| o.undefined.iter().map(|u| u.name.clone()))
        .collect())
}

// the libraries of a symbol-prune package linked a second time, with only the exports
// something asks for, so lto's internalize can delete the rest. the links the wrapper
// wrote down are run again as they were, in the same sandbox, with -o moved and a
// version script added. the first link ships under PRUNED beside the second
fn prune_pass(
    root: &Path,
    p: &Package,
    t: &str,
    work: &Path,
    c: &mut Command,
    log: &Path,
    verbose: bool,
) -> Result<(), String> {
    let (src, dest) = (work.join("src"), work.join("dest"));
    let mut files = Vec::new();
    under(&dest, &mut files);
    let libs: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|f| {
            let o = elf::read(&f).ok()?;
            (!o.interp).then_some(())?;
            Some((f, o.soname?))
        })
        .collect();
    let ours: HashSet<String> = libs.iter().map(|(_, s)| s.clone()).collect();
    if ours.is_empty() {
        return Err("it built no shared library".into());
    }
    let keep = keep_list(root, p, t, &dest, &ours)?;

    let mut vs = String::from("{\n  global:\n");
    for k in &keep {
        vs.push_str(&format!("    \"{k}\";\n"));
    }
    vs.push_str("  local: *;\n};\n");
    fs::write(src.join(".kiry-keep"), vs).map_err(|e| format!(".kiry-keep: {e}"))?;

    // the last link of each output wins: libtool links again at install time
    let links = fs::read(src.join(".kiry-links")).map_err(|_| "no shared link was written down")?;
    let mut by_out: HashMap<String, Vec<String>> = HashMap::new();
    for rec in links.split(|b| *b == b'\n').filter(|r| !r.is_empty()) {
        let f: Vec<String> = rec
            .split(|b| *b == 0)
            .map(|x| String::from_utf8_lossy(x).into_owned())
            .filter(|x| !x.is_empty())
            .collect();
        let Some(i) = f.iter().skip(3).position(|a| a == "-o").map(|i| i + 3) else { continue };
        let Some(out) = f.get(i + 1).and_then(|o| o.rsplit('/').next()) else { continue };
        by_out.insert(out.to_string(), f);
    }

    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut script = String::from("mkdir -p /src/.kiry-pruned\n");
    let mut todo = Vec::new();
    for (lib, _) in &libs {
        let base = lib.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let Some(f) = by_out.get(&base) else {
            say!("{} {t} no link of {base} was written down, left whole", p.name);
            continue;
        };
        // the PATH it ran under too: a recipe that puts its own tools first is running a
        // different link otherwise
        let (cwd, path, driver, argv) = (&f[0], &f[1], &f[2], &f[3..]);
        let mut args: Vec<String> = Vec::new();
        let mut it = argv.iter();
        while let Some(a) = it.next() {
            if a == "-o" {
                it.next();
                args.push("-o".into());
                args.push(format!("/src/.kiry-pruned/{base}"));
            } else {
                args.push(a.clone());
            }
        }
        args.push("-Wl,--version-script=/src/.kiry-keep".into());
        let line: Vec<String> = args.iter().map(|a| q(a)).collect();
        script.push_str(&format!(
            "(cd {} && PATH={} {} {}) || echo 'kirycc: {base} did not link pruned'\n",
            q(cwd),
            q(path),
            q(driver),
            line.join(" ")
        ));
        todo.push((lib.clone(), base));
    }
    if todo.is_empty() {
        return Err("nothing to link again".into());
    }
    fs::write(work.join("sysroot/build"), script).map_err(|e| format!("prune script: {e}"))?;
    c.env("KIRY_PRUNE", "");
    if !verbose {
        let out = fs::OpenOptions::new()
            .append(true)
            .open(log)
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| format!("{}: {e}", log.display()))?;
        c.stdout(out).stderr(err);
    }
    run(c, "the pruned links")?;

    for (lib, base) in todo {
        let pruned = src.join(".kiry-pruned").join(&base);
        let Ok(small) = elf::read(&pruned) else {
            say!("{} {t} {base} left whole, its pruned link failed and the log says why", p.name);
            continue;
        };
        let Ok(big) = elf::read(&lib) else { continue };
        let full = dest.join(PRUNED).join(&base);
        mkdirs(full.parent().unwrap_or(&dest))?;
        fs::rename(&lib, &full).map_err(|e| format!("{}: {e}", full.display()))?;
        fs::copy(&pruned, &lib).map_err(|e| format!("{}: {e}", lib.display()))?;
        let size = |f: &Path| fs::metadata(f).map_or(0, |m| m.len()) / 1024;
        say!(
            "{} {t} pruned {base} {} exports -> {}, {}K -> {}K",
            p.name,
            big.exports.len(),
            small.exports.len(),
            size(&full),
            size(&lib)
        );
    }
    Ok(())
}

// a flag turned on is a claim that the build uses what it brought in, and DT_NEEDED is
// where that shows. a library the flag added that nothing links is a flag configure
// never looked at -- the negative direction needs no check, the sandbox enforces it
fn unlinked(root: &Path, p: &Package, f: &Flags, t: &str, dest: &Path) -> Vec<(String, String)> {
    let Some(g) = gentoo(&p.dir) else { return Vec::new() };
    let set = chosen(&f.use_flags);
    if !set.values().any(|on| *on) {
        return Vec::new();
    }
    let brought: Vec<String> = g
        .wanted(&set)
        .into_iter()
        .filter(|(_, n)| *n != Needs::Tool)
        .filter_map(|(a, _)| recipe_for(root, &a))
        .collect();
    let mut files = Vec::new();
    under(dest, &mut files);
    let needed: HashSet<String> = files
        .iter()
        .filter_map(|x| elf::read(x).ok())
        .flat_map(|o| o.needed)
        .collect();
    let mut out = Vec::new();
    for dep in brought {
        let Ok(ps) = db::read_provides(root, t, &dep) else { continue };
        if ps.is_empty() || ps.iter().any(|x| needed.contains(&x.soname)) {
            continue;
        }
        if !out.iter().any(|(d, _): &(String, String)| *d == dep) {
            out.push((dep, ps[0].soname.clone()));
        }
    }
    out
}

// what the gentoo entry is, and what the chosen flags do to the closure through it
fn gentoo_says(root: &Path, d: &Path, f: &Flags) {
    let Ok(text) = fs::read_to_string(d.join("gentoo")) else { return };
    let head = text.lines().next().and_then(|l| l.strip_prefix("# ")).unwrap_or("?").trim();
    let Some(g) = gentoo(d) else {
        say!("gentoo {head} is for another version, so no flags");
        return;
    };
    say!("gentoo {head}");
    let set = chosen(&f.use_flags);
    let Ok(p) = read_recipe(d) else { return };
    let host = sandbox::host();
    let have: Vec<String> = p
        .depends
        .iter()
        .map(|x| called(root, &x.name, &host).unwrap_or_else(|| x.name.clone()))
        .collect();
    for n in &have {
        if g.unwanted(&set, n) {
            say!("drops {n}");
        }
    }
    let mut added: Vec<String> = Vec::new();
    for (a, _) in g.wanted(&set) {
        match recipe_for(root, &a) {
            Some(n) if !have.contains(&n) && !added.contains(&n) && n != p.name => {
                say!("adds {n}");
                added.push(n);
            }
            Some(_) => {}
            None => say!("wants {a}, and no recipe answers to it"),
        }
    }
}

// which flags a config chose for one package, as name -> on. a flag nobody set is
// absent rather than defaulted: the recipe's depends are already alpine's choice, and
// only a flag someone chose moves the closure off it
fn chosen(use_flags: &[String]) -> HashMap<&str, bool> {
    use_flags
        .iter()
        .map(|f| match f.strip_prefix('-') {
            Some(n) => (n, false),
            None => (f.as_str(), true),
        })
        .collect()
}

// where a gentoo dependency sits, which is what kind of edge it becomes here
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Needs {
    Run,
    Build,
    Tool,
}

struct Gentoo {
    // declared flags, and whether each is on by default
    iuse: Vec<(String, bool)>,
    // every conditional atom, with the flags it sits under and the state each one wants
    conds: Vec<(Vec<(String, bool)>, String, Needs)>,
    // atoms under no condition at all, which no flag can take out
    plain: Vec<String>,
}

// the md5-cache entry for exactly the version the recipe builds, verbatim under a
// header naming which one it is. a header for any other version is a file a bump carried
// across, and it counts as no file at all -- absence over approximation, until sync
// fetches the right one
fn gentoo(dir: &Path) -> Option<Gentoo> {
    let text = fs::read_to_string(dir.join("gentoo")).ok()?;
    let ver = fs::read_to_string(dir.join("version")).ok()?;
    let head = text.lines().next()?.strip_prefix("# ")?.trim();
    let (_, pv) = pf(head.rsplit('/').next()?)?;
    if Some(pv) != ver.split_whitespace().next() {
        return None;
    }
    let mut g = Gentoo {
        iuse: Vec::new(),
        conds: Vec::new(),
        plain: Vec::new(),
    };
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        match k {
            "IUSE" => {
                g.iuse = v
                    .split_whitespace()
                    .map(|f| match f.strip_prefix('+') {
                        Some(n) => (n.to_string(), true),
                        None => (f.trim_start_matches('-').to_string(), false),
                    })
                    .collect();
            }
            "RDEPEND" | "PDEPEND" => spec(v, Needs::Run, &mut g),
            "DEPEND" => spec(v, Needs::Build, &mut g),
            "BDEPEND" | "IDEPEND" => spec(v, Needs::Tool, &mut g),
            _ => {}
        }
    }
    Some(g)
}

// a gentoo PF split into name and version: mesa-26.2.3-r1 is mesa and 26.2.3. a name may
// not end in anything that reads as a version, so the last hyphen before a digit is it
fn pf(s: &str) -> Option<(&str, &str)> {
    let base = match s.rsplit_once("-r") {
        Some((b, r)) if !r.is_empty() && r.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => s,
    };
    let at = base
        .match_indices('-')
        .map(|(i, _)| i)
        .filter(|&i| base[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .last()?;
    Some((&base[..at], &base[at + 1..]))
}

// as much of DEPEND's grammar as a flag needs. flag? and !flag? open a group that only
// counts under that flag, || ( ) and a bare ( ) are groups with no condition, and a
// blocker is not a dependency
fn spec(v: &str, needs: Needs, g: &mut Gentoo) {
    let mut stack: Vec<Option<(String, bool)>> = Vec::new();
    let mut next: Option<(String, bool)> = None;
    for t in v.split_whitespace() {
        match t {
            "(" => stack.push(next.take()),
            ")" => {
                stack.pop();
            }
            "||" => {}
            _ if t.ends_with('?') => {
                let f = &t[..t.len() - 1];
                next = Some(match f.strip_prefix('!') {
                    Some(n) => (n.to_string(), false),
                    None => (f.to_string(), true),
                });
            }
            _ => {
                let Some(a) = atom(t) else { continue };
                let under: Vec<(String, bool)> = stack.iter().flatten().cloned().collect();
                match under.is_empty() {
                    true => g.plain.push(a),
                    false => g.conds.push((under, a, needs)),
                }
            }
        }
    }
}

// category/name out of an atom: >=x11-libs/libdrm-2.4.133[abi_x86_32(-)?]:0= is
// x11-libs/libdrm
fn atom(t: &str) -> Option<String> {
    if t.starts_with('!') {
        return None;
    }
    let t = t.split(['[', ':']).next()?;
    let bare = t.trim_start_matches(['>', '<', '=', '~']);
    let name = match bare.len() == t.len() {
        true => bare,
        false => pf(bare.trim_end_matches('*'))?.0,
    };
    name.contains('/').then(|| name.to_string())
}

// gentoo's name and this tree's for one package agree once case and alpine's prefixes
// are set aside: x11-libs/libX11 is libx11, media-libs/libsdl2 is sdl2, dev-qt/qtbase is
// qt6-qtbase, dev-lang/lua is lua5.4
fn same(atom: &str, name: &str) -> bool {
    let a = atom.rsplit('/').next().unwrap_or(atom).to_lowercase();
    let b = name.to_lowercase();
    let b = match b.split_once('-') {
        Some((p, rest)) if matches!(p, "py3" | "perl" | "qt5" | "qt6" | "kf5" | "kf6") => rest.to_string(),
        _ => b,
    };
    let bare = b.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    a == b
        || a.strip_prefix("lib") == Some(b.as_str())
        || b.strip_prefix("lib") == Some(a.as_str())
        || (bare != b && bare == a)
}

// the recipe a gentoo atom names, for a flag turned on that asks for something the
// recipe never listed
fn recipe_for(root: &Path, atom: &str) -> Option<String> {
    let (cat, pn) = atom.split_once('/')?;
    let pn = pn.to_lowercase();
    let mut names = vec![pn.clone(), format!("lib{pn}")];
    if let Some(s) = pn.strip_prefix("lib") {
        names.push(s.to_string());
    }
    let prefix = match cat {
        "dev-python" => Some("py3"),
        "dev-perl" => Some("perl"),
        "dev-qt" => Some("qt6"),
        _ => None,
    };
    if let Some(x) = prefix {
        names.insert(0, format!("{x}-{pn}"));
    }
    names
        .into_iter()
        .map(|n| called(root, &n, &sandbox::host()).unwrap_or(n))
        .find(|n| recipe(root, n).is_some())
}

// a flag's meson name is the option of the same name, and its value is the option's
// type: a feature is enabled or disabled, a boolean true or false. a combo only where it
// has a word for the state asked -- which of mesa's glx=dri|xlib on should mean is not
// something a flag can say. auto_features stays auto either way; this is only for what
// someone chose
fn meson_args(src: &Path, use_flags: &[String]) -> String {
    let set = chosen(use_flags);
    if set.is_empty() {
        return String::new();
    }
    // newer projects say meson.options and mesa has already moved, so looking only for
    // the old name finds nothing and says nothing
    let mut dirs = vec![src.to_path_buf()];
    let mut tops: Vec<PathBuf> = fs::read_dir(src)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    tops.sort();
    dirs.extend(tops);
    let Some(text) = dirs
        .iter()
        .flat_map(|d| [d.join("meson.options"), d.join("meson_options.txt")])
        .find_map(|f| fs::read_to_string(f).ok())
    else {
        return String::new();
    };
    let mut out = Vec::new();
    for (name, kind, choices) in options(&text) {
        let Some(&on) = set.get(name.as_str()) else { continue };
        let has = |w: &[&'static str]| w.iter().copied().find(|c| choices.iter().any(|x| x == c));
        let v = match (kind.as_str(), on) {
            ("feature", true) => "enabled",
            ("feature", false) => "disabled",
            ("boolean", true) => "true",
            ("boolean", false) => "false",
            ("combo", true) => match has(&["enabled", "true"]) {
                Some(v) => v,
                None => continue,
            },
            ("combo", false) => match has(&["disabled", "false", "none"]) {
                Some(v) => v,
                None => continue,
            },
            _ => continue,
        };
        out.push(format!("-D{name}={v}"));
    }
    out.join(" ")
}

// meson's option() calls as (name, type, choices). tokens rather than lines, because an
// option spans several lines as often as not and a description can hold a quote or a #
fn options(text: &str) -> Vec<(String, String, Vec<String>)> {
    enum T {
        S(String),
        W(String),
        P(char),
    }
    let mut toks = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '#' => {
                while it.peek().is_some_and(|&n| n != '\n') {
                    it.next();
                }
            }
            '\'' => {
                let mut s = String::new();
                // a ''' string runs to the next ''' and may hold a lone quote
                let triple = it.peek() == Some(&'\'') && {
                    let mut look = it.clone();
                    look.next();
                    look.peek() == Some(&'\'')
                };
                if triple {
                    it.next();
                    it.next();
                }
                while let Some(n) = it.next() {
                    match n {
                        '\\' => {
                            if let Some(e) = it.next() {
                                s.push(e);
                            }
                        }
                        '\'' if !triple => break,
                        '\'' if it.peek() == Some(&'\'') => {
                            let mut look = it.clone();
                            look.next();
                            if look.peek() == Some(&'\'') {
                                it.next();
                                it.next();
                                break;
                            }
                            s.push(n);
                        }
                        _ => s.push(n),
                    }
                }
                toks.push(T::S(s));
            }
            c if c.is_alphanumeric() || c == '_' => {
                let mut w = c.to_string();
                while let Some(&n) = it.peek().filter(|n| n.is_alphanumeric() || **n == '_') {
                    w.push(n);
                    it.next();
                }
                toks.push(T::W(w));
            }
            c if c.is_whitespace() => {}
            c => toks.push(T::P(c)),
        }
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if !matches!((&toks[i], toks.get(i + 1)), (T::W(w), Some(T::P('('))) if w == "option") {
            i += 1;
            continue;
        }
        i += 2;
        let (mut name, mut kind, mut choices) = (None, String::new(), Vec::new());
        let mut depth = 1;
        while i < toks.len() && depth > 0 {
            match &toks[i] {
                T::P('(') => depth += 1,
                T::P(')') => depth -= 1,
                T::S(s) if name.is_none() => name = Some(s.clone()),
                T::W(w) if w == "type" => {
                    if let Some(T::S(t)) = toks.get(i + 2) {
                        kind = t.clone();
                    }
                }
                T::W(w) if w == "choices" => {
                    let mut j = i + 3;
                    while let Some(T::S(c)) = toks.get(j) {
                        choices.push(c.clone());
                        j += 2;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if let Some(n) = name {
            out.push((n, kind, choices));
        }
    }
    out
}

impl Gentoo {
    // a flag turned off takes out a dependency only when every place gentoo names it is
    // under a flag the config has against it. one occurrence still standing keeps it,
    // and a name gentoo never mentions is alpine's choice and not a flag's to undo
    fn unwanted(&self, set: &HashMap<&str, bool>, name: &str) -> bool {
        if self.plain.iter().any(|a| same(a, name)) {
            return false;
        }
        let mut seen = false;
        for (under, a, _) in &self.conds {
            if !same(a, name) {
                continue;
            }
            seen = true;
            if !under.iter().any(|(f, w)| set.get(f.as_str()) == Some(&!w)) {
                return false;
            }
        }
        seen
    }

    // atoms whose every condition holds, where at least one of them holds because a
    // config said so. a flag's default only fills in the rest of a nested condition
    fn wanted(&self, set: &HashMap<&str, bool>) -> Vec<(String, Needs)> {
        let default = |f: &str| self.iuse.iter().any(|(n, on)| n == f && *on);
        let mut out: Vec<(String, Needs)> = self
            .conds
            .iter()
            .filter(|(under, _, _)| {
                under.iter().any(|(f, _)| set.contains_key(f.as_str()))
                    && under
                        .iter()
                        .all(|(f, w)| set.get(f.as_str()).copied().unwrap_or_else(|| default(f)) == *w)
            })
            .map(|(_, a, n)| (a.clone(), *n))
            .collect();
        // a runtime edge first, so an atom gentoo lists in RDEPEND and BDEPEND both does
        // not end up here as a build tool only
        out.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }
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
        // the config resolves for any name at all, so a typo would get a confident answer.
        // a settings file of its own is enough to be asked about
        let known = dir.is_some()
            || root.join("etc/kiry/pkg").join(name).is_file()
            || db::targets(&root)
                .unwrap_or_default()
                .iter()
                .any(|t| db::read(&root, t, name).is_ok());
        if !known {
            die(format!("no recipe, settings or installed package {name}"));
        }
        for (src, k, v) in &f.from {
            match v.is_empty() {
                true => say!("{src} {k}"),
                false => say!("{src} {k} {v}"),
            }
        }
        for l in f.record() {
            say!("resolved {l}");
        }
        if let Some(d) = dir.as_deref() {
            gentoo_says(&root, d, &f);
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
    let _w = writer(&root);
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
# regex                                      action
LLVM ERROR: out of memory.*lto               set LTO thin
(out of memory|signal 9|exit status 137)     set KIRY_THINLTO_JOBS /2
(Not a valid object file|invalid bitcode)    filter-lto
unknown argument: '(-[fm][\\w=-]+)'          drop $1
recompile with -fPIC                         append -fPIC
undefined symbol: __\\w+_chk                  append -U_FORTIFY_SOURCE
error: instruction requires:                 set CFLAGS_MARCH x86-64
undefined reference to `__isoc99_            notaflag musl-portability
PLEASE submit a bug report                   set OPT -O2
";

struct Rule {
    rx: Rx,
    act: String,
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
        // the pattern has single spaces in it, so only a run of two or a tab ends it.
        // a third column some tables on disk still carry is read as nothing: every retry
        // starts clean, and a relink after a kill happens inside the build, in kirycc
        let mut f = l.split('\t').flat_map(|x| x.split("  ")).map(str::trim).filter(|x| !x.is_empty());
        let (Some(rx), Some(act)) = (f.next(), f.next()) else {
            return Err(format!("failures:{}: wants a pattern and an action", n + 1));
        };
        out.push(Rule {
            rx: Rx::new(rx).map_err(|e| format!("failures:{}: {e}", n + 1))?,
            act: act.to_string(),
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
            });
        }
    }
    None
}

// drop, append and filter-lto are the recipe's to carry; set is the machine's opinion
// about one package and belongs beside the other settings. what comes back is the line
// and where it went, for the retry to say
fn write_fix(root: &Path, dir: &Path, name: &str, f: &Fix, log: &str) -> Result<Option<String>, String> {
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
        "notaflag" => return Ok(None),
        _ => return Err(format!("{verb} is not an action")),
    };

    let had = fs::read_to_string(&at).unwrap_or_default();
    // never the same fix twice: a rule that fired and did not help fires again on the
    // next log, and an entry appended each time would grow without bound
    if had.lines().any(|l| l.split('#').next().unwrap_or("").trim() == line) {
        return Ok(None);
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
    Ok(Some(format!("{line} in {}", at.display())))
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

// cpu seconds a build used, itself and every descendant waited for, which is the whole
// tree: the sandbox init reaps what it spawned. read after it exits and before it is
// reaped, while /proc still has it. utime stime cutime cstime are fields 14-17, in
// USER_HZ, which is 100 on x86. rustix has no getrusage, and RUSAGE_CHILDREN here would
// add up every build running in parallel
fn cpu_of(ch: &std::process::Child) -> Option<u64> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
    let pid = Pid::from_raw(ch.id() as i32)?;
    waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT).ok()?;
    let stat = fs::read_to_string(format!("/proc/{}/stat", ch.id())).ok()?;
    // comm is in parens and may hold spaces, so the fields count from its close
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<u64> = rest.split(' ').skip(11).take(4).filter_map(|v| v.parse().ok()).collect();
    (f.len() == 4).then(|| f.iter().sum::<u64>() / 100)
}

// appended rather than rewritten, so a batch that dies partway through has still
// recorded what it finished. a line per build rather than per package keeps it honest
// about what a package used to cost, and a few thousand of them is a hundred kilobytes
fn keep_time(root: &Path, p: &Package, t: &str, secs: u64, cpu: Option<u64>) {
    let at = times(root);
    let Some(up) = at.parent() else { return };
    if mkdirs(up).is_err() {
        return;
    }
    let cpu = cpu.map_or(String::new(), |c| format!(" {c}"));
    let line = format!("{} {} {t} {secs} {}{cpu}\n", p.name, p.version.upstream, today());
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
// whether to wait, and that is a decision made at the beginning. what comes back is
// which of todo compile, by position
fn forecast(root: &Path, todo: &[(String, String)], recipes: &HashMap<String, Package>) -> Vec<usize> {
    let past = history(root);
    let (mut builds, mut secs, mut new) = (Vec::new(), 0u64, 0);
    for (i, (name, t)) in todo.iter().enumerate() {
        let Some(p) = recipes.get(name) else { continue };
        if cached(root, p, t).is_some() {
            continue;
        }
        builds.push(i);
        match past.get(&(name.clone(), t.clone())) {
            Some(s) => secs += s,
            None => new += 1,
        }
    }
    let n = builds.len();
    if n < 2 {
        return builds;
    }
    let note = match new {
        0 => String::new(),
        u => format!("  {u} never built here"),
    };
    say!("{n} builds  ~{}{note}", clock(secs));
    builds
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
            "-q" => QUIET.store(true, Ordering::Relaxed),
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
        settle(&root, &going, live);
        let _w = writer(&root);
        apply_batch(&root, &paths, force);
        skew(&root);
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
        let p = match load(&root, &at) {
            Ok(p) => p,
            Err(e) => die(e),
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

    let mut set = wanted(&root, &seeds, &mut recipes, &HashSet::new());
    // a build runs on the toolchain, and no recipe names it. a root without it takes the
    // cached one first, stale or not: it compiles the same, and nothing here can build a
    // compiler without one
    if set.iter().any(|(n, t)| !fresh(&root, &recipes[n], t)) {
        let tools = first_tools(&root, &mut recipes);
        if !tools.is_empty() {
            for (n, t, _) in &tools {
                say!("{n} {} {t} goes in first from the cache, for the toolchain", recipes[n].version.upstream);
            }
            if !dry {
                let _w = writer(&root);
                apply_batch(&root, &tools.iter().map(|x| x.2.clone()).collect::<Vec<_>>(), force);
            }
            let have: HashSet<(String, String)> = tools.into_iter().map(|(n, t, _)| (n, t)).collect();
            set = wanted(&root, &seeds, &mut recipes, &have);
        }
    }
    let built: Vec<bool> = set.iter().map(|(n, t)| fresh(&root, &recipes[n], t)).collect();
    let mut plan = levels(&set, &recipes, &built);
    longest_first(&mut plan, &set, &history(&root));
    if dry {
        let mut building = 0;
        for (n, t) in &set {
            let how = match cached(&root, &recipes[n], t) {
                Some(_) => "cached",
                None => {
                    building += 1;
                    "builds"
                }
            };
            say!("{n} {} {t} {how}", recipes[n].version.upstream);
        }
        // the denominator for what follows
        if set.len() > 1 {
            say!("{building} builds  {} cached", set.len() - building);
        }
        // an inherited makedepends line is a subtree, not a package: an optional vlc
        // video source took obs from 13 builds to 311, and the number was here to read
        // before the first one started. what a name costs is what goes away with it,
        // which is the closure walked again without that edge
        let mut alone: Vec<(usize, &str)> = Vec::new();
        for (n, _) in &seeds {
            for d in &recipes[n].depends {
                if alone.iter().any(|(_, o)| *o == d.name) {
                    continue;
                }
                // one is a package and it is already a line above. what is worth
                // reading here is a name that takes others with it
                match set.len() - without(&root, &seeds, &recipes, &d.name) {
                    gone if gone > 1 => alone.push((gone, &d.name)),
                    _ => {}
                }
            }
        }
        alone.sort_unstable_by(|a, b| b.cmp(a));
        let w = alone.iter().map(|(_, n)| n.len()).max().unwrap_or(0);
        for (gone, name) in &alone {
            say!("only via {name:w$}  {gone} builds");
        }

        // the same answer the real run would get, override included
        let (risky, why) = route(&root, &set);
        match (risky, live) {
            (true, true) => say!("route live  --live, no fallback: {why}"),
            (true, false) => say!("route live  fallback first: {why}"),
            (false, _) => say!("route live  {why}"),
        }
        return;
    }
    // a level has to be in place before the one above it can build against it, which is
    // what forces the install to happen a level at a time. when every member is already
    // built that ordering buys nothing, so the set goes in as one transaction instead
    let ready: Option<Vec<PathBuf>> = set
        .iter()
        .map(|(n, t)| cached(&root, &recipes[n], t))
        .collect();
    // decided once, before any of it is built, so a batch cannot change its mind halfway
    settle(&root, &set, live);
    let builds = forecast(&root, &set, &recipes);
    if let Some(all) = ready {
        let rows: Vec<[&str; 3]> = set
            .iter()
            .map(|(n, t)| [n.as_str(), recipes[n].version.upstream.as_str(), t.as_str()])
            .collect();
        widths(&rows);
        for [n, v, t] in rows {
            say!("{}", row(n, v, installed_version(&root, t, n).as_deref(), t, "cached", None));
        }
        let _w = writer(&root);
        apply_batch(&root, &all, force);
        skew(&root);
        return;
    }

    let mut names: Vec<&String> = set.iter().map(|(n, _)| n).collect();
    names.sort();
    names.dedup();
    let rows: Vec<[&str; 3]> = set
        .iter()
        .map(|(n, t)| [n.as_str(), recipes[n].version.upstream.as_str(), t.as_str()])
        .collect();
    let building: Vec<[&str; 3]> = builds.iter().map(|&i| rows[i]).collect();
    batch_begin(&root, names.len(), &building, &rows);
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

        // one package is the unit, so its targets still build together and a level
        // runs width packages at a time. -v streams a build to the terminal, and two
        // streams at once are one unreadable one
        let targets_of = |name: &String| -> (bool, Vec<String>) {
            let mine: Vec<&(usize, bool)> = level.iter().filter(|(i, _)| set[*i].0 == *name).collect();
            let boot = mine.iter().any(|(_, b)| *b);
            (boot, mine.iter().map(|(i, _)| set[*i].1.clone()).collect())
        };
        let w = if verbose { 1 } else { width().min(order.len().max(1)) };
        let built = parallel(&order, w, |name| {
            let p = &recipes[*name];
            let (boot, targets) = targets_of(name);

            let need: Vec<String> = targets
                .iter()
                .filter(|t| match cached(&root, p, t) {
                    Some(_) => {
                        let was = installed_version(&root, t, &p.name);
                        say!("{}", row(&p.name, &p.version.upstream, was.as_deref(), t, "cached", None));
                        false
                    }
                    None => true,
                })
                .cloned()
                .collect();
            if need.is_empty() {
                return Ok(());
            }
            match (boot, fix) {
                (true, _) => build(&root, p, &need, verbose, true, w).map(|_| ()),
                (false, true) => need.iter().try_for_each(|t| recover(&root, p, t, verbose, w)),
                (false, false) => build(&root, p, &need, verbose, false, w).map(|_| ()),
            }
        });
        if let Err(e) = built {
            die(e);
        }

        let mut made = Vec::new();
        for name in order {
            let p = &recipes[name];
            let (_, targets) = targets_of(name);
            for t in &targets {
                match cached(&root, p, t) {
                    Some(a) => made.push(a),
                    None => die(format!("{} {t}: built nothing", p.name)),
                }
            }
        }
        let w = writer(&root);
        apply_batch(&root, &made, force);
        drop(w);
    }
    let _w = writer(&root);
    skew(&root);
    batch_end(true);
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
    requeue_pruned(root, &hurt);
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

// a pruned library dropped whatever nothing installed asked for when it was linked. a
// package installed since that asks for one of them gets the library queued, with the
// symbols, so the rebuild keeps them -- and the queue row heals the moment the soname
// exports them again, the same way every other row does
fn requeue_pruned(root: &Path, hurt: &[(String, Finding)]) {
    if !hurt.iter().any(|(_, f)| matches!(f.what, What::MissingSymbol(_))) {
        return;
    }
    // (target, owner, soname, what the first link exported)
    let mut pruned: Vec<(String, String, String, HashSet<String>)> = Vec::new();
    for t in db::targets(root).unwrap_or_default() {
        for name in db::installed(root, &t).unwrap_or_default() {
            let Ok(rec) = db::read(root, &t, &name) else { continue };
            for e in rec.manifest.iter().filter(|e| e.path.starts_with(PRUNED)) {
                let Ok(o) = elf::read(&root.join(&e.path)) else { continue };
                let Some(so) = o.soname else { continue };
                pruned.push((t.clone(), name.clone(), so, o.exports.into_iter().map(|x| x.name).collect()));
            }
        }
    }
    let mut rows: Vec<db::Queued> = db::read_queue(root).unwrap_or_default();
    let before = rows.len();
    for (t, f) in hurt {
        let What::MissingSymbol(sym) = &f.what else { continue };
        let sym = sym.split('@').next().unwrap_or(sym);
        let Ok(o) = elf::read(&root.join(&f.path)) else { continue };
        for (pt, owner, so, full) in &pruned {
            if pt != t || !o.needed.contains(so) || !full.contains(sym) {
                continue;
            }
            match rows.iter_mut().find(|q| q.target == *t && q.name == *owner && q.soname == *so) {
                Some(q) if !q.changed.iter().any(|c| c == sym) => q.changed.push(sym.to_string()),
                Some(_) => {}
                None => rows.push(db::Queued {
                    target: t.clone(),
                    name: owner.clone(),
                    soname: so.clone(),
                    changed: vec![sym.to_string()],
                }),
            }
            say!("{owner} {t} was pruned of {sym}, which {} needs  queued", f.pkg);
        }
    }
    if rows.len() != before || rows.iter().any(|q| !q.changed.is_empty()) {
        rows.sort();
        rows.dedup();
        let _ = db::write_queue(root, &rows);
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
// what the closure would be with one dependency edge cut. the whole of it is loaded by
// the time this runs, so it walks what wanted() already walked and touches no disk beyond
// asking what is installed
fn without(
    root: &Path,
    seeds: &[(String, String)],
    recipes: &HashMap<String, Package>,
    cut: &str,
) -> usize {
    let host = sandbox::host();
    let mut out: Vec<(String, String)> = seeds.to_vec();
    let mut seen: HashSet<(String, String)> = seeds.iter().cloned().collect();
    let mut i = 0;
    while i < out.len() {
        let (name, t) = out[i].clone();
        i += 1;
        let Some(p) = recipes.get(&name) else { continue };
        for d in p.depends.iter().filter(|d| d.applies(&t) && d.name != cut) {
            let at = (d.name.clone(), if d.host { host.clone() } else { t.clone() });
            if db::read(root, &at.1, &at.0).is_ok() || !seen.insert(at.clone()) {
                continue;
            }
            out.push(at);
        }
    }
    out.len()
}

// have is what counts as installed without the db saying so yet: a dry run's toolchain
fn wanted(
    root: &Path,
    seeds: &[(String, String)],
    recipes: &mut HashMap<String, Package>,
    have: &HashSet<(String, String)>,
) -> Vec<(String, String)> {
    let host = sandbox::host();
    let mut out = seeds.to_vec();
    let mut seen: HashSet<(String, String)> = seeds.iter().cloned().collect();
    // who asked, for the one error where a bare name says nothing: java-jre on its own
    // does not say it came from prismlauncher, or that it is a name an alias can answer
    let mut by: HashMap<String, String> = HashMap::new();
    let mut i = 0;
    while i < out.len() {
        let (name, t) = out[i].clone();
        i += 1;
        if !recipes.contains_key(&name) {
            let at = match resolve(root, &name) {
                Ok(at) => at,
                Err(e) => match by.get(&name) {
                    Some(who) => die(format!(
                        "{who} depends on {name}, and no recipe, alias or installed package answers to it\n\
                         a line \"{name} <package>\" in {} says which one does",
                        repos(root).first().map_or(PathBuf::from("aliases"), |r| r.join("aliases")).display()
                    )),
                    None => die(e),
                },
            };
            match load(root, &at) {
                Ok(p) => recipes.insert(name.clone(), p),
                Err(e) => die(e),
            };
        }
        let p = &recipes[&name];
        if !p.targets.contains(&t) {
            die(format!("{name} is wanted for {t} and does not build for it"));
        }
        // a built artifact is installed, which asks for its runtime deps alone. walking
        // its build deps too closed llvm, cmake and python3 into a cycle on an empty
        // root with every one of them sitting in the cache
        let built = fresh(root, p, &t);
        let next: Vec<(String, String)> = p
            .depends
            .iter()
            .filter(|d| d.applies(&t) && !(built && d.make))
            .map(|d| {
                let dt = if d.host { host.clone() } else { t.clone() };
                (d.name.clone(), dt)
            })
            .collect();
        for d in next {
            if db::read(root, &d.1, &d.0).is_ok() || have.contains(&d) || !seen.insert(d.clone()) {
                continue;
            }
            by.entry(d.0.clone()).or_insert_with(|| name.clone());
            out.push(d);
        }
    }
    out
}

// the toolchain members the root lacks, with the runtime deps they bring, each from an
// artifact of its current version whatever its flags were
fn first_tools(root: &Path, recipes: &mut HashMap<String, Package>) -> Vec<(String, String, PathBuf)> {
    let host = sandbox::host();
    let mut queue: Vec<String> = match sandbox::toolchain(root) {
        Ok(t) => t.into_iter().map(|d| d.name).collect(),
        Err(e) => die(e),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while let Some(n) = queue.pop() {
        if db::read(root, &host, &n).is_ok() || !seen.insert(n.clone()) {
            continue;
        }
        if !recipes.contains_key(&n) {
            match resolve(root, &n).and_then(|at| load(root, &at)) {
                Ok(p) => recipes.insert(n.clone(), p),
                Err(e) => die(e),
            };
        }
        let p = &recipes[&n];
        let a = artifact(root, p, &host);
        if !a.is_file() {
            die(format!("{n} is in the toolchain and not installed, and there is no {host} artifact of it to start from"));
        }
        queue.extend(p.depends.iter().filter(|d| !d.make && d.applies(&host)).map(|d| d.name.clone()));
        out.push((n, host.clone(), a));
    }
    out
}

// the pair of root subvolumes. the one mounted at / is the one the machine lives on and
// every install goes into it. the other is its fallback, a snapshot taken before a risky
// transaction and never installed into -- installing into the other half and booting it
// is what lost work here three ways: whatever was done on the running root after the
// snapshot, whatever was only in the other half when the next snapshot replaced it, and
// everything on a new root when a boot that never committed bounced back to the old one
const ROOTS: &[&str] = &["@root-a", "@root-b"];

// on /run, so a mount point left behind by a crash is gone at the next boot, and so is
// the marker that says this boot already took its fallback
const TOP: &str = "/run/kiry/top";
const TAKEN: &str = "/run/kiry/fallback";

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

// which of the pair is running and which is not, and the device they share
fn pair(text: &str) -> Result<(String, String, String), String> {
    let (dev, now) =
        mounted_at(text, "/").ok_or("nothing in /proc/mounts says which subvolume / is on")?;
    // a root that is not one of the pair has no other half, and guessing one would name
    // a subvolume that means nothing on this machine
    if !ROOTS.contains(&now) {
        return Err(format!(
            "/ is on {now} and the two roots are {}",
            ROOTS.join(" and ")
        ));
    }
    let other = ROOTS
        .iter()
        .find(|r| **r != now)
        .ok_or("there is only one root subvolume")?;
    Ok((dev.to_string(), now.to_string(), (*other).to_string()))
}

// the running root as it was before a risky transaction, in the other half of the pair.
// once a boot: a tree that booted is the proven one, and a second risky install in the
// same boot would otherwise swap it for a tree only the first install has touched
fn fallback(text: &str, taken: &Path) -> Result<(), String> {
    let (dev, now, other) = pair(text)?;
    if taken.exists() {
        say!("            {other} is the fallback, {now} as this boot found it");
        return Ok(());
    }
    let top = PathBuf::from(TOP);
    mkdirs(&top)?;

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
    put(taken, &format!("{other}\n"))?;
    say!("            {other} is the fallback, a snapshot of {now}. boot {} from the firmware menu if this goes wrong", label(&other));
    Ok(())
}

// btrfs says what it did on stdout, and the line kiry prints after says it once
fn snapshot(top: &Path, now: &str, other: &str) -> Result<(), String> {
    let at = top.join(other);
    if at.exists() {
        run(
            Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(&at)
                .stdout(Stdio::null()),
            &format!("btrfs subvolume delete {other}"),
        )?;
    }
    run(
        Command::new("btrfs")
            .args(["subvolume", "snapshot"])
            .arg(top.join(now))
            .arg(&at)
            .stdout(Stdio::null()),
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

// what says an installed elf cannot run, which is what an install has to say about the
// tree it just wrote. the rest of what doctor reports is worth reading and not worth
// shouting after every install: duplicate symbols across two libraries that genuinely both
// export a name is a standing condition, not a regression
fn broken(root: &Path) -> Vec<(String, Finding)> {
    let mut out = Vec::new();
    for (t, f) in checks(root) {
        if f.what.breaks() {
            out.push((t, f));
        }
    }
    out
}

// the transaction lands on the running root either way. what a risky one gets first is
// a fallback, and which of the two happened is said rather than left to be guessed at
fn settle(root: &Path, going: &[(String, String)], live: bool) {
    let (risky, why) = route(root, going);
    if !risky {
        say!("route live  {why}");
    } else if live {
        // overruled rather than reconsidered, and the reason is still printed. what is
        // worth having in a log six months later is which answer was set aside
        say!("route live  --live, no fallback: {why}");
    } else {
        say!("route live  fallback first: {why}");
        let made = fs::read_to_string("/proc/mounts")
            .map_err(|e| format!("/proc/mounts: {e}"))
            .and_then(|text| fallback(&text, Path::new(TAKEN)));
        if let Err(e) = made {
            die(e);
        }
    }
    say_unkept(root);
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

// whether a transaction wants a fallback before it goes in. a snapshot first is right for
// a libc and pointless for a new cli tool, and it is decidable rather than a question
// worth asking: kiry knows every file the batch replaces, and /proc says which of them
// something running still has mapped
fn route(root: &Path, going: &[(String, String)]) -> (bool, String) {
    // first, and not one of the reasons below. a fallback means snapshotting the subvolume
    // the machine is running from, so it is a property of / and of nothing else -- a
    // staging root asked about a libc must not reach anything that touches the real
    // filesystem
    let at = root.canonicalize();
    if at.as_deref().unwrap_or(root) != Path::new("/") {
        return (false, "not the running root".to_string());
    }
    match why_ab(root, going) {
        Some(w) => (true, w),
        None => (false, nothing_open(root, going)),
    }
}

// what the transaction touches that is worth a fallback
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
    let moved = layouts_moved(root, &jobs);
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
    for so in &done.preserved {
        say!("preserved {so}, still named by what has not rebuilt yet");
    }
    enqueue(root, &done.broke, &named(&jobs));
    queue_layouts(root, &moved, &named(&jobs));
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
            Ok(s) => complain(&format!("{}: {s}", h.display())),
            Err(e) => complain(&format!("{}: {e}", h.display())),
        }
    }
}

fn named(jobs: &[install::Job]) -> HashSet<(String, String)> {
    jobs.iter()
        .map(|j| (j.target.clone(), j.name.clone()))
        .collect()
}

fn remove_cmd(args: &[String]) {
    let (root, force, rest) = opts(args);
    let names = asked(rest, &[]);
    writes(&root);
    if names.is_empty() {
        die("nothing to remove".into());
    }
    let _w = writer(&root);

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
    if !force {
        held_by(&root, &plan);
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
    skew(&root);
}

// everything a removal would leave without what it needs, named all at once: told only
// the first, the next try finds the second. a depends line is what somebody wrote down
// and what the files link is what breaks -- jq names oniguruma as make only and links
// libonig.so.5 all the same -- so doctor is asked what it would newly find with these
// gone, which also catches a script whose interpreter is going
fn held_by(root: &Path, plan: &[(&String, Vec<String>)]) {
    let mut held: Vec<String> = Vec::new();
    for (t, mine) in plan {
        for other in db::installed(root, t).unwrap_or_default() {
            if mine.contains(&other) {
                continue;
            }
            let Ok(o) = db::read(root, t, &other) else { continue };
            for d in o.depends.iter().filter(|d| !d.make && mine.contains(&d.name)) {
                held.push(format!("{other} still needs {}", d.name));
            }
        }
    }

    let gone: Vec<(String, String)> = plan
        .iter()
        .flat_map(|(t, mine)| mine.iter().map(|n| ((*t).clone(), n.clone())))
        .collect();
    let row = |t: &String, f: &Finding| (t.clone(), f.path.clone(), f.what.to_string());
    let before: HashSet<(String, String, String)> =
        broken(root).iter().map(|(t, f)| row(t, f)).collect();
    let mut by: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (t, f) in checks_without(root, &gone).0 {
        // a preserved record keeps its package's name after the package is gone
        let theirs = gone.contains(&(t.clone(), f.pkg.clone()));
        if !f.what.breaks() || theirs || before.contains(&row(&t, &f)) {
            continue;
        }
        by.entry((f.pkg.clone(), t)).or_default().insert(f.what.to_string());
    }
    for ((pkg, t), what) in by {
        let what: Vec<String> = what.into_iter().collect();
        held.push(format!("{pkg} {t} would break: {}", what.join(", ")));
    }
    if !held.is_empty() {
        die(held.join("\n"));
    }
}

fn list_cmd(args: &[String]) {
    let (root, _, _) = opts(args);
    there(&root);
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
            let Ok(p) = read_recipe(&at) else { continue };
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

// a recipe dir as it is written. the repair path never reads one -- it installs from an
// artifact's sidecar -- so this is the kiry crate's, not kiry-core's
#[derive(Debug)]
struct Package {
    name: String,
    dir: PathBuf,
    version: pkg::Version,
    sources: Vec<String>,
    checksums: Vec<String>,
    depends: Vec<Dep>,
    targets: Vec<String>,
    users: Vec<String>,
}

fn read_recipe(dir: &Path) -> Result<Package, kiry_core::Error> {
    if !dir.is_dir() {
        return Err(kiry_core::Error::NoPackage(dir.to_path_buf()));
    }

    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| kiry_core::Error::Name(dir.to_path_buf()))?
        .to_string();

    let version = pkg::Version::parse(&pkg::required(&dir.join("version"))?)?;

    let sources = pkg::lines(&dir.join("sources"))?;
    let checksums = pkg::lines(&dir.join("checksums"))?;
    // a hand written recipe can have sources and no checksums yet
    if !checksums.is_empty() && sources.len() != checksums.len() {
        return Err(kiry_core::Error::Counts {
            sources: sources.len(),
            checksums: checksums.len(),
        });
    }

    let depends = pkg::depends_from(pkg::lines(&dir.join("depends"))?);
    let users = pkg::lines(&dir.join("users"))?;

    // one line or one per line, either way
    let mut targets = Vec::new();
    for l in pkg::lines(&dir.join("targets"))? {
        targets.extend(l.split_whitespace().map(String::from));
    }
    if targets.is_empty() {
        return Err(kiry_core::Error::Empty(dir.join("targets")));
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod recipes {
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

        let p = read_recipe(&d).unwrap();
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

        let p = read_recipe(&d).unwrap();
        assert!(p.depends.is_empty());
        assert!(p.sources.is_empty());
        assert_eq!(p.targets, ["x86_64-musl"]);
    }
    #[test]
    fn targets_one_per_line_works_too() {
        let d = scratch("perline");
        write(&d, "version", "2 3");
        write(&d, "targets", "x86_64-musl\nx86_64-gnu\n");

        assert_eq!(read_recipe(&d).unwrap().targets, ["x86_64-musl", "x86_64-gnu"]);
    }
    #[test]
    fn a_directory_that_isnt_there_says_so() {
        let d = scratch("gone");
        fs::remove_dir_all(&d).unwrap();
        assert!(matches!(read_recipe(&d), Err(kiry_core::Error::NoPackage(_))));
    }
    #[test]
    fn a_missing_version_names_the_file() {
        let d = scratch("noversion");
        write(&d, "targets", "x86_64-musl");

        match read_recipe(&d) {
            Err(kiry_core::Error::Required(p)) => assert!(p.ends_with("version")),
            other => panic!("wanted Required, got {other:?}"),
        }
    }
    #[test]
    fn targets_cannot_be_blank() {
        let d = scratch("notargets");
        write(&d, "version", "1 1");
        write(&d, "targets", "\n\n# only a comment\n");

        assert!(matches!(read_recipe(&d), Err(kiry_core::Error::Empty(_))));
    }
    #[test]
    fn checksums_have_to_pair_up_with_sources() {
        let d = scratch("shortsums");
        write(&d, "version", "1 1");
        write(&d, "targets", "x86_64-musl");
        write(&d, "sources", "a\nb\n");
        write(&d, "checksums", "aa\n");

        match read_recipe(&d) {
            Err(kiry_core::Error::Counts { sources, checksums }) => {
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

        assert!(matches!(read_recipe(&d), Err(kiry_core::Error::Io(_, _))));
    }
}

// a recipe the way the rest of kiry sees it: every dependency named the way this system
// names it. a depends file can say what alpine said -- java-jre, so:libGL.so.1,
// cmd:emacs -- and which package that means is decided here, when it is read, against
// the aliases and what is installed now. the plan, the sidecar and the installed record
// then carry a real package, so removal and why see the real edge, and a pair added to
// an aliases file counts from the next command rather than the next conversion
fn load(root: &Path, at: &Path) -> Result<Package, String> {
    let mut p = read_recipe(at).map_err(|e| e.to_string())?;
    let host = sandbox::host();
    let mut out: Vec<Dep> = Vec::new();
    for d in std::mem::take(&mut p.depends) {
        // one soname can be two packages, mesa on musl and libglvnd on gnu, so a name is
        // answered once per target the line is for and split by target where they differ
        let mut on: Vec<&str> =
            p.targets.iter().filter(|t| d.applies(t)).map(String::as_str).collect();
        if d.host || on.is_empty() {
            on = vec![host.as_str()];
        }
        let got: Vec<Option<String>> = on.iter().map(|t| called(root, &d.name, t)).collect();
        let split = got.iter().any(|g| *g != got[0]);
        for (t, to) in on.iter().zip(got) {
            let mut d = d.clone();
            if split {
                d.only = Some((*t).to_string());
            }
            match to {
                // no equivalent here, or its own subpackage
                Some(to) if to == "-" || to == p.name => continue,
                Some(to) => d.name = to,
                None => {}
            }
            if !out.iter().any(|x| x.name == d.name && x.make == d.make && x.host == d.host && x.only == d.only) {
                out.push(d);
            }
        }
    }
    // flags act through the closure: off takes a dependency out of the namespace the
    // build sees, on puts one in, and configure's own detection does the rest
    if let Some(g) = gentoo(&p.dir) {
        let f = flags(root, &p.name, Some(&p.dir))?;
        let set = chosen(&f.use_flags);
        if !set.is_empty() {
            out.retain(|d| !g.unwanted(&set, &d.name));
            for (a, needs) in g.wanted(&set) {
                let Some(n) = recipe_for(root, &a) else { continue };
                if n == p.name || out.iter().any(|d| d.name == n) {
                    continue;
                }
                out.push(Dep {
                    name: n,
                    make: needs != Needs::Run,
                    host: needs == Needs::Tool,
                    only: None,
                });
            }
        }
    }
    // core/ may never depend on extra/: whatever is in core drags its closure in with it,
    // and nothing said so while python3 named libffi and xz named gettext-tiny
    let repo = |d: &Path| d.parent().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned());
    if repo(&p.dir).as_deref() == Some("core") {
        for d in &out {
            let Some(at) = recipe(root, &d.name) else { continue };
            if let Some(r) = repo(&at).filter(|r| r == "extra" || r == "testing") {
                return Err(format!(
                    "core/{} depends on {r}/{}, and core may never depend on extra",
                    p.name, d.name
                ));
            }
        }
    }
    p.depends = out;
    Ok(p)
}

// read once per root and kept: a plan loads hundreds of recipes, and the generated table
// is seventeen thousand lines. the owner index waits for the first so:, cmd: or pc:
// name, because it walks every manifest installed
struct Names {
    alias: HashMap<String, String>,
    owners: Option<HashMap<(String, String), String>>,
}

// t is the target the name is wanted on. a provider installed only on the other target
// answers nothing here: that is how gnu libglvnd came to stand in for musl mesa
fn called(root: &Path, name: &str, t: &str) -> Option<String> {
    static SEEN: std::sync::Mutex<Option<HashMap<PathBuf, Names>>> = std::sync::Mutex::new(None);
    let mut all = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let n = all.get_or_insert_with(HashMap::new).entry(root.to_path_buf()).or_insert_with(|| Names {
        alias: convert::aliases(&repos(root)),
        owners: None,
    });
    let Some((kind, what)) = name.split_once(':') else {
        return n.alias.get(name).cloned();
    };
    // what this machine has installed is the answer before anything alpine wrote down
    let owners = n.owners.get_or_insert_with(|| owners(root));
    if let Some(p) = owners.get(&(t.to_string(), name.to_string())).or_else(|| n.alias.get(name)) {
        return Some(p.clone());
    }
    // nothing installed answers. a recipe named after the command or the .pc is the
    // likely provider, and a wrong guess still fails loudly at its own build
    (matches!(kind, "cmd" | "pc") && recipe(root, what).is_some()).then(|| what.to_string())
}

// so:<soname>, cmd:<command> and pc:<module> for everything installed, keyed by target
// as well, to the package that ships it there
fn owners(root: &Path) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    for t in db::targets(root).unwrap_or_default() {
        for name in db::installed(root, &t).unwrap_or_default() {
            for pv in db::read_provides(root, &t, &name).unwrap_or_default() {
                out.entry((t.clone(), format!("so:{}", pv.soname))).or_insert_with(|| name.clone());
            }
            let Ok(rec) = db::read(root, &t, &name) else { continue };
            for e in &rec.manifest {
                let path = e.path.as_str();
                let key = if let Some(c) = ["usr/bin/", "usr/sbin/", "bin/", "sbin/"]
                    .iter()
                    .find_map(|d| path.strip_prefix(d))
                    .filter(|c| !c.contains('/'))
                {
                    format!("cmd:{c}")
                } else if let Some(m) = path
                    .strip_suffix(".pc")
                    .and_then(|s| s.rsplit_once("/pkgconfig/"))
                    .map(|(_, m)| m)
                {
                    format!("pc:{m}")
                } else {
                    continue;
                };
                out.entry((t.clone(), key)).or_insert_with(|| name.clone());
            }
        }
    }
    out
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
// built says which members install from the cache, whose build deps order nothing
fn levels(
    want: &[(String, String)],
    recipes: &HashMap<String, Package>,
    built: &[bool],
) -> Vec<Vec<(usize, bool)>> {
    let host = sandbox::host();
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
                                let t = if x.host { &host } else { &want[*i].1 };
                                x.applies(&want[*i].1)
                                    && !(built[*i] && x.make)
                                    && x.name == want[*j].0
                                    && *t == want[*j].1
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

// within a level nothing depends on anything else, so the order is free, and a level
// takes as long as its slowest member wherever that one starts. longest first is what
// leaves the short tail at the end
fn longest_first(
    plan: &mut [Vec<(usize, bool)>],
    want: &[(String, String)],
    past: &HashMap<(String, String), u64>,
) {
    for level in plan.iter_mut() {
        // never built here goes first. an unknown cost is the one worth starting early
        // and it is the only guess on offer
        level.sort_by_key(|(i, _)| {
            (
                std::cmp::Reverse(past.get(&want[*i]).copied().unwrap_or(u64::MAX)),
                want[*i].clone(),
            )
        });
    }
}

#[cfg(test)]
mod order {
    use super::*;

    // the vlc number: one line in a converted makedepends, and what goes away with it is
    // a subtree rather than a package
    #[test]
    fn what_a_dependency_costs_is_what_leaves_with_it() {
        let t = "x86_64-musl".to_string();
        let pkg = |name: &str, deps: &[&str]| Package {
            name: name.to_string(),
            dir: PathBuf::new(),
            version: pkg::Version::parse("1 1").unwrap(),
            sources: Vec::new(),
            checksums: Vec::new(),
            depends: deps
                .iter()
                .map(|d| pkg::Dep { name: (*d).to_string(), make: false, host: false, only: None })
                .collect(),
            targets: vec![t.clone()],
            users: Vec::new(),
        };
        let recipes = HashMap::from([
            ("app".to_string(), pkg("app", &["vlc", "zlib"])),
            ("vlc".to_string(), pkg("vlc", &["dvdread", "zlib"])),
            ("dvdread".to_string(), pkg("dvdread", &["dvdcss"])),
            ("dvdcss".to_string(), pkg("dvdcss", &[])),
            ("zlib".to_string(), pkg("zlib", &[])),
        ]);
        let seeds = vec![("app".to_string(), t)];
        // nothing is installed, so the root can be anywhere -- db::read just fails
        let none = Path::new("/nonexistent-kiry-root");

        // app vlc dvdread dvdcss zlib
        assert_eq!(without(none, &seeds, &recipes, ""), 5);
        // zlib is reached around vlc as well, so only the dvd chain leaves with it
        assert_eq!(without(none, &seeds, &recipes, "vlc"), 2);
        // and a leaf costs itself alone
        assert_eq!(without(none, &seeds, &recipes, "zlib"), 4);
    }

    fn three() -> Vec<(String, String)> {
        ["quick", "slow", "new"]
            .iter()
            .map(|n| ((*n).to_string(), "x86_64-musl".to_string()))
            .collect()
    }

    #[test]
    fn a_level_starts_with_the_one_that_takes_longest() {
        let want = three();
        let past = HashMap::from([(want[0].clone(), 30u64), (want[1].clone(), 3000u64)]);
        let mut plan = vec![vec![(0, false), (1, false), (2, false)]];
        longest_first(&mut plan, &want, &past);
        let names: Vec<&str> = plan[0].iter().map(|(i, _)| want[*i].0.as_str()).collect();
        assert_eq!(names, ["new", "slow", "quick"]);
    }

    use std::sync::Mutex;
    use std::time::Duration;

    // each item waits to see the other running. one at a time, nobody ever does
    fn overlapped(width: usize) -> usize {
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let _ = parallel(&[0, 1], width, |_| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            for _ in 0..100 {
                if running.load(Ordering::SeqCst) == 2 {
                    peak.store(2, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            running.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        });
        peak.load(Ordering::SeqCst)
    }

    #[test]
    fn a_level_two_wide_runs_two_at_once() {
        assert_eq!(overlapped(2), 2);
        assert_eq!(overlapped(1), 1);
    }

    #[test]
    fn a_failure_starts_nothing_new_and_waits_for_what_runs() {
        let ran = Mutex::new(Vec::new());
        let r = parallel(&[0, 1, 2, 3], 2, |i| {
            if *i == 0 {
                std::thread::sleep(Duration::from_millis(30));
                return Err("zero broke".to_string());
            }
            std::thread::sleep(Duration::from_millis(120));
            ran.lock().unwrap().push(*i);
            Ok(())
        });
        assert_eq!(r, Err("zero broke".to_string()));
        // one was already running beside the failure and finished. nothing after it began
        assert_eq!(*ran.lock().unwrap(), [1]);
    }

    #[test]
    fn a_level_starts_its_builds_in_the_order_given() {
        let seen = Mutex::new(Vec::new());
        parallel(&["slow", "mid", "quick"], 1, |n| {
            seen.lock().unwrap().push(*n);
            Ok(())
        })
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), ["slow", "mid", "quick"]);
    }

    // the only test that touches KIRY_PARALLEL, so setting it here races nothing
    #[test]
    fn one_build_at_a_time_unless_asked() {
        std::env::remove_var("KIRY_PARALLEL");
        assert_eq!(width(), 1);
        std::env::set_var("KIRY_PARALLEL", "3");
        assert_eq!(width(), 3);
        std::env::remove_var("KIRY_PARALLEL");
    }

    #[test]
    fn make_gets_the_machine_divided_by_the_builds_on_it() {
        assert_eq!(makeflags(16, 1), "-j16");
        assert_eq!(makeflags(16, 2), "-j8");
        // more builds than threads still leaves each of them something to run with
        assert_eq!(makeflags(4, 8), "-j1");
    }

    // bmake reads MAKEFLAGS, has no -l in its option set and quits with its usage line
    // rather than ignoring one, which is lowdown every time it is not building alone
    #[test]
    fn nothing_gnu_make_alone_understands_goes_in_makeflags() {
        for at_once in 1..=8 {
            let f = makeflags(16, at_once);
            assert!(!f.contains("-l"), "bmake cannot read {f}");
        }
    }

    #[test]
    fn the_levels_themselves_do_not_move() {
        let want = three();
        let past = HashMap::from([(want[2].clone(), 9000u64)]);
        let mut plan = vec![vec![(0, false)], vec![(2, false)], vec![(1, false)]];
        longest_first(&mut plan, &want, &past);
        // a level is a dependency step. reordering those builds something against what
        // is not installed yet, which is the one thing this is not allowed to touch
        let order: Vec<&str> = plan.iter().map(|l| want[l[0].0].0.as_str()).collect();
        assert_eq!(order, ["quick", "new", "slow"]);
    }

    #[test]
    fn a_make_dep_is_a_level_below_what_builds_with_it() {
        let t = sandbox::host();
        let want = vec![("acl".to_string(), t.clone()), ("attr".to_string(), t.clone())];
        let pkg = |name: &str, depends: Vec<pkg::Dep>| Package {
            name: name.to_string(),
            dir: PathBuf::new(),
            version: pkg::Version::parse("1 1").unwrap(),
            sources: Vec::new(),
            checksums: Vec::new(),
            depends,
            targets: vec![t.clone()],
            users: Vec::new(),
        };
        let attr = pkg::Dep { name: "attr".to_string(), make: true, host: true, only: None };
        let recipes = HashMap::from([
            ("acl".to_string(), pkg("acl", vec![attr])),
            ("attr".to_string(), pkg("attr", Vec::new())),
        ]);
        let plan = levels(&want, &recipes, &[false, false]);
        let order: Vec<Vec<&str>> = plan
            .iter()
            .map(|l| l.iter().map(|(i, _)| want[*i].0.as_str()).collect())
            .collect();
        assert_eq!(order, [["attr"], ["acl"]]);

        // acl is in the cache, so nothing builds with attr and neither waits
        let plan = levels(&want, &recipes, &[true, false]);
        assert_eq!(plan.len(), 1, "{plan:?}");
    }
}

fn rebuild_cmd(args: &[String]) {
    let mut dry = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "-n" {
            dry = true;
        } else if a == "-q" {
            QUIET.store(true, Ordering::Relaxed);
        } else {
            rest.push(a.clone());
        }
    }
    let (root, _, extra) = opts(&rest);
    writes(&root);
    if let Some(a) = extra.first() {
        die(format!("rebuild takes no arguments, got {a}"));
    }

    if let Err(e) = db::targets(&root) {
        die(e.to_string());
    }

    let mut want: Vec<(String, String)> = Vec::new();
    let w = (!dry).then(|| writer(&root));
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
    // what put each one here, for the rebuild log: a queue row's soname, or doctor
    let mut why: HashMap<(String, String), Vec<String>> = HashMap::new();
    for q in live {
        why.entry((q.name.clone(), q.target.clone())).or_default().push(q.soname);
        if !want.contains(&(q.name.clone(), q.target.clone())) {
            want.push((q.name, q.target));
        }
    }
    for (t, f) in checks(&root) {
        if f.what.rebuilds() && !want.contains(&(f.pkg.clone(), t.clone())) {
            why.entry((f.pkg.clone(), t.clone())).or_default().push("doctor".into());
            want.push((f.pkg.clone(), t));
        }
    }
    drop(w);
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
        match load(&root, &dir) {
            Ok(p) => recipes.insert(name.clone(), p),
            Err(e) => die(e),
        };
    }

    let mut plan = levels(&want, &recipes, &vec![false; want.len()]);
    longest_first(&mut plan, &want, &history(&root));
    if dry {
        for (i, boot) in plan.into_iter().flatten() {
            let how = if boot { "would bootstrap" } else { "would rebuild" };
            say!("{} {} {how}", want[i].0, want[i].1);
        }
        return;
    }

    let mut names: Vec<&String> = want.iter().map(|(n, _)| n).collect();
    names.sort();
    names.dedup();
    let rows: Vec<[&str; 3]> = want
        .iter()
        .map(|(n, t)| [n.as_str(), recipes[n].version.upstream.as_str(), t.as_str()])
        .collect();
    batch_begin(&root, names.len(), &rows, &rows);
    // one stuck package out of a storm leaves the rest to build. what needs it, directly
    // or through something skipped, is skipped rather than built against the old one
    let host = sandbox::host();
    let mut failed: Vec<usize> = Vec::new();
    for level in plan {
        let level: Vec<(usize, bool)> = level
            .into_iter()
            .filter(|(i, _)| {
                let (name, target) = &want[*i];
                let p = &recipes[name];
                let gone = failed.iter().copied().find(|j| {
                    want[*j].0 == *name
                        || p.depends.iter().any(|x| {
                            let t = if x.host { &host } else { target };
                            x.applies(target) && x.name == want[*j].0 && *t == want[*j].1
                        })
                });
                if let Some(j) = gone {
                    say!("{name} {} {target} skip  {} failed", p.version.upstream, want[j].0);
                    failed.push(*i);
                }
                gone.is_none()
            })
            .collect();
        // every member of a level compiles before any of it is installed, and the level
        // below it is already in place, so each one links against what it will run with
        // a package at a time, its targets one after another: a recovery rewrites the
        // package's filter file, and two targets doing that at once lose a line
        let mut names: Vec<&String> = Vec::new();
        for (i, _) in &level {
            if !names.contains(&&want[*i].0) {
                names.push(&want[*i].0);
            }
        }
        let w = width().min(names.len().max(1));
        let errs: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
        let _ = parallel(&names, w, |name| {
            let p = &recipes[*name];
            for (i, boot) in level.iter().filter(|(i, _)| want[*i].0 == **name) {
                let target = &want[*i].1;
                // a bootstrap pass is a stand-in that exists to be replaced, so it is
                // built straight rather than put through the recovery loop
                let r = match boot {
                    true => {
                        build(&root, p, std::slice::from_ref(target), false, true, w).map(|_| ())
                    }
                    false => recover(&root, p, target, false, w),
                };
                if let Err(e) = r {
                    errs.lock().unwrap_or_else(|e| e.into_inner()).push(((*name).clone(), e));
                    break;
                }
            }
            Ok(())
        });
        let errs = errs.into_inner().unwrap_or_else(|e| e.into_inner());
        for (_, e) in &errs {
            for l in e.lines() {
                complain(l);
            }
        }
        let mut made = Vec::new();
        for (i, _) in &level {
            let (name, target) = &want[*i];
            let p = &recipes[name];
            // a package's targets go in together or not at all
            if errs.iter().any(|(n, _)| n == name) {
                failed.push(*i);
                continue;
            }
            match cached(&root, p, target) {
                Some(a) => made.push(a),
                None => die(format!("{} {target}: built nothing", p.name)),
            }
        }
        if made.is_empty() {
            continue;
        }
        let w = writer(&root);
        let jobs = match install::plan(&root, &made, false) {
            Ok(j) => j,
            Err(e) => die(e.to_string()),
        };
        let was: Vec<Option<db::Installed>> =
            jobs.iter().map(|j| db::read(&root, &j.target, &j.name).ok()).collect();
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
        for (j, old) in jobs.iter().zip(&was) {
            let now = db::read(&root, &j.target, &j.name).ok();
            let same = matches!((old, &now), (Some(a), Some(b)) if hashes(&a.manifest) == hashes(&b.manifest));
            let note = if same { "  to the same bytes it replaced" } else { "" };
            say!("{} {} {} rebuilt{note}", j.name, j.version.upstream, j.target);
            let k = (j.name.clone(), j.target.clone());
            let reason = why.get(&k).map_or("-".to_string(), |w| w.join(","));
            rebuilt_log(&root, &j.name, &j.target, &reason, same);
        }
        for so in &done.preserved {
            say!("preserved {so}, still named by what has not rebuilt yet");
        }
        enqueue(&root, &broke, &named(&jobs));
        hooks(&root, jobs.iter().map(|j| j.target.clone()).collect());
        drop(w);
    }

    let _w = writer(&root);
    // a rebuild that ran is off the queue whether or not it fixed anything, or the next
    // drain starts from the same list. what these rebuilds queued in turn stays, and so
    // does what failed or was skipped, which never ran to the end
    let left_over: Vec<db::Queued> = db::read_queue(&root)
        .unwrap_or_default()
        .into_iter()
        .filter(|q| {
            let k = (q.name.clone(), q.target.clone());
            !want.contains(&k) || failed.iter().any(|i| want[*i] == k)
        })
        .collect();
    if let Err(e) = db::write_queue(&root, &left_over) {
        die(e.to_string());
    }

    let mut left = 0;
    for (t, f) in checks(&root) {
        if f.what.rebuilds() {
            say!("{} {t} {}", f.path, f.what);
            left += 1;
        }
    }
    batch_end(left == 0 && failed.is_empty());
    if left > 0 || !failed.is_empty() {
        std::process::exit(1);
    }
}

// every file a record holds, by content
fn hashes(m: &[db::Entry]) -> BTreeSet<(&str, &str)> {
    m.iter()
        .filter_map(|e| match &e.kind {
            db::Kind::File(h) => Some((e.path.as_str(), h.as_str())),
            _ => None,
        })
        .collect()
}

// one line per rebuild: what queued it, and whether it came out as the same bytes it
// replaced. one storm says little, several say whether the per-consumer filter queues
// too much. same is certain; changed can be a build that is not reproducible
fn rebuilt_log(root: &Path, name: &str, target: &str, why: &str, same: bool) {
    let at = root.join("var/kiry/log/rebuilds");
    let when = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = format!("{name} {target} {why} {} {when}\n", if same { "same" } else { "changed" });
    let _ = at.parent().map(fs::create_dir_all);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&at)
        .and_then(|mut f| f.write_all(line.as_bytes()));
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
            // a soname that left has no symbol to filter by, and a consumer that can
            // dlsym reaches names its undefined set never shows. both go whole
            let uses = seen.iter().any(|(_, s)| {
                let install::Seen::Elf(o) = s else {
                    return false;
                };
                o.needed.contains(&b.soname)
                    && (b.changed.is_empty()
                        || o.undefined.iter().any(|u| DL.contains(&u.name.as_str()))
                        || o.undefined
                            .iter()
                            .any(|u| moved.contains(symbol(u, versions).as_str())))
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
    compare_as(a, b, false)
}

// letters is what a scheme buys: a recipe that has one has said how its numbering goes,
// so text in the same place orders as text -- tzdata's 2026c before 2026d. without one
// that is exactly the guess the comparison refuses
fn compare_as(a: &str, b: &str, letters: bool) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().min(y.len()) {
        match (&x[i], &y[i]) {
            (Part::Num(p), Part::Num(q)) if p != q => return Some(p.cmp(q)),
            (Part::Num(_), Part::Num(_)) | (Part::Sep, Part::Sep) => {}
            (Part::Text(p), Part::Text(q)) if p == q => {}
            (Part::Text(p), Part::Text(q)) if letters => return Some(p.cmp(q)),
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

// what a bump is, read off the first component that moved -- 1.2.3 to 1.3.0 moved the
// second -- and what the scheme calls that component. a recipe without one reads as
// major minor patch, which is what most upstreams are. core gets no such default: a
// guess about the toolchain's numbering is what the scheme rule exists to prevent
fn level(dir: &Path, ours: &str, up: &str) -> Option<&'static str> {
    let scheme = plain(&dir.join("scheme")).into_iter().next();
    let core = dir.parent().and_then(|r| r.file_name()).is_some_and(|n| n == "core");
    if scheme.is_none() && core {
        return None;
    }
    let roles: Vec<String> = match &scheme {
        Some(s) => s.split_whitespace().map(String::from).collect(),
        None => ["major", "minor", "patch"].map(String::from).to_vec(),
    };
    let comps = |v: &str| -> Vec<Part> { parts(v).into_iter().filter(|p| !matches!(p, Part::Sep)).collect() };
    let (x, y) = (comps(ours), comps(up));
    let moved = (0..x.len().max(y.len())).find(|&i| match (x.get(i), y.get(i)) {
        (Some(Part::Num(a)), Some(Part::Num(b))) => a != b,
        (Some(Part::Text(a)), Some(Part::Text(b))) => a != b,
        (None, Some(Part::Num(0))) | (Some(Part::Num(0)), None) => false,
        _ => true,
    })?;
    // a component past the end of the scheme is a finer one than it names
    match roles.get(moved).map_or("patch", String::as_str) {
        "major" => Some("major"),
        "minor" => Some("minor"),
        "patch" => Some("patch"),
        _ => None,
    }
}

// how far a bump may go before a person promotes it. auto=always is the default and the
// rule: the fallback snapshot makes a bad install one reboot either way, so a gate here
// buys nothing it does not
fn policy_allows(dir: &Path, level: Option<&str>) -> Result<(), String> {
    let core = dir.parent().and_then(|r| r.file_name()).is_some_and(|n| n == "core");
    if core && !dir.join("scheme").is_file() {
        return Err("core recipes promote only with a scheme to say what the bump is".into());
    }
    let p = plain(&dir.join("policy")).into_iter().next().unwrap_or_else(|| "auto=always".into());
    let ok = match (p.as_str(), level) {
        ("auto=always", _) => true,
        ("auto=minor", Some("minor" | "patch")) | ("auto=patch", Some("patch")) => true,
        ("auto=minor" | "auto=patch" | "auto=none", _) => false,
        (other, _) => return Err(format!("policy {other} is not auto=always, minor, patch or none")),
    };
    match ok {
        true => Ok(()),
        false => Err(format!("{p} holds a {} bump for promote", level.unwrap_or("unclassified"))),
    }
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

// a tracker is a url, and a pattern on the next line when the body is more than a
// version on its own. steam's is a debian Packages file: the pattern's first group is
// the version, and the highest one any line gives is the answer
fn from_tracker(dir: &Path) -> Result<Up, &'static str> {
    let lines = plain(&dir.join("tracker"));
    let url = lines.first().ok_or("tracker is empty")?;

    let at = std::env::temp_dir().join(format!("kiry-tracker-{}", std::process::id()));
    let _ = fs::remove_file(&at);
    let mut c = fetcher(url, &at).map_err(|_| "no fetcher")?;
    run(c.stderr(Stdio::null()), url).map_err(|_| "tracker fetch failed")?;
    let body = fs::read_to_string(&at).map_err(|_| "tracker fetch wrote nothing")?;
    let _ = fs::remove_file(&at);

    let v = match lines.get(1) {
        Some(pat) => version_in(&body, pat)?,
        None => {
            let mut words = body.split_whitespace();
            let v = words.next().ok_or("tracker answered nothing")?;
            if words.next().is_some() || !v.starts_with(|c: char| c.is_ascii_digit()) {
                return Err("tracker body is not a version on its own, and no pattern says where one is");
            }
            v.to_string()
        }
    };
    Ok(Up {
        version: v,
        from: "tracker".to_string(),
    })
}

fn version_in(body: &str, pat: &str) -> Result<String, &'static str> {
    let rx = Rx::new(pat).map_err(|_| "tracker pattern does not parse")?;
    let mut best: Option<String> = None;
    for l in body.lines() {
        let Some(v) = rx.find(l).and_then(|c| c.into_iter().next()).filter(|v| !v.is_empty()) else {
            continue;
        };
        if best.as_deref().is_none_or(|b| compare(b, &v) == Some(std::cmp::Ordering::Less)) {
            best = Some(v);
        }
    }
    best.ok_or("tracker pattern matched nothing")
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
    // extra carries all of aports, which makes it a catalogue rather than a list of what
    // this machine keeps. asked about nothing in particular, only what is installed from
    // it is worth an answer -- the rest is a bump nobody would build, fetched for nothing
    let kept: HashSet<String> = db::targets(root)
        .unwrap_or_default()
        .iter()
        .flat_map(|t| db::installed(root, t).unwrap_or_default())
        .collect();
    for r in repos(root) {
        let repo = r
            .file_name()
            .map_or_else(String::new, |x| x.to_string_lossy().into_owned());
        let Ok(rd) = fs::read_dir(&r) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let Ok(p) = read_recipe(&d) else { continue };
            if !want.is_empty() && !want.contains(&p.name) {
                continue;
            }
            if want.is_empty() && repo == "extra" && !kept.contains(&p.name) {
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
                Some(u) => match compare_as(&p.version.upstream, &u.version, d.join("scheme").is_file()) {
                    Some(std::cmp::Ordering::Equal) => ("ok", String::new()),
                    Some(std::cmp::Ordering::Less) => (
                        "behind",
                        level(&d, &p.version.upstream, &u.version).unwrap_or("no scheme").to_string(),
                    ),
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
        // a bump waiting in testing/ that someone has edited since sync wrote it is the
        // review in progress. converting over it would throw that away, so it waits for
        // a promote or a delete. one nobody touched is converted again as before, and
        // one with no record of what sync wrote is taken as edited
        let wrote = fs::read_to_string(converted(&root, &r.name).join("testing")).ok();
        let now = fs::read_to_string(fresh.join("build")).ok();
        if now.is_some() && now != wrote {
            say!("{:w$} {:v$} -> {:u$}  held", r.name, r.ours, up.version);
            say!("  {} edited since sync wrote it, promote or delete it first", fresh.display());
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
                        let allowed = policy_allows(&old, level(&old, &r.ours, &up.version));
                        if let Err(why) = &allowed {
                            notes.push(why.clone());
                        }
                        if b.settled && b.differ.is_empty() && allowed.is_ok() {
                            match promote_one(&root, &list, &r.name) {
                                Ok(to) => outcome = format!("promoted  {}", to.display()),
                                Err(e) => notes.push(format!("not promoted {e}")),
                            }
                        }
                    }
                }
            }
        }
        // what testing/ holds now, carried and all, so the next sync can tell an edit
        if let Ok(b) = fs::read_to_string(fresh.join("build")) {
            let _ = fs::write(conv.join("testing"), b);
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
    // after the bumps, so what testing/ just received gets its entry in the same run. a
    // gentoo mirror that cannot be reached costs flags for a while, not the sync. only
    // with --net, which is what reaching out is for -- 150MB of md5-cache is not a thing
    // an offline sync against the aports clone should go and fetch -- or with KIRY_GENTOO
    // naming the source outright
    if !dry && (net || std::env::var_os("KIRY_GENTOO").is_some()) {
        match refresh_gentoo(&root, &list, &want) {
            Ok(0) => {}
            Ok(n) => say!("{n} gentoo entries written"),
            Err(e) => say!("gentoo {e}"),
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

const PORTAGE: &str = "rsync://rsync.gentoo.org/gentoo-portage/metadata/md5-cache/";

// gentoo's md5-cache mirrored under var, where everything regenerable lives, and the
// entry for each recipe's exact version copied into the recipe as its gentoo file. extra
// and testing are machine-written and get one wherever gentoo has the version; a
// hand-kept repo only has one refreshed where someone put one. KIRY_GENTOO names another
// place to rsync from, which is how the tests do it
fn refresh_gentoo(root: &Path, list: &[PathBuf], want: &[String]) -> Result<usize, String> {
    let cache = root.join("var/kiry/gentoo/md5-cache");
    mkdirs(&cache)?;
    let from = std::env::var("KIRY_GENTOO").unwrap_or_else(|_| PORTAGE.into());
    run(
        Command::new("rsync")
            .args(["-a", "--delete", "--exclude=Manifest*"])
            .arg(&from)
            .arg(format!("{}/", cache.display())),
        "rsync",
    )?;

    // lowercase name -> (category, pf) for every entry, read off the file names alone.
    // virtual/pipewire and acct-user/pipewire are not pipewire, and counted they would
    // make every name they share ambiguous
    let mut by: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for c in fs::read_dir(&cache).map_err(|e| format!("{}: {e}", cache.display()))?.flatten() {
        let cat = c.file_name().to_string_lossy().into_owned();
        if matches!(cat.as_str(), "virtual" | "acct-user" | "acct-group") {
            continue;
        }
        let Ok(rd) = fs::read_dir(c.path()) else { continue };
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if let Some((pn, _)) = pf(&f) {
                by.entry(pn.to_lowercase()).or_default().push((cat.clone(), f.clone()));
            }
        }
    }

    let mut wrote = 0;
    for repo in list {
        let machine = repo.file_name().is_some_and(|n| n == "extra" || n == "testing");
        let Ok(rd) = fs::read_dir(repo) else { continue };
        for e in rd.flatten() {
            let dir = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if !want.is_empty() && !want.contains(&name) {
                continue;
            }
            if !(machine || dir.join("gentoo").is_file()) || !dir.join("build").is_file() {
                continue;
            }
            let Ok(ver) = fs::read_to_string(dir.join("version")) else { continue };
            let Some(ver) = ver.split_whitespace().next() else { continue };
            let Some((cat, pf_)) = portage_entry(&by, &name, ver, &cache) else { continue };
            let text = fs::read_to_string(cache.join(&cat).join(&pf_)).map_err(|e| format!("{cat}/{pf_}: {e}"))?;
            let body = format!("# {cat}/{pf_}\n{text}");
            if fs::read_to_string(dir.join("gentoo")).ok().as_deref() == Some(body.as_str()) {
                continue;
            }
            fs::write(dir.join("gentoo"), body).map_err(|e| format!("{}: {e}", dir.display()))?;
            wrote += 1;
        }
    }
    Ok(wrote)
}

// the entry for a recipe: its name as gentoo spells it, in exactly one category, at
// exactly its version, keyworded for amd64 stable or testing. the highest -r of that
// version, since a revision is gentoo's fix and not a different upstream. anything less
// certain is no entry: a package with no flags for a while is better than one with the
// wrong ones
fn portage_entry(
    by: &HashMap<String, Vec<(String, String)>>,
    name: &str,
    ver: &str,
    cache: &Path,
) -> Option<(String, String)> {
    let lower = name.to_lowercase();
    let mut guesses = vec![(lower.clone(), None)];
    if let Some((p, rest)) = lower.split_once('-') {
        match p {
            "py3" => guesses.push((rest.to_string(), Some("dev-python"))),
            "perl" => guesses.push((rest.to_string(), Some("dev-perl"))),
            "qt6" => guesses.push((rest.to_string(), Some("dev-qt"))),
            _ => {}
        }
    }
    for (pn, cat) in guesses {
        let Some(all) = by.get(&pn) else { continue };
        let all: Vec<&(String, String)> = all.iter().filter(|(c, _)| cat.is_none_or(|w| c == w)).collect();
        let cats: HashSet<&String> = all.iter().map(|(c, _)| c).collect();
        if cats.len() != 1 {
            continue;
        }
        let rev = |f: &str| {
            f.rsplit_once("-r")
                .and_then(|(_, r)| r.parse::<u32>().ok())
                .unwrap_or(0)
        };
        let best = all
            .iter()
            .filter(|(_, f)| pf(f).is_some_and(|(_, v)| v == ver))
            .filter(|(c, f)| {
                fs::read_to_string(cache.join(c).join(f)).is_ok_and(|t| {
                    t.lines().any(|l| {
                        l.strip_prefix("KEYWORDS=")
                            .is_some_and(|k| k.split_whitespace().any(|w| w == "amd64" || w == "~amd64"))
                    })
                })
            })
            .max_by_key(|(_, f)| rev(f))?;
        return Some((best.0.clone(), best.1.clone()));
    }
    None
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
// a local source the bump lacks is this tree's own, unless the last conversion had it
// too, in which case alpine dropped it and it goes. with no last conversion to ask, it
// stays: a lost patch fails loudly, a kept one alpine dropped usually just re-applies
fn keep_local(old: &Path, fresh: &Path, base: Option<&Path>) -> Result<Vec<String>, String> {
    let alpines: Vec<String> = base
        .and_then(|b| fs::read_to_string(b).ok())
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("source=").map(String::from)))
        .map(|l| {
            l.trim_matches('"')
                .split_whitespace()
                .map(|w| w.split_once("::").map_or(w, |(n, _)| n).to_string())
                .collect()
        })
        .unwrap_or_default();
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
        if s.contains("://") || now.iter().any(|(n, _)| *n == s) || alpines.contains(&s) {
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
    let kept = keep_local(old, fresh, base)?;
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
    let _w = writer(&root);
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

// the two halves of the root pair, said from the machine they belong to. --root names a
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
    let (now, other) = match roots(args, "commit") {
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

    // nothing boots a root but the firmware menu, so getting here off the other entry is
    // someone choosing the fallback. no doctor gate: the root being left is the suspect
    if let Err(e) = commits(&num, &was) {
        die(e);
    }
    say!("{now} boots from now on  Boot{num}  {other} is the fallback and the next risky install replaces it");
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
    // no gate going this way. back to the fallback is the direction that needs no
    // permission, and the tree being left is the one under suspicion
    if let Err(e) = commits(&num, &order(&seen)) {
        die(e);
    }
    say!("{other} boots from now on  reboot to leave {now}, which becomes the fallback");
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
    // name-version-rev.target.log, and a name can hold a dash of its own: perl- starts
    // perl-dbd-mysql's logs too. so the version is what the recipe and the db say, and
    // only with neither is it whatever starts with a digit
    let mut vers: Vec<String> = Vec::new();
    if let Some(p) = recipe(&root, name).and_then(|d| read_recipe(&d).ok()) {
        vers.push(format!("{}-{}.", p.version.upstream, p.version.rev));
    }
    for t in db::targets(&root).unwrap_or_default() {
        if let Ok(r) = db::read(&root, &t, name) {
            vers.push(format!("{}-{}.", r.version.upstream, r.version.rev));
        }
    }
    let mine = |f: &str| {
        let Some(rest) = f.strip_prefix(name.as_str()).and_then(|r| r.strip_prefix('-')) else {
            return false;
        };
        match vers.is_empty() {
            true => rest.starts_with(|c: char| c.is_ascii_digit()),
            false => vers.iter().any(|v| rest.starts_with(v.as_str())),
        }
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        if !mine(&f) || !f.ends_with(".log") {
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
    let mut out: Vec<String> = Vec::new();

    for t in db::targets(&root).unwrap_or_default() {
        out.push(format!("installed {t} {}", db::installed(&root, &t).unwrap_or_default().len()));
    }

    let mut recipes = 0;
    for r in repos(&root) {
        let n = fs::read_dir(&r)
            .map(|rd| rd.flatten().filter(|e| e.path().join("build").is_file()).count())
            .unwrap_or(0);
        out.push(format!("recipes {} {n}", r.display()));
        recipes += n;
    }
    out.push(format!("recipes total {recipes}"));

    let (mut arts, mut bytes) = (0u64, 0u64);
    for a in listing(&root.join("var/kiry/cache")) {
        if a.extension().is_some_and(|x| x == "zst") {
            arts += 1;
            bytes += weigh(&a);
        }
    }
    out.push(format!("cache {arts} artifacts {}", size(bytes)));
    // the three that answer where the disk went, which one number over the cache
    // directory did not: the tarballs under it outweigh the artifacts in it
    for (what, at) in [
        ("sources", "var/kiry/cache/sources"),
        ("stage", "var/kiry/stage"),
        ("log", "var/kiry/log"),
        ("lto", "var/kiry/lto"),
        ("cargo", "var/kiry/cargo"),
        ("gomod", "var/kiry/gomod"),
    ] {
        out.push(format!("{what} {}", size(weigh(&root.join(at)))));
    }
    let past = history(&root);
    if !past.is_empty() {
        let mut rows: Vec<(&(String, String), &u64)> = past.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let names: HashSet<&str> = past.keys().map(|(n, _)| n.as_str()).collect();
        out.push(format!("built {} packages", names.len()));
        // the sum of everything's last build, which is what building it all again costs
        out.push(format!("world ~{}", clock(rows.iter().map(|(_, s)| **s).sum())));
        for ((n, t), s) in rows.iter().take(3) {
            out.push(format!("slowest {n} {t} {}", clock(**s)));
        }
    }
    // every build since they were first recorded, not the last of each like world
    let cpu: Vec<u64> = plain(&times(&root))
        .iter()
        .filter_map(|l| l.split_whitespace().nth(5)?.parse().ok())
        .collect();
    if !cpu.is_empty() {
        out.push(format!("cpu {} over {} builds", clock(cpu.iter().sum()), cpu.len()));
    }
    // the last note recovery wrote in each filter or settings file says how that package
    // ended: a rung it built at, or stuck. notes a person wrote are neither
    let mut files: Vec<PathBuf> = repos(&root)
        .iter()
        .flat_map(|r| fs::read_dir(r).into_iter().flatten().flatten())
        .map(|e| e.path().join("filter"))
        .collect();
    let pkg = fs::read_dir(root.join("etc/kiry/pkg"));
    files.extend(pkg.into_iter().flatten().flatten().map(|e| e.path()));
    let (mut recovered, mut stuck) = (0, 0);
    for f in files {
        let text = fs::read_to_string(&f).unwrap_or_default();
        let last = text
            .lines()
            .filter_map(|l| l.strip_prefix("# ")?.split_once(' ').map(|(_, w)| w))
            .filter(|w| w.starts_with("rung ") || w.starts_with("stuck on "))
            .last();
        match last {
            Some(w) if w.starts_with("stuck") => stuck += 1,
            Some(_) => recovered += 1,
            None => {}
        }
    }
    if recovered + stuck > 0 {
        out.push(format!("failures {recovered} recovered, {stuck} stuck"));
    }
    out.push(format!("queued {}", db::read_queue(&root).unwrap_or_default().len()));
    let log = fs::read_to_string(root.join("var/kiry/log/rebuilds")).unwrap_or_default();
    let rows: Vec<&str> = log.lines().collect();
    if !rows.is_empty() {
        let same = rows.iter().filter(|l| l.split(' ').nth(3) == Some("same")).count();
        out.push(format!("rebuilt {}, {same} to the same bytes", rows.len()));
    }
    // modules whose object came out of the cache over every module a link needed, from
    // each build that linked through it
    let lto = fs::read_to_string(root.join("var/kiry/log/thinlto")).unwrap_or_default();
    let (hit, all) = lto
        .lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split(' ').collect();
            Some((w.get(2)?.parse::<u64>().ok()?, w.get(3)?.parse::<u64>().ok()?))
        })
        .fold((0, 0), |(h, a), (x, y)| (h + x, a + x + y));
    if all > 0 {
        out.push(format!("thinlto {}% of {all} modules", hit * 100 / all));
    }
    with_art(&root, "idle", &out);
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
    let _w = (!dry).then(|| writer(&root));
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
        // a build holds its lock for as long as it runs, however long ago it last wrote.
        // a stage dir is <pkg>-<ver>-<rev>.<target> and its lock drops the target
        let name = e.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lock = match name.ends_with(".lock") {
            true => e.clone(),
            false => e.with_file_name(format!("{}.lock", name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s))),
        };
        if locked(&lock) {
            busy += 1;
            continue;
        }
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
    // a pack killed partway leaves its .part. one still packing holds its build's lock
    // until the rename, so a .part nobody holds is dead however new it is
    let mut packing = 0;
    for a in listing(&var.join("cache")) {
        let name = a.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let Some(stem) = name.strip_suffix(".tar.zst.part") else {
            continue;
        };
        // the lock drops the target, the way a stage dir's does
        let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
        let lock = var.join("stage").join(format!("{stem}.lock"));
        match locked(&lock) {
            true => packing += 1,
            false => doomed.push(a),
        }
    }
    sweep("cache", doomed, keep.len() + packing);

    // a tarball no recipe names any more. the recipes are the only thing that decides
    // this, which is why sources is a cache and not a store
    let mut live: HashSet<String> = HashSet::new();
    let mut recipes = 0;
    for r in repos(&root) {
        for d in listing(&r) {
            let Ok(p) = read_recipe(&d) else { continue };
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
    let (gone, kept): (Vec<PathBuf>, Vec<PathBuf>) = listing(&var.join("cache/sources"))
        .into_iter()
        .partition(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !live.contains(n))
        });
    sweep("sources", gone, kept.len());

    // a log for a version no recipe builds any more. kiry log answers for the current
    // one, and nothing reads the others
    let mut now_building: HashSet<String> = HashSet::new();
    let mut named: HashSet<String> = HashSet::new();
    for r in repos(&root) {
        for d in listing(&r) {
            if let Ok(p) = read_recipe(&d) {
                named.insert(p.name.clone());
                now_building.insert(format!(
                    "{}-{}-{}.",
                    p.name, p.version.upstream, p.version.rev
                ));
            }
        }
    }
    // build logs and nothing else. rebuilds sits beside them and is what stats reads
    let (old, kept): (Vec<PathBuf>, Vec<PathBuf>) = listing(&var.join("log"))
        .into_iter()
        .filter(|f| f.extension().is_some_and(|e| e == "log"))
        .partition(|f| {
            f.file_name().and_then(|n| n.to_str()).is_none_or(|n| {
                !now_building.iter().any(|pre| n.starts_with(pre.as_str()))
            })
        });
    sweep("log", old, kept.len());

    // an entry is only ever read by the next build of the same package under the same
    // llvm, so a directory for another llvm or for a package no recipe builds is dead.
    // inside a live one lld prunes for itself
    let tag = lto_tag(&root);
    let (mut dead, mut warm) = (Vec::new(), 0);
    for d in listing(&var.join("lto")) {
        let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !named.contains(&name) {
            dead.push(d);
            continue;
        }
        for g in listing(&d) {
            match g.file_name().is_some_and(|n| n == tag.as_str()) {
                true => warm += 1,
                false => dead.push(g),
            }
        }
    }
    sweep("lto", dead, warm);

    // a preserved library goes when nothing names its soname any more and nothing
    // running still has it open. either alone is the wrong answer: a library no
    // installed elf references can be mapped by a process using it right now, and one
    // nothing has mapped is still what the next start of that program will open
    let live = root.canonicalize().as_deref().unwrap_or(&root) == Path::new("/");
    // nothing runs out of a staging root, so there is nothing there to be blind about
    let (open, blind) = if live { mapped() } else { (HashMap::new(), 0) };
    let mut doomed = Vec::new();
    let mut kept_back = 0;
    for t in db::targets(&root).unwrap_or_default() {
        // dt_needed is recorded nowhere, so the only way to know a soname is still
        // wanted is to go and read every elf that could want it
        let mut wanted: HashSet<String> = HashSet::new();
        for name in db::installed(&root, &t).unwrap_or_default() {
            let Ok(rec) = db::read(&root, &t, &name) else {
                continue;
            };
            for (_, what) in install::scan(&root, &rec.manifest).unwrap_or_default() {
                if let install::Seen::Elf(o) = what {
                    wanted.extend(o.needed);
                }
            }
        }
        for name in db::preserving(&root, &t).unwrap_or_default() {
            let Ok(m) = db::read_preserved(&root, &t, &name) else {
                continue;
            };
            // the same rule removal uses: a file that no longer hashes to what was
            // written down was touched by hand and is not ours to delete
            let edited: HashSet<String> =
                install::modified(&root, &m).unwrap_or_default().into_iter().collect();
            let groups = generations(&root, &m);
            // an entry no library in the record answers for is left standing rather than
            // guessed at, which is also what stops the record being dropped early
            let grouped: HashSet<&String> = groups.iter().flat_map(|(_, e)| e).map(|e| &e.path).collect();
            let mut stay: Vec<db::Entry> =
                m.iter().filter(|e| !grouped.contains(&e.path)).cloned().collect();
            for (soname, entries) in groups {
                let busy = entries.iter().any(|e| open.contains_key(&e.path));
                let touched = entries.iter().any(|e| edited.contains(&e.path));
                if blind > 0 || busy || touched || wanted.contains(&soname) {
                    kept_back += 1;
                    stay.extend(entries);
                    continue;
                }
                say!("{name} {t} {soname} released");
                doomed.extend(entries.iter().map(|e| root.join(&e.path)));
            }
            if !dry && stay.len() != m.len() {
                stay.sort_by(|a, b| a.path.cmp(&b.path));
                if let Err(e) = db::write_preserved(&root, &t, &name, &stay) {
                    die(e.to_string());
                }
            }
        }
    }
    if blind > 0 {
        say!("{blind} processes could not be read, so nothing preserved was released");
    }
    sweep("preserved", doomed, kept_back);
}

// a record holds two generations when a second soname bump lands before the queue
// drains, so what gets released is one soname and not everything it ever kept
fn generations(root: &Path, m: &[db::Entry]) -> Vec<(String, Vec<db::Entry>)> {
    let mut of: HashMap<String, String> = HashMap::new();
    for (path, what) in install::scan(root, m).unwrap_or_default() {
        if let install::Seen::Elf(o) = what {
            if let Some(so) = o.soname {
                of.insert(path, so);
            }
        }
    }
    // one hop, which is the shape a soname link has. a longer chain leaves its links
    // ungrouped and they stay, which is the safe direction to be wrong in
    let mut by: HashMap<String, Vec<db::Entry>> = HashMap::new();
    for e in m {
        let who = match &e.kind {
            db::Kind::Link(t) => install::beside(&e.path, t).and_then(|p| of.get(&p).cloned()),
            _ => of.get(&e.path).cloned(),
        };
        if let Some(so) = who {
            by.entry(so).or_default().push(e.clone());
        }
    }
    let mut out: Vec<(String, Vec<db::Entry>)> = by.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
    // what this batch derives outranks what an earlier one wrote, and neither outranks a
    // pair somebody kept by hand. so the file convert wrote is read apart from the others
    let table = out.join("aliases");
    let hand_kept: Vec<PathBuf> = repos
        .iter()
        .filter(|r| !convert::generated(&r.join("aliases")))
        .cloned()
        .collect();
    let hand = convert::aliases(&hand_kept);
    let before = match convert::generated(&table) {
        true => convert::aliases(std::slice::from_ref(&out)),
        false => HashMap::new(),
    };
    let files: Vec<&Path> = names.iter().map(Path::new).collect();
    let derived = convert::merged(&hand, &convert::parents(&files), &before);
    let mut alias = derived.clone();
    alias.extend(hand);
    if table.exists() && !convert::generated(&table) {
        say!("{} is kept by hand, so the subpackage table was not written", table.display());
    } else if !derived.is_empty() {
        if let Err(e) = mkdirs(&out).and_then(|_| convert::write_parents(&table, &derived)) {
            die(e);
        }
    }
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
        // the manifest holds paths relative to the root as the package spelled them, and
        // a caller types whatever they were just shown, /bin/sh as often as /usr/bin/sh
        let typed = lexical(want).join("/");
        let real = spelled(&root, want);
        let hit = [&typed, &real].into_iter().find_map(|k| owner.get(k.as_str()).map(|w| (k, w)));
        match hit {
            Some((key, who)) => {
                for (name, t) in who {
                    say!("/{key} {name} {t}");
                }
            }
            None if fs::symlink_metadata(root.join(&real)).is_err() => {
                say!("/{real} does not exist");
                missed = true;
            }
            None => {
                say!("/{real} owned by nobody");
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
    there(&root);

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    let mut found = 0;
    let (all, waiting) = checks_without(&root, &[]);
    for (t, f) in all {
        say!("{} {t} {}", f.path, f.what);
        found += 1;
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
    // not a fault and not counted as one, but an ignore list that hides things is worse
    // than the noise it removes. these are libraries the tree has moved past and is
    // carrying until their consumers rebuild
    let held: usize = targets
        .iter()
        .flat_map(|t| db::preserving(&root, t).unwrap_or_default().into_iter().map(move |n| (t, n)))
        .filter_map(|(t, n)| db::read_preserved(&root, t, &n).ok())
        .flatten()
        .filter(|e| matches!(e.kind, db::Kind::File(_)))
        .count();
    match (held, waiting) {
        (0, _) => {}
        (_, 0) => say!("{held} preserved and nothing links them. kiry gc takes them unless still open"),
        _ => say!("{held} preserved, waiting on a rebuild. kiry gc takes them when nothing asks"),
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
        for name in db::preserving(root, t).unwrap_or_default() {
            let Ok(m) = db::read_preserved(root, t, &name) else {
                continue;
            };
            for p in install::modified(root, &m).unwrap_or_default() {
                out.push(Finding {
                    pkg: name.clone(),
                    path: p,
                    what: What::Modified,
                });
            }
        }
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
        // kiry put these here and kiry will take them away again, so an orphan is what
        // they are not
        for name in db::preserving(root, t).unwrap_or_default() {
            if let Ok(m) = db::read_preserved(root, t, &name) {
                owned.extend(m.into_iter().map(|e| e.path));
            }
        }
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
    // the hash and nothing else: a whole record is its manifest, and llvm's is 3567 lines
    for t in targets {
        for name in db::installed(root, t).unwrap_or_default() {
            let hash = fs::read_to_string(db::dir(root, t, &name).join("hash")).unwrap_or_default();
            let hash = hash.trim();
            if !hash.is_empty() {
                seen.entry(name).or_default().push((t.clone(), hash.to_string()));
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
fn missing_users(root: &Path, users: &[(String, Vec<String>)]) -> Vec<Finding> {
    let passwd = fs::read_to_string(root.join("etc/passwd")).unwrap_or_default();
    let have: HashSet<&str> = passwd.lines().filter_map(|l| l.split(':').next()).collect();

    let mut out = Vec::new();
    for (name, wants) in users {
        for u in wants.iter().filter(|u| !have.contains(u.as_str())) {
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
    each(names.len(), |i| {
        db::read(root, target, &names[i])
            .map_err(|e| e.to_string())
            .and_then(|rec| {
                install::scan(root, &rec.manifest)
                    .map_err(|e| e.to_string())
                    .map(|seen| (rec, seen))
            })
    })
}

// f over 0..n on every thread there is, the results in index order. a work queue rather
// than chunks, bc one package is llvm and the next is a font
fn each<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..jobs().min(n) {
            let (tx, next, f) = (tx.clone(), &next, &f);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    return;
                }
                let _ = tx.send((i, f(i)));
            });
        }
    });
    drop(tx);
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
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

type Script = (String, String, Vec<String>);

// one target's findings, and what the shebang check needs from it: its scripts and the
// index of every path it owns. checks() runs that last, over all targets at once
//
// gone is packages to read as already removed, which is how r asks what a removal
// would break. the count is preserved libraries something still links
fn check(root: &Path, target: &str, gone: &[String]) -> (Vec<Finding>, Vec<Script>, World, usize) {
    let Some(dirs) = defaults(target) else {
        let bad = Finding {
            pkg: "-".into(),
            path: target.to_string(),
            what: What::UnknownTarget,
        };
        return (vec![bad], Vec::new(), (HashSet::new(), HashMap::new()), 0);
    };

    let names: Vec<String> = match db::installed(root, target) {
        Ok(n) => n.into_iter().filter(|n| !gone.contains(n)).collect(),
        Err(e) => die(e.to_string()),
    };

    let mut elves = Vec::new();
    // index-aligned with elves, the way here already is
    let mut owners: Vec<String> = Vec::new();
    let mut here: HashMap<String, usize> = HashMap::new();
    let mut links: HashMap<String, String> = HashMap::new();
    let mut shebangs: Vec<Script> = Vec::new();
    // every regular file, not only the ones that parse as elf: an interpreter is a file
    // a hardlink is one too -- the same inode under a second name, which is how perl
    // ships /usr/bin/perl beside perl5.44.0
    let mut present: HashSet<String> = HashSet::new();
    let mut out: Vec<Finding> = Vec::new();
    // every installed name of this target is in names, so this is the whole graph and
    // nothing has to go back to disk for a depends the scan already read
    let mut deps: HashMap<String, Vec<Dep>> = HashMap::new();
    let mut users: Vec<(String, Vec<String>)> = Vec::new();

    for (name, one) in names.iter().zip(scans(root, target, &names)) {
        let (mut rec, seen) = match one {
            Ok(x) => x,
            Err(e) => die(e),
        };
        deps.insert(name.clone(), std::mem::take(&mut rec.depends));
        users.push((name.clone(), std::mem::take(&mut rec.users)));
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

    // a preserved library belongs to no installed record, and doctor builds its index
    // out of what records claim. without this every consumer of a soname the tree has
    // moved past reads unresolved and every symbol behind it reads missing -- 47 rows
    // against electron that were one fact
    let kept = db::preserving(root, target).unwrap_or_default();
    let first_kept = elves.len();
    let read = each(kept.len(), |i| {
        let m = db::read_preserved(root, target, &kept[i]).ok()?;
        let seen = install::scan(root, &m);
        Some((m, seen))
    });
    for (name, one) in kept.into_iter().zip(read) {
        let Some((m, seen)) = one else { continue };
        index(&m, &mut present, &mut links);
        let Ok(seen) = seen else { continue };
        for (path, what) in seen {
            let install::Seen::Elf(o) = what else { continue };
            here.insert(fold(&path), elves.len());
            owners.push(name.clone());
            elves.push((path, o));
        }
    }

    let sets = exported(&elves);
    let mut linked: HashSet<usize> = HashSet::new();
    let mut needs: Vec<Vec<usize>> = vec![Vec::new(); elves.len()];
    let mut open: Vec<(usize, String)> = Vec::new();
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
                None => open.push((i, want.clone())),
            }
        }
    }

    // a library that only its own package's code ever loads can lean on that package
    // having loaded something first. every native library of a jdk names libjvm.so, which
    // sits in server/ where none of their $ORIGIN rpaths look, and they resolve it
    // because the jvm is already in the process when System.loadLibrary opens them. an
    // executable gets no such pass -- nothing is loaded ahead of it -- and neither does a
    // library some other package links, which is what the search path is for
    for (i, want) in open {
        let theirs = |k: usize| owners[k] == owners[i];
        let inside = !elves[i].1.interp && (0..elves.len()).all(|k| !needs[k].contains(&i) || theirs(k));
        let j = (0..elves.len()).find(|&j| theirs(j) && elves[j].1.soname.as_deref() == Some(want.as_str()));
        match j.filter(|_| inside) {
            Some(j) => {
                linked.insert(j);
                needs[i].push(j);
            }
            None => out.push(Finding {
                pkg: owners[i].clone(),
                path: elves[i].0.clone(),
                what: What::Unresolved(want),
            }),
        }
    }

    out.extend(accounted(target, &deps, &elves, &owners, &needs));
    let waiting = (first_kept..elves.len()).filter(|j| linked.contains(j)).count();

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
    // the programs each library ends up in. load order only decides something when both
    // are in one process: ffmpeg 7 and 8 sit side by side for their own consumers, and
    // two jdks' libjli never meet
    let mut into: Vec<Vec<usize>> = vec![Vec::new(); elves.len()];
    for (e, _) in elves.iter().enumerate().filter(|(_, (_, o))| o.interp) {
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stack = vec![e];
        while let Some(i) = stack.pop() {
            if seen.insert(i) {
                into[i].push(e);
                stack.extend(needs[i].iter().copied());
            }
        }
    }
    for ((a, b), n) in dupes {
        if !into[a].iter().any(|e| into[b].binary_search(e).is_ok()) {
            continue;
        }
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
    out.extend(missing_users(root, &users));
    (out, shebangs, (present, links), waiting)
}

fn checks(root: &Path) -> Vec<(String, Finding)> {
    checks_without(root, &[]).0
}

// every target's findings. the shebang check wants every target's files, and each
// check() indexed its own already, so their union is that and no record is read twice
fn checks_without(root: &Path, gone: &[(String, String)]) -> (Vec<(String, Finding)>, usize) {
    let mut out = Vec::new();
    let mut waiting = 0;
    let (mut anywhere, mut anylinks) = (HashSet::new(), HashMap::new());
    let mut scripts = Vec::new();
    for t in db::targets(root).unwrap_or_default() {
        let mine: Vec<String> =
            gone.iter().filter(|(gt, _)| *gt == t).map(|(_, n)| n.clone()).collect();
        let (found, shebangs, (present, links), linked) = check(root, &t, &mine);
        waiting += linked;
        out.extend(found.into_iter().map(|f| (t.clone(), f)));
        anywhere.extend(present);
        anylinks.extend(links);
        scripts.extend(shebangs.into_iter().map(|s| (t.clone(), s)));
    }
    // the kernel will not start a script whose interpreter is not there, which is the
    // same failure DT_NEEDED describes and nothing was checking it
    for (t, (pkg, path, words)) in &scripts {
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
            out.push((
                t.clone(),
                Finding {
                    pkg: pkg.clone(),
                    path: path.clone(),
                    what: What::NoInterpreter(words.join(" ")),
                },
            ));
        }
    }
    (out, waiting)
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
    let mut parts = lexical(p);
    if matches!(parts.first().map(String::as_str), Some("lib" | "lib64" | "bin" | "sbin")) {
        parts.insert(0, "usr".into());
    }
    parts.join("/")
}

fn lexical(p: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c.to_string()),
        }
    }
    parts
}

// a path through whatever directory links the root really has, read from it rather than
// assumed the way fold does, because owns answers for one root on disk. the last part is
// never followed: a symlink a package ships is owned as itself
fn spelled(root: &Path, p: &str) -> String {
    let mut parts = lexical(p);
    let (mut i, mut hops) = (0, 0);
    while i + 1 < parts.len() {
        match fs::read_link(root.join(parts[..=i].join("/"))) {
            Ok(to) if hops < 40 => {
                hops += 1;
                let to = to.to_string_lossy();
                let base = if to.starts_with('/') { String::new() } else { parts[..i].join("/") };
                parts = lexical(&format!("{base}/{to}/{}", parts[i + 1..].join("/")));
                i = 0;
            }
            _ => i += 1,
        }
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
    let p = match read_recipe(&dir) {
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

    // the second risky install in a boot keeps the fallback the first one took. the
    // device does not exist, so reaching for the snapshot at all fails even as root and
    // the test says so, and never gets near a real subvolume
    #[test]
    fn a_fallback_is_taken_once_a_boot() {
        let fake = REAL.replace("/dev/mapper/cryptroot", "/dev/kiry-test-none");
        let at = std::env::temp_dir().join(format!("kiry-taken-{}", std::process::id()));
        let _ = fs::remove_file(&at);
        assert!(fallback(&fake, &at).is_err(), "it did not try to snapshot");
        fs::write(&at, "@root-b\n").unwrap();
        assert_eq!(fallback(&fake, &at), Ok(()));
        // a root outside the pair has no other half to hold a fallback, marker or not
        let third = fake.replace("subvol=/@root-a", "subvol=/@root-c");
        assert!(fallback(&third, &at).is_err());
        let _ = fs::remove_file(&at);
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

    // a fallback is decided for / and these rules never fire anywhere else, so they are
    // reached directly rather than through a command that would refuse first
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

    // rrdtool's python binding died on a line with no keyword in it, and the warning above
    // it said not found
    #[test]
    fn blame_takes_the_line_before_makes_first_failure_over_a_warning() {
        let log = "configure.ac:320: warning: macro 'AM_GNU_GETTEXT_VERSION' not found in library\n\
                   make[3]: *** [Makefile:9: doc] Error 1 (ignored)\n\
                   CC       librrdupd_la-rrd_error.lo\n\
                   The setup requires setuptools.\n\
                   make[2]: *** [Makefile:781: python/wheel.stamp] Error 1\n\
                   make: *** [Makefile:513: all-recursive] Error 1\n";
        assert_eq!(blame(log), "The setup requires setuptools.");
    }

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

    // tables on disk carry a retry column, and a file kiry reads has to keep working
    // across kiry versions
    #[test]
    fn a_table_with_the_old_retry_column_still_reads() {
        let at = std::env::temp_dir().join(format!("kiry-oldtable-{}", std::process::id()));
        let d = at.join("etc/kiry/failures.d");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("10-local"), "No zipfiles found\tnotaflag source-missing\tclean\n").unwrap();
        let rs = rules(&at).unwrap();
        let f = scan(&rs, "unzip: No zipfiles found.").unwrap();
        assert_eq!(f.act, "notaflag source-missing");
        let f = scan(&rs, "ld.lld: error: out of memory").unwrap();
        assert_eq!(f.act, "set KIRY_THINLTO_JOBS /2");
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
    fn a_row_missing_its_action_is_refused() {
        let root = scratch("badrow");
        let d = root.join("etc/kiry/failures.d");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("10-bad"), "boom set LTO none\n").unwrap();
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

    // a setting with nothing after it reads as one somebody forgot to finish
    #[test]
    fn no_rung_writes_a_setting_without_a_value() {
        for (filter, l) in LADDER {
            let v = l.split_once(' ').map_or("", |(_, v)| v.trim());
            assert!(*filter || !v.is_empty(), "{l} has no value");
        }
    }

    // what the ladder wrote goes, header and all. what the table wrote and what a person
    // wrote stay, and so does a hand comment sitting on top of a rung
    #[test]
    fn bisect_takes_out_the_rungs_and_nothing_else() {
        let had = "\
# seeded from the portage grep
filter-flags -fno-plt
# the upstream build hates lto
# 2026-09-12 rung 1 after check failed on x86_64-musl
filter-lto
# 2026-09-20 link failed, recompile with -fPIC
#   relocation R_X86_64_32 cannot be used; recompile with -fPIC
append-flags -fPIC
# 2026-09-24 rung 3 after compile failed on x86_64-musl
#   foo.c:3:9: error: not on this cpu
CFLAGS_MARCH
# 2026-09-24 stuck on x86_64-gnu: the ladder ran out

";
        assert_eq!(
            unrung(had),
            "\
# seeded from the portage grep
filter-flags -fno-plt
# the upstream build hates lto
# 2026-09-20 link failed, recompile with -fPIC
#   relocation R_X86_64_32 cannot be used; recompile with -fPIC
append-flags -fPIC
# 2026-09-24 stuck on x86_64-gnu: the ladder ran out

"
        );
        assert_eq!(unrung("# 2026-09-12 rung 1 after compile failed on x86_64-musl\nfilter-lto\n"), "");
        // a rung whose line somebody deleted by hand leaves its header over the next
        // entry, and it is the header nearest a line that says whose the line is
        let dangling = "\
# 2026-09-12 rung 2 after compile failed on x86_64-musl
# 2026-09-20 link failed, recompile with -fPIC
append-flags -fPIC
";
        assert_eq!(unrung(dangling), dangling);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod sidecar {
    use super::*;

    // alpine derives a runtime dep from the sonames its build produced; a converted
    // recipe carries only what makedepends said. the sidecar is where the two meet, so
    // a library the build linked lands there whether the recipe named it or not
    #[test]
    fn a_library_the_build_linked_lands_in_the_sidecar() {
        let at = std::env::temp_dir()
            .join(format!("kiry-meta-{}", std::process::id()))
            .join("derived");
        let _ = fs::remove_dir_all(&at);
        mkdirs(&at).unwrap();

        let declared = pkg::Dep { name: "zlib".into(), make: false, host: false, only: None };
        let only_gnu = pkg::Dep {
            name: "argp".into(),
            make: false,
            host: false,
            only: Some("musl".into()),
        };
        let recipe = at.join("thing");
        mkdirs(&recipe).unwrap();
        fs::write(recipe.join("version"), "1.0 1\n").unwrap();
        let p = Package {
            name: "thing".into(),
            dir: recipe,
            version: pkg::Version::parse("1.0 1").unwrap(),
            sources: Vec::new(),
            checksums: Vec::new(),
            depends: vec![declared, only_gnu],
            targets: vec!["x86_64-gnu".into()],
            users: Vec::new(),
        };

        let art = at.join("thing-1.0-1.x86_64-gnu.tar.zst");
        let linked = vec!["libpcre2".to_string(), "zlib".to_string()];
        let f = Flags {
            cflags: String::new(),
            cxxflags: String::new(),
            ldflags: String::new(),
            rustflags: String::new(),
            use_flags: Vec::new(),
            env: Vec::new(),
            from: Vec::new(),
        };
        meta(&at, &p, "x86_64-gnu", "0", &f, &art, &linked).unwrap();

        let got = fs::read_to_string(at.join("thing-1.0-1.x86_64-gnu.tar.zst.meta/depends")).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        // zlib was declared and linked, and is one line either way
        assert_eq!(lines, vec!["zlib", "libpcre2"], "{got}");
    }

    // a make line the build linked through is how most of extra reads, and it kept the
    // make: 32 installed packages linked a library their record called build-only
    #[test]
    fn a_build_only_dep_the_build_linked_becomes_a_runtime_one() {
        let at = std::env::temp_dir()
            .join(format!("kiry-meta-{}", std::process::id()))
            .join("promoted");
        let _ = fs::remove_dir_all(&at);
        mkdirs(&at).unwrap();

        let dep = |name: &str, make: bool, host: bool, only: Option<&str>| pkg::Dep {
            name: name.into(),
            make,
            host,
            only: only.map(String::from),
        };
        let recipe = at.join("thing");
        mkdirs(&recipe).unwrap();
        fs::write(recipe.join("version"), "1.0 1\n").unwrap();
        let p = Package {
            name: "thing".into(),
            dir: recipe,
            version: pkg::Version::parse("1.0 1").unwrap(),
            sources: Vec::new(),
            checksums: Vec::new(),
            depends: vec![
                dep("libpng", true, true, None),
                dep("cmake", true, true, None),
                dep("argp", true, false, Some("musl")),
            ],
            targets: vec!["x86_64-musl".into()],
            users: Vec::new(),
        };

        let art = at.join("thing-1.0-1.x86_64-musl.tar.zst");
        let linked = vec!["argp".to_string(), "libpng".to_string()];
        let f = Flags {
            cflags: String::new(),
            cxxflags: String::new(),
            ldflags: String::new(),
            rustflags: String::new(),
            use_flags: Vec::new(),
            env: Vec::new(),
            from: Vec::new(),
        };
        meta(&at, &p, "x86_64-musl", "0", &f, &art, &linked).unwrap();

        let got =
            fs::read_to_string(at.join("thing-1.0-1.x86_64-musl.tar.zst.meta/depends")).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines, vec!["libpng", "cmake make", "argp musl"], "{got}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod provenance {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-drift-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(root: &Path, t: &str, name: &str, hash: &str) {
        db::write(
            root,
            &db::Installed {
                name: name.into(),
                target: t.into(),
                version: pkg::Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: Vec::new(),
                hash: hash.into(),
                users: Vec::new(),
                flags: Vec::new(),
            },
        )
        .unwrap();
    }

    // the hash every artifact carries is what makes "the two graphics stacks cannot
    // drift" a check and not a design claim. a bump that reached one target and not the
    // other is what it catches
    #[test]
    fn a_package_whose_targets_were_built_from_different_sources_is_a_finding() {
        let at = root("mismatch");
        let ts = ["x86_64-musl".to_string(), "x86_64-gnu".to_string()];
        rec(&at, &ts[0], "mesa", "aaa");
        rec(&at, &ts[1], "mesa", "aaa");
        rec(&at, &ts[0], "zlib", "bbb");
        rec(&at, &ts[1], "zlib", "ccc");

        let found = drift(&at, &ts);
        let names: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, vec!["zlib"], "{names:?}");
    }

    // a record written before kiry recorded one says nothing rather than no, the same
    // call the cache key makes
    #[test]
    fn a_record_with_no_hash_is_not_drift() {
        let at = root("nohash");
        let ts = ["x86_64-musl".to_string(), "x86_64-gnu".to_string()];
        rec(&at, &ts[0], "mesa", "aaa");
        rec(&at, &ts[1], "mesa", "");

        assert!(drift(&at, &ts).is_empty());
    }

    // one target of a two-target recipe is the ordinary case for a package that only
    // builds on one, and nothing to report
    #[test]
    fn a_package_on_one_target_alone_is_not_drift() {
        let at = root("single");
        let ts = ["x86_64-musl".to_string(), "x86_64-gnu".to_string()];
        rec(&at, &ts[0], "dwl", "aaa");

        assert!(drift(&at, &ts).is_empty());
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod names {
    use super::*;

    const T: &str = "x86_64-musl";

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-names-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn recipe(root: &Path, repo: &str, name: &str, depends: &str) -> PathBuf {
        let d = root.join(REPOS_AT).join(repo).join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("version"), "1.0 1\n").unwrap();
        fs::write(d.join("targets"), format!("{T}\n")).unwrap();
        fs::write(d.join("build"), "true\n").unwrap();
        fs::write(d.join("depends"), depends).unwrap();
        d
    }

    fn installed(root: &Path, name: &str, files: &[&str], sonames: &[&str]) {
        installed_on(root, T, name, files, sonames)
    }

    fn installed_on(root: &Path, t: &str, name: &str, files: &[&str], sonames: &[&str]) {
        db::write(
            root,
            &db::Installed {
                name: name.into(),
                target: t.into(),
                version: pkg::Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: files
                    .iter()
                    .map(|f| db::Entry {
                        mode: 0o755,
                        kind: db::Kind::File("0".repeat(64)),
                        path: (*f).to_string(),
                    })
                    .collect(),
                hash: String::new(),
                users: Vec::new(),
                flags: Vec::new(),
            },
        )
        .unwrap();
        let ps: Vec<db::Provide> = sonames
            .iter()
            .map(|s| db::Provide {
                soname: (*s).to_string(),
                versioned: false,
                path: format!("usr/lib/{s}"),
            })
            .collect();
        db::write_provides(root, t, name, &ps).unwrap();
    }

    fn deps(p: &Package) -> Vec<String> {
        p.depends.iter().map(ToString::to_string).collect()
    }

    // a virtual is a choice, and the pair in an aliases file is where it was made. it
    // counts when the recipe is read, not only when one is next converted
    #[test]
    fn an_alias_decides_what_a_virtual_means_when_the_recipe_is_read() {
        let r = root("virtual");
        let at = recipe(&r, "extra", "prismlauncher", "java-jre\nopenjdk8 make\n");
        recipe(&r, "extra", "openjdk21", "");
        fs::create_dir_all(r.join(REPOS_AT).join("local")).unwrap();
        fs::write(r.join(REPOS_AT).join("local/aliases"), "java-jre\topenjdk21\n").unwrap();

        let p = load(&r, &at).unwrap();
        assert_eq!(deps(&p), vec!["openjdk21", "openjdk8 make"]);
    }

    // what is installed says who ships a library, a command or a .pc before anything
    // alpine wrote down does
    #[test]
    fn a_library_a_command_and_a_pc_file_resolve_to_what_ships_them() {
        let r = root("owners");
        installed(&r, "mesa", &["usr/lib/libGL.so.1"], &["libGL.so.1"]);
        installed(&r, "ncurses", &["usr/bin/tput", "usr/lib/pkgconfig/ncursesw.pc"], &[]);
        let at = recipe(&r, "extra", "app", "so:libGL.so.1\ncmd:tput\npc:ncursesw make\n");

        let p = load(&r, &at).unwrap();
        assert_eq!(deps(&p), vec!["mesa", "ncurses", "ncurses make"]);
    }

    // one soname, two packages: mesa on musl and libglvnd on gnu. gnu sorts first, so a
    // lookup across targets would hand firefox on musl the gnu one
    #[test]
    fn a_library_resolves_to_what_ships_it_on_the_target_asking() {
        let r = root("per-target");
        installed_on(&r, "x86_64-gnu", "libglvnd", &[], &["libGL.so.1"]);
        installed_on(&r, "x86_64-musl", "mesa", &[], &["libGL.so.1"]);
        let at = recipe(&r, "extra", "firefox", "so:libGL.so.1\n");
        assert_eq!(deps(&load(&r, &at).unwrap()), vec!["mesa"]);

        // a recipe for both gets both answers, each held to its own target
        fs::write(at.join("targets"), "x86_64-musl\nx86_64-gnu\n").unwrap();
        assert_eq!(
            deps(&load(&r, &at).unwrap()),
            vec!["mesa x86_64-musl", "libglvnd x86_64-gnu"]
        );
    }

    // nothing installed has the command, and a recipe named after it is the likely
    // provider. a library has no such name to guess from, so it stays as alpine wrote it
    // and the plan says so
    #[test]
    fn nothing_installed_falls_back_to_a_recipe_by_that_name_and_no_further() {
        let r = root("guess");
        recipe(&r, "extra", "emacs", "");
        let at = recipe(&r, "extra", "emacs-magit", "cmd:emacs\nso:libSDL3.so.0\n");

        let p = load(&r, &at).unwrap();
        assert_eq!(deps(&p), vec!["emacs", "so:libSDL3.so.0"]);
    }

    // - is no equivalent here, and a name that comes back to the recipe itself is its own
    // subpackage. neither is an edge, and two spellings of one package are one
    #[test]
    fn nothing_itself_and_twice_are_not_edges() {
        let r = root("drops");
        installed(&r, "mesa", &[], &["libGL.so.1", "libEGL.so.1"]);
        fs::create_dir_all(r.join(REPOS_AT).join("local")).unwrap();
        fs::write(r.join(REPOS_AT).join("local/aliases"), "gtk-doc\t-\nlibfoo-tools\tfoo\n").unwrap();
        let at = recipe(&r, "extra", "foo", "gtk-doc make\nlibfoo-tools\nso:libGL.so.1\nso:libEGL.so.1\n");

        let p = load(&r, &at).unwrap();
        assert_eq!(deps(&p), vec!["mesa"]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod gentoo_flags {
    use super::*;

    fn entry(iuse: &str, rdepend: &str) -> Gentoo {
        let mut g = Gentoo {
            iuse: iuse
                .split_whitespace()
                .map(|f| match f.strip_prefix('+') {
                    Some(n) => (n.to_string(), true),
                    None => (f.to_string(), false),
                })
                .collect(),
            conds: Vec::new(),
            plain: Vec::new(),
        };
        spec(rdepend, Needs::Run, &mut g);
        g
    }

    fn set(fs: &[&str]) -> Vec<String> {
        fs.iter().map(|f| (*f).to_string()).collect()
    }

    #[test]
    fn a_pf_splits_at_the_version_and_keeps_hyphens_in_the_name() {
        assert_eq!(pf("mesa-26.2.3-r1"), Some(("mesa", "26.2.3")));
        assert_eq!(pf("font-noto-cjk-20230817"), Some(("font-noto-cjk", "20230817")));
        assert_eq!(pf("gtk+-3.24.43"), Some(("gtk+", "3.24.43")));
        assert_eq!(pf("foo-rust-1.0_rc2-r3"), Some(("foo-rust", "1.0_rc2")));
        assert_eq!(pf("nothing"), None);
    }

    #[test]
    fn an_atom_is_its_category_and_name_whatever_hangs_off_it() {
        assert_eq!(atom(">=x11-libs/libdrm-2.4.133[abi_x86_32(-)?]").as_deref(), Some("x11-libs/libdrm"));
        assert_eq!(atom("dev-qt/qtbase:6[gui,widgets]").as_deref(), Some("dev-qt/qtbase"));
        assert_eq!(atom("=dev-libs/foo-1.2*").as_deref(), Some("dev-libs/foo"));
        assert_eq!(atom("~media-libs/mesa-26.2.3:=").as_deref(), Some("media-libs/mesa"));
        assert_eq!(atom("media-libs/libsdl2").as_deref(), Some("media-libs/libsdl2"));
        assert_eq!(atom("!dev-libs/bar"), None);
        assert_eq!(atom("!!<dev-libs/bar-2"), None);
    }

    // off takes a dependency out only where nothing else still asks for it
    #[test]
    fn off_takes_out_only_what_every_mention_puts_under_that_flag() {
        let g = entry(
            "+wayland X doc",
            "dev-libs/wayland X? ( x11-libs/libX11 ) wayland? ( dev-libs/wayland ) doc? ( app-text/doxygen ) \
             || ( X? ( x11-libs/libXext ) media-libs/mesa )",
        );
        let off = set(&["-X", "-wayland"]);
        let s = chosen(&off);
        assert!(g.unwanted(&s, "libx11"));
        assert!(g.unwanted(&s, "libxext"));
        // named with no condition too, so the flag does not own it
        assert!(!g.unwanted(&s, "wayland"));
        // doc was not chosen either way, and alpine's choice stands
        assert!(!g.unwanted(&s, "doxygen"));
        // gentoo never mentions it, so no flag can take it
        assert!(!g.unwanted(&s, "zlib"));
    }

    #[test]
    fn a_negated_group_is_taken_out_by_the_flag_being_on() {
        let g = entry("system-lua", "!system-lua? ( dev-lang/lua )");
        assert!(g.unwanted(&chosen(&set(&["system-lua"])), "lua5.4"));
        assert!(!g.unwanted(&chosen(&set(&["-system-lua"])), "lua5.4"));
    }

    // on brings in what a chosen flag names, and a default fills in the rest of a nested
    // condition. a default alone brings nothing: that is alpine's closure already
    #[test]
    fn on_brings_in_what_a_chosen_flag_names_and_defaults_only_finish_the_sentence() {
        let g = entry(
            "vulkan +wayland X",
            "wayland? ( dev-libs/wayland-extra ) \
             vulkan? ( media-libs/vulkan-loader wayland? ( dev-libs/wayland ) X? ( x11-libs/libX11 ) )",
        );
        let names = |fs: &[&str]| -> Vec<String> {
            g.wanted(&chosen(&set(fs))).into_iter().map(|(a, _)| a).collect()
        };
        assert_eq!(names(&["vulkan"]), vec!["media-libs/vulkan-loader", "dev-libs/wayland"]);
        assert_eq!(names(&["vulkan", "X"]).len(), 3);
        assert!(names(&[]).is_empty());
        assert!(names(&["-vulkan"]).is_empty());
    }

    #[test]
    fn gentoo_and_alpine_names_meet_past_case_and_prefixes() {
        assert!(same("x11-libs/libX11", "libx11"));
        assert!(same("media-libs/libsdl2", "sdl2"));
        assert!(same("dev-qt/qtbase", "qt6-qtbase"));
        assert!(same("dev-lang/lua", "lua5.4"));
        assert!(same("x11-libs/gtk+", "gtk+3.0"));
        assert!(same("dev-python/pillow", "py3-pillow"));
        assert!(!same("dev-libs/libfoo", "bar"));
        assert!(!same("dev-libs/wayland", "wayland-protocols"));
    }

    // what an option looks like in the wild: split over lines, a quote in a description,
    // a comment with a quote in it, a ''' description
    #[test]
    fn meson_options_read_through_what_real_files_do() {
        let text = "\
# don't be fooled by option('fake', type : 'boolean')
option('glvnd', type : 'feature', value : 'auto',
  description : 'use glvnd, it\\'s optional')
option(
  'glx',
  type : 'combo',
  choices : ['auto', 'disabled', 'dri', 'xlib',],
  value : 'auto',
  description : '''a ''quoted'' thing''',
)
option('x11', type : 'boolean', value : true)
option('platforms', type : 'array', choices : ['x11', 'wayland'])
";
        let got = options(text);
        let names: Vec<&str> = got.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["glvnd", "glx", "x11", "platforms"]);
        assert_eq!(got[1].1, "combo");
        assert_eq!(got[1].2, vec!["auto", "disabled", "dri", "xlib"]);

        let d = std::env::temp_dir().join(format!("kiry-meson-{}", std::process::id()));
        let top = d.join("mesa-26.2.3");
        fs::create_dir_all(&top).unwrap();
        fs::write(top.join("meson.options"), text).unwrap();
        let args = meson_args(&d, &set(&["-glvnd", "-glx", "x11", "platforms", "unrelated"]));
        assert_eq!(args, "-Dglvnd=disabled -Dglx=disabled -Dx11=true");
        // on for a combo with no word for on is not a guess worth making
        assert_eq!(meson_args(&d, &set(&["glx"])), "");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod bump_policy {
    use super::*;

    fn at(repo: &str, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-policy-{}", std::process::id()))
            .join(repo)
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        for (f, body) in files {
            fs::write(d.join(f), body).unwrap();
        }
        d
    }

    #[test]
    fn a_bump_is_what_the_scheme_calls_the_first_part_that_moved() {
        let plain = at("extra", "plain", &[]);
        assert_eq!(level(&plain, "1.2.3", "1.2.4"), Some("patch"));
        assert_eq!(level(&plain, "1.2.3", "1.3.0"), Some("minor"));
        assert_eq!(level(&plain, "1.2.3", "2.0"), Some("major"));
        // a trailing zero is the same version and a new trailing part is the finest one
        assert_eq!(level(&plain, "1.2", "1.2.1"), Some("patch"));
        // 1.2 and 1.2.0 are one version, so 1.2.0.1 moved the fourth part and not the third
        let four = at("extra", "four", &[("scheme", "major minor minor patch\n")]);
        assert_eq!(level(&four, "1.2", "1.2.0.1"), Some("patch"));
        // mesa's first number is a year and its second a release in that year
        let mesa = at("extra", "mesa", &[("scheme", "major minor patch\n")]);
        assert_eq!(level(&mesa, "25.2.3", "26.0.0"), Some("major"));
        let tz = at("extra", "tzdata", &[("scheme", "major patch\n")]);
        assert_eq!(level(&tz, "2026c", "2026d"), Some("patch"));
        assert_eq!(level(&at("extra", "odd", &[("scheme", "epoch\n")]), "1", "2"), None);
    }

    // the rule: a core recipe says what its numbering is, or its bumps wait for a person
    #[test]
    fn core_gets_no_default_scheme() {
        let gcc = at("core", "gcc", &[]);
        assert_eq!(level(&gcc, "15.2.0", "15.2.1"), None);
        assert!(policy_allows(&gcc, None).unwrap_err().contains("scheme"));
        let llvm = at("core", "llvm", &[("scheme", "major minor patch\n")]);
        assert!(policy_allows(&llvm, level(&llvm, "20.1.8", "21.1.0")).is_ok());
    }

    #[test]
    fn a_policy_lets_through_what_it_names_and_holds_the_rest() {
        let a = at("extra", "always", &[]);
        assert!(policy_allows(&a, Some("major")).is_ok());
        let m = at("extra", "minor", &[("policy", "auto=minor\n")]);
        assert!(policy_allows(&m, Some("patch")).is_ok());
        assert!(policy_allows(&m, Some("minor")).is_ok());
        assert!(policy_allows(&m, Some("major")).unwrap_err().contains("auto=minor holds a major bump"));
        let p = at("extra", "patch", &[("policy", "auto=patch\n")]);
        assert!(policy_allows(&p, Some("minor")).is_err());
        let n = at("extra", "none", &[("policy", "auto=none\n")]);
        assert!(policy_allows(&n, Some("patch")).is_err());
        let bad = at("extra", "bad", &[("policy", "sometimes\n")]);
        assert!(policy_allows(&bad, Some("patch")).unwrap_err().contains("not auto=always"));
    }

    #[test]
    fn a_scheme_lets_letters_order() {
        assert_eq!(compare("2026c", "2026d"), None);
        assert_eq!(compare_as("2026c", "2026d", true), Some(std::cmp::Ordering::Less));
    }

    // steam's tracker answers a debian Packages file, and the version is one field of it
    #[test]
    fn a_tracker_pattern_finds_the_highest_version_it_matches() {
        let body = "Package: steam-launcher\nVersion: 1:1.0.0.83\nArchitecture: all\n\n\
                    Package: steam-libs-amd64\nVersion: 1:1.0.0.85\n\n\
                    Package: steam\nVersion: 1:1.0.0.84\n";
        assert_eq!(version_in(body, "Version: 1:([0-9.]+)").unwrap(), "1.0.0.85");
        assert!(version_in(body, "Release: ([0-9.]+)").is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod layouts {
    use super::*;

    // real clang output, trimmed: one named record of this package's, one of musl's that
    // its headers never mention, and an anonymous one in each tree
    const DUMP: &str = "\
*** Dumping AST Record Layout
         0 | struct bar
         0 |   int a
           | [sizeof=4, dsize=4, align=4,
           |  nvsize=4, nvalign=4]

*** Dumping AST Record Layout
         0 | struct timespec
         0 |   time_t tv_sec
           | [sizeof=16, dsize=16, align=8,
           |  nvsize=16, nvalign=8]

*** Dumping AST Record Layout
         0 | struct (unnamed at /dest/usr/include/bar.h:7:9)
         0 |   char b
           | [sizeof=1, dsize=1, align=1,
           |  nvsize=1, nvalign=1]

*** Dumping AST Record Layout
         0 | struct (unnamed at /usr/include/bits/alltypes.h:41:9)
         0 |   long long __ll
           | [sizeof=8, dsize=8, align=8,
           |  nvsize=8, nvalign=8]
";

    fn ids(words: &[&str]) -> HashSet<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn only_what_this_package_declares_is_fingerprinted() {
        let got = records(DUMP, &ids(&["bar", "a"]));
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["struct bar", "/usr/include/bar.h#1"]);
    }

    // a comment above an anonymous record moves its line, and nothing about its layout
    #[test]
    fn a_line_moving_is_not_a_layout_moving() {
        let moved = DUMP.replace("bar.h:7:9", "bar.h:12:9");
        assert_eq!(records(DUMP, &ids(&["bar"])), records(&moved, &ids(&["bar"])));
        let grown = DUMP.replace("sizeof=4, dsize=4", "sizeof=8, dsize=8");
        assert_ne!(records(DUMP, &ids(&["bar"])), records(&grown, &ids(&["bar"])));
    }

    // what the same batch built against the new header already has it
    #[test]
    fn a_dependent_built_in_the_same_batch_is_not_queued() {
        let root = std::env::temp_dir().join(format!("kiry-layoutq-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let t = "x86_64-musl";
        for (n, deps) in [("libbar", vec![]), ("old", vec!["libbar"]), ("fresh", vec!["libbar"])] {
            db::write(
                &root,
                &db::Installed {
                    name: n.into(),
                    target: t.into(),
                    version: pkg::Version::parse("1.0 1").unwrap(),
                    depends: deps
                        .iter()
                        .map(|d| Dep { name: (*d).into(), make: true, host: false, only: None })
                        .collect(),
                    manifest: Vec::new(),
                    hash: String::new(),
                    users: Vec::new(),
                    flags: Vec::new(),
                },
            )
            .unwrap();
        }
        let moved = vec![(t.to_string(), "libbar".to_string(), vec!["struct bar".to_string()])];
        let just: HashSet<(String, String)> = [(t.to_string(), "fresh".to_string())].into_iter().collect();
        queue_layouts(&root, &moved, &just);
        let names: Vec<String> = db::read_queue(&root).unwrap().into_iter().map(|q| q.name).collect();
        assert_eq!(names, vec!["old"]);
    }

    #[test]
    fn a_location_comes_out_and_the_rest_stays() {
        assert_eq!(
            unplaced("struct (unnamed at /dest/x.h:3:1)::(unnamed at /dest/x.h:4:2) y"),
            "struct (unnamed)::(unnamed) y"
        );
    }
}
