// an apkbuild is shell. abuild sources it rather than parsing it, so this does too, and
// with busybox ash because that is the shell abuild runs. case on $CARCH, ${pkgver/_/-}
// and a makedepends assembled out of three other variables all come out right for free;
// none of them survive a regex

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kiry_core::pkg::Dep;

const WANT: &[&str] = &[
    "pkgname",
    "pkgver",
    "pkgrel",
    "source",
    "sha512sums",
    "depends",
    "makedepends",
    // alpine splits build deps three ways for cross compiling, and a package that uses
    // the split forms leaves makedepends empty. libedit's ncurses is in _host
    "makedepends_host",
    "makedepends_build",
    // to tell a private helper from a subpackage function, which is the one kind that
    // must not come across
    "subpackages",
    "builddir",
    // abuild passes it to every patch in default_prepare. readline's upstream patches
    // are -p0 and land on the wrong file at -p1
    "patch_args",
];

// what could not be carried across, reported rather than guessed at
pub struct Report {
    pub name: String,
    pub notes: Vec<String>,
}

// alpine's name for a thing and this system's name for it are not always the same, and
// nothing can derive one from the other. read in repo order like a recipe lookup, so
// local wins, and a name mapped to - is one with no equivalent here at all
pub fn aliases(repos: &[PathBuf]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for r in repos {
        let Ok(text) = fs::read_to_string(r.join("aliases")) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut f = line.split_whitespace();
            if let (Some(from), Some(to)) = (f.next(), f.next()) {
                out.entry(from.to_string())
                    .or_insert_with(|| to.to_string());
            }
        }
    }
    out
}

pub fn recipe(
    apkbuild: &Path,
    out: &Path,
    fetch: bool,
    alias: &HashMap<String, String>,
    repos: &[PathBuf],
) -> Result<Report, String> {
    let dir = apkbuild.parent().unwrap_or(Path::new("."));
    let text = fs::read_to_string(apkbuild).map_err(|e| format!("{}: {e}", apkbuild.display()))?;
    let (v, mine) = variables(apkbuild)?;
    let mut notes = Vec::new();

    let name = v.get("pkgname").cloned().unwrap_or_default();
    if name.is_empty() {
        return Err(format!("{}: no pkgname", apkbuild.display()));
    }
    let ver = v.get("pkgver").cloned().unwrap_or_default();
    let rev = v.get("pkgrel").cloned().unwrap_or_else(|| "0".into());

    let sums = sha512sums(v.get("sha512sums").map(String::as_str).unwrap_or(""));
    let mut sources = Vec::new();
    let mut checksums = Vec::new();
    let mut files = Vec::new();
    let d = out.join(&name);
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;

    for entry in v
        .get("source")
        .map(String::as_str)
        .unwrap_or("")
        .split_whitespace()
    {
        // name::url, which kiry spells the same way. the sha512 is keyed by the name,
        // so reading the url's basename instead loses the checksum as well as the name
        let (file, url) = match entry.split_once("::") {
            Some((n, u)) if !n.contains(['/', ':']) => (n, u),
            _ => (entry.rsplit('/').next().unwrap_or(entry), entry),
        };

        files.push(file.to_string());
        let local = dir.join(url);
        let at = if url.contains("://") {
            if !fetch {
                notes.push(format!("{file} not fetched, no checksum"));
                sources.push(entry.to_string());
                continue;
            }
            let dst = d.join(file);
            grab(url, &dst)?;
            sources.push(entry.to_string());
            dst
        } else if local.is_file() {
            fs::copy(&local, d.join(file)).map_err(|e| format!("{file}: {e}"))?;
            sources.push(file.to_string());
            d.join(file)
        } else {
            notes.push(format!("{file} is named by source and is not here"));
            sources.push(file.to_string());
            continue;
        };

        // alpine's hash checks the bytes, kiry's records them. running both means the
        // recipe's sha256 is taken from what alpine signed off on rather than from
        // whatever a mirror happened to serve
        match sums.get(file) {
            Some(want) => {
                let got = sha512(&at)?;
                if &got != want {
                    return Err(format!("{file}: sha512 is {got}, the apkbuild says {want}"));
                }
            }
            None => notes.push(format!("{file} has no sha512 in the apkbuild")),
        }
        checksums.push(
            kiry_core::sha256(fs::File::open(&at).map_err(|e| format!("{file}: {e}"))?)
                .map_err(|e| format!("{file}: {e}"))?,
        );
        if url.contains("://") {
            let _ = fs::remove_file(&at);
        }
    }

    let mut depends = Vec::new();
    for (key, make) in [
        ("depends", false),
        ("makedepends", true),
        ("makedepends_host", true),
        ("makedepends_build", true),
    ] {
        for raw in v
            .get(key)
            .map(String::as_str)
            .unwrap_or("")
            .split_whitespace()
        {
            let Some(n) = dep(raw) else {
                notes.push(format!("dropped {key} entry {raw}"));
                continue;
            };
            match alias.get(&n).map(String::as_str) {
                Some("-") => notes.push(format!("{raw} has no equivalent here")),
                Some(to) => depends.push(Dep {
                    name: to.to_string(),
                    make,
                }),
                None => depends.push(Dep { name: n, make }),
            }
        }
    }
    depends.sort_by(|a, b| (a.make, &a.name).cmp(&(b.make, &b.name)));
    depends.dedup_by(|a, b| a.name == b.name && a.make == b.make);

    for x in &depends {
        if !repos
            .iter()
            .any(|r| r.join(&x.name).join("build").is_file())
            && x.name != name
        {
            notes.push(format!("{} is a dependency no repo carries", x.name));
        }
    }

    let builddir = v
        .get("builddir")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("/src/{name}-{ver}"));
    // a body says $pkgver as readily as it says $srcdir, and an unset one expands to
    // nothing rather than failing, so libbz2.so.$pkgver installs as libbz2.so.
    let mut script = format!(
        ". /usr/share/kiry/lib.sh\nsrcdir=/src\npkgname=\"{name}\"\npkgver=\"{ver}\"\npkgrel=\"{rev}\"\n"
    );
    for n in &mine {
        if let Some(val) = v.get(n).filter(|s| !s.is_empty()) {
            script.push_str(&format!("{n}=\"{val}\"\n"));
        }
    }
    // the raw entries, not the names they land under. abuild's $source holds what the
    // apkbuild wrote, and 64 recipes reach for ${p##*/} to get a name back out of it --
    // bash matches */bash[0-9][0-9]-[0-9]* to find its vendor patches, and a bare name
    // never matches, so nine security patches went unapplied and nothing said so
    script.push_str(&format!(
        "source=\"{}\"\nbuilddir=\"{builddir}\"\n",
        sources.join(" ")
    ));
    if let Some(a) = v.get("patch_args").filter(|a| !a.is_empty()) {
        script.push_str(&format!("patch_args=\"{a}\"\n"));
    }

    // definitions rather than inlined bodies, which is what abuild runs too. 852 scripts
    // declare a local in a phase and ash refuses one outside a function. it also puts a
    // helper like _configure in scope wherever in the file it happens to be written
    let subs = subpackage_funcs(
        &name,
        v.get("subpackages").map(String::as_str).unwrap_or(""),
    );
    let mut wrote: Vec<String> = Vec::new();
    for f in functions(&text) {
        if subs.contains(&f) || SKIP.contains(&f.as_str()) || wrote.contains(&f) {
            continue;
        }
        let Some(b) = body(&text, &f) else { continue };
        wrote.push(f.clone());
        script.push_str(&format!(
            "\n{f}() {{\n{}}}\n",
            b.replace("$pkgdir", "$DESTDIR")
                .replace("${pkgdir}", "$DESTDIR")
        ));
    }
    // abuild runs default_prepare when an apkbuild defines no prepare() of its own, and
    // that is the only thing applying the patches for 1505 of them. leaving it out built
    // them unpatched and said nothing
    if !wrote.iter().any(|w| w == "prepare") {
        script.push_str("\nprepare() {\n\tdefault_prepare\n}\n");
        wrote.push("prepare".to_string());
    }
    script.push_str("\ncd \"$builddir\"\n");

    let mut had = false;
    for f in PHASES {
        if wrote.iter().any(|w| w == f) {
            had = *f != "prepare" || had;
            script.push_str(&format!("{f}\n"));
        } else {
            notes.push(format!("no {f}() in the apkbuild"));
        }
    }
    if !had {
        return Err(format!("{name}: no build() or package() to convert"));
    }

    put(&d.join("version"), &format!("{ver} {rev}\n"))?;
    put(&d.join("targets"), "x86_64-musl\n")?;
    put(&d.join("sources"), &joined(&sources))?;
    put(&d.join("checksums"), &joined(&checksums))?;
    put(
        &d.join("depends"),
        &joined(
            &depends
                .iter()
                .map(|x| {
                    if x.make {
                        format!("{} make", x.name)
                    } else {
                        x.name.clone()
                    }
                })
                .collect::<Vec<_>>(),
        ),
    )?;
    put(&d.join("build"), &script)?;

    Ok(Report { name, notes })
}

fn joined(v: &[String]) -> String {
    if v.is_empty() {
        String::new()
    } else {
        format!("{}\n", v.join("\n"))
    }
}

fn put(p: &Path, body: &str) -> Result<(), String> {
    fs::write(p, body).map_err(|e| format!("{}: {e}", p.display()))
}

// values hold newlines, so they come back nul separated rather than a line each
fn variables(apkbuild: &Path) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let dir = apkbuild.parent().unwrap_or(Path::new("."));
    let file = apkbuild
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("apkbuild has no file name")?;

    let mut prog = format!(". ./{file}\n");
    for v in WANT {
        prog.push_str(&format!("printf '%s\\0' \"${v}\"\n"));
    }
    // a case branch can set a private variable inline, so asking the shell what it ended
    // up holding beats looking for the assignment in the text
    prog.push_str(
        "for _n in $(set | busybox sed -n 's/^\\(_[A-Za-z0-9_]*\\)=.*/\\1/p' | busybox sort -u); do\n\
         eval \"printf '%s\\0%s\\0' $_n \\\"\\$$_n\\\"\"\n\
         done\n",
    );

    let out = Command::new("busybox")
        .arg("ash")
        .arg("-c")
        .arg(&prog)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("CARCH", "x86_64")
        .env("srcdir", "/src")
        .env("CHOST", "x86_64-alpine-linux-musl")
        .env("CBUILD", "x86_64-alpine-linux-musl")
        // binutils and gcc read CTARGET to decide they are cross compilers, and an unset
        // one is not equal to CHOST, so pkgname comes out binutils-$CTARGET_ARCH
        .env("CTARGET", "x86_64-alpine-linux-musl")
        .env("CTARGET_ARCH", "x86_64")
        .output()
        .map_err(|e| format!("busybox ash: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{}: {}",
            apkbuild.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let mut got = HashMap::new();
    let mut mine = Vec::new();
    let parts: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect();
    for (i, key) in WANT.iter().enumerate() {
        if let Some(v) = parts.get(i) {
            got.insert((*key).to_string(), v.trim().to_string());
        }
    }
    let mut i = WANT.len();
    while let (Some(name), Some(value)) = (parts.get(i), parts.get(i + 1)) {
        let name = name.trim();
        if name.starts_with('_') && !got.contains_key(name) && name != "_n" {
            got.insert(name.to_string(), value.trim().to_string());
            mine.push(name.to_string());
        }
        i += 2;
    }
    Ok((got, mine))
}

// alpine writes one function per line-anchored brace, so the closing } is in column
// zero and nothing nested can be mistaken for it
// the abuild phases kiry runs, in order
const PHASES: &[&str] = &["prepare", "build", "package"];
// alpine runs these and kiry does not: it builds, it does not test what it built
const SKIP: &[&str] = &["check", "sanitycheck"];

// every top level definition, in the order they are written
fn functions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find("()") else {
            continue;
        };
        if !line[open + 2..].trim_start().starts_with('{') {
            continue;
        }
        let name = &line[..open];
        if !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
        {
            out.push(name.to_string());
        }
    }
    out
}

// abuild runs dev() for the subpackage foo-dev, or the name after a colon when one is
// given. those move files into a package kiry does not build, so they stay behind
fn subpackage_funcs(pkg: &str, subpackages: &str) -> Vec<String> {
    let mut out = Vec::new();
    for entry in subpackages.split_whitespace() {
        let mut parts = entry.split(':');
        let name = parts.next().unwrap_or("");
        match parts.next().filter(|s| !s.is_empty()) {
            Some(f) => out.push(f.to_string()),
            None => {
                let short = name.strip_prefix(pkg).unwrap_or(name);
                let short = short.strip_prefix('-').unwrap_or(short);
                out.push(short.replace(['-', '+', '.'], "_"));
            }
        }
    }
    out
}

fn body(text: &str, name: &str) -> Option<String> {
    let open = format!("{name}() {{");
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if !inside {
            inside = line.trim_end() == open;
            continue;
        }
        if line == "}" {
            return Some(out);
        }
        out.push_str(line);
        out.push('\n');
    }
    None
}

// alpine deps carry three things kiry's do not: a version constraint, a ! meaning a
// conflict, and a -dev suffix for a split kiry does not do. so: pc: and cmd: name a
// file or a command rather than a package and have no equivalent at all
fn dep(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('!') || raw.contains(':') {
        return None;
    }
    let cut = raw.find(['<', '>', '=', '~']).unwrap_or(raw.len());
    let mut n = &raw[..cut];
    for suffix in ["-dev", "-static", "-libs"] {
        if let Some(s) = n.strip_suffix(suffix) {
            n = s;
            break;
        }
    }
    (!n.is_empty()).then(|| n.to_string())
}

fn sha512sums(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        if let (Some(h), Some(name)) = (f.next(), f.next()) {
            out.insert(name.to_string(), h.to_string());
        }
    }
    out
}

fn sha512(p: &Path) -> Result<String, String> {
    let out = Command::new("busybox")
        .arg("sha512sum")
        .arg(p)
        .output()
        .map_err(|e| format!("sha512sum: {e}"))?;
    if !out.status.success() {
        return Err(format!("sha512sum {}", p.display()));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("sha512sum said nothing about {}", p.display()))
}

fn grab(url: &str, dst: &Path) -> Result<(), String> {
    let spec = std::env::var("KIRY_FETCH").unwrap_or_else(|_| "curl -sfL -o %o %u".into());
    let mut it = spec.split_whitespace();
    let prog = it.next().ok_or("KIRY_FETCH is empty")?;
    let mut c = Command::new(prog);
    for a in it {
        c.arg(a.replace("%u", url).replace("%o", &dst.to_string_lossy()));
    }
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{url}: fetch {s}")),
        Err(e) => Err(format!("{url}: {e}")),
    }
}
