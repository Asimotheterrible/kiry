// an apkbuild is shell. abuild sources it rather than parsing it, so this does too, and
// with busybox ash because that is the shell abuild runs. case on $CARCH, ${pkgver/_/-}
// and a makedepends assembled out of three other variables all come out right for free;
// none of them survive a regex

use std::collections::{HashMap, HashSet};
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
    // options_has reads it, and abuild keeps .la files only when it names libtool
    "options",
    // the other names a recipe answers to, which is what parents() reads
    "provides",
    // what a -dev package hands whatever builds against it, read off the dep's apkbuild
    "depends_dev",
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
    let mut carried: Option<HashSet<String>> = None;
    for r in repos {
        let Ok(text) = fs::read_to_string(r.join("aliases")) else {
            continue;
        };
        // a generated pair folds a subpackage into whichever aport builds it, which is
        // alpine's split and not always the split here. where a repo carries a recipe
        // under that very name the pair would alias a real package away: extra/libpulse
        // is the client library on a box whose sound server is pipewire, and the pair
        // reads it as pulseaudio, the daemon nothing here runs
        let own = text.lines().next() == Some(GENERATED);
        if own && carried.is_none() {
            carried = Some(carries(repos));
        }
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut f = line.split_whitespace();
            if let (Some(from), Some(to)) = (f.next(), f.next()) {
                if own && carried.as_ref().is_some_and(|c| c.contains(from)) {
                    continue;
                }
                out.entry(from.to_string())
                    .or_insert_with(|| to.to_string());
            }
        }
    }
    out
}

fn carries(repos: &[PathBuf]) -> HashSet<String> {
    let mut out = HashSet::new();
    for r in repos {
        let Ok(rd) = fs::read_dir(r) else { continue };
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|k| k.is_dir()) {
                out.insert(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}

// the first line of an aliases file convert wrote, which is how it knows the file is its
// own to rewrite and not somebody's hand-kept table
pub const GENERATED: &str =
    "# written by kiry convert from each apkbuild's subpackages and provides. hand-kept pairs go in another repo's aliases, which win";

pub fn generated(file: &Path) -> bool {
    fs::read_to_string(file).is_ok_and(|t| t.lines().next() == Some(GENERATED))
}

// a subpackage is folded into the recipe that builds it, so a dependency on libpq is a
// dependency on postgresql. read out of the apkbuilds rather than typed in: 1788 of 9535
// imported recipes named one, and nobody is writing that table by hand
//
// a name two apkbuilds both claim is left out rather than handed to whichever came first
// -- lua is provided by five lua versions and luajit, and picking one is a decision
pub fn parents(apkbuilds: &[&Path]) -> HashMap<String, String> {
    let mut claims: HashMap<String, Vec<String>> = HashMap::new();
    let mut names = std::collections::HashSet::new();
    for a in apkbuilds {
        let Ok((v, _)) = variables(a) else { continue };
        let Some(me) = v.get("pkgname").filter(|n| !n.is_empty()) else {
            continue;
        };
        names.insert(me.clone());
        let listed = [v.get("subpackages"), v.get("provides")];
        for entry in listed.into_iter().flatten().flat_map(|s| s.split_whitespace()) {
            // name:function:arch in subpackages. the name is the part before the first colon
            let head = entry.split(':').next().unwrap_or("");
            let Some(n) = dep(head) else { continue };
            if n != *me {
                let who = claims.entry(n).or_default();
                if !who.contains(me) {
                    who.push(me.clone());
                }
            }
        }
    }
    claims
        .into_iter()
        .filter(|(n, who)| who.len() == 1 && !names.contains(n))
        .map(|(n, mut who)| (n, who.remove(0)))
        .collect()
}

// hand-kept pairs first, then this batch's parents, then whatever an earlier batch
// wrote. a parent that is itself aliased is chased one step, so clang lands on this
// tree's llvm and not on alpine's clang23
pub fn merged(
    hand: &HashMap<String, String>,
    fresh: &HashMap<String, String>,
    before: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = before.clone();
    for (n, p) in fresh {
        out.insert(n.clone(), p.clone());
    }
    for to in out.values_mut() {
        if let Some(t) = hand.get(to.as_str()) {
            to.clone_from(t);
        }
    }
    out.retain(|n, _| !hand.contains_key(n));
    out
}

pub fn write_parents(file: &Path, m: &HashMap<String, String>) -> Result<(), String> {
    let mut rows: Vec<(&String, &String)> = m.iter().collect();
    rows.sort();
    let mut body = format!("{GENERATED}\n\n");
    for (n, p) in rows {
        body.push_str(&format!("{n}\t{p}\n"));
    }
    put(file, &body)
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
    let mut absent: Vec<String> = Vec::new();
    let mut checksums = Vec::new();
    let mut unchecked = 0;
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
                sources.push(entry.to_string());
                // alpine's word for what the bytes are, checked by the first fetch
                match sums.get(file) {
                    Some(want) => checksums.push(want.clone()),
                    None => {
                        notes.push(format!("{file} not fetched and alpine names no sha512"));
                        unchecked += 1;
                    }
                }
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
            // sources and checksums pair by position, so an entry recorded without a
            // checksum writes a recipe that cannot be read back. left out instead, and
            // counted rather than named one line at a time -- an apkbuild carrying
            // forty patches otherwise buries every other note under them
            absent.push(file.to_string());
            files.pop();
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
    let mut dev = Vec::new();
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
            if make {
                dev.extend(depends_dev(apkbuild, raw, alias));
            }
            match alias.get(&n).map(String::as_str) {
                Some("-") => notes.push(format!("{raw} has no equivalent here")),
                // its own subpackage, which the one recipe already builds
                Some(to) if to == name => {}
                // an apkbuild makedepends is a tool alpine runs during the build, so
                // it converts to " make". a header-only one wants " build" instead and
                // the apkbuild does not say which it is -- corrected by hand when a
                // gnu target of the recipe cannot find its headers
                Some(to) => depends.push(Dep {
                    name: to.to_string(),
                    make,
                    host: make,
                    only: None,
                }),
                None => depends.push(Dep {
                    name: n,
                    make,
                    host: make,
                    only: None,
                }),
            }
        }
    }
    // compiled against, so from the target, and only what the recipe does not name already
    for raw in dev {
        let Some(n) = dep(&raw) else { continue };
        let n = alias.get(&n).cloned().unwrap_or(n);
        if n != "-" && n != name && !depends.iter().any(|d: &Dep| d.name == n) {
            depends.push(Dep { name: n, make: true, host: false, only: None });
        }
    }
    depends.sort_by(|a, b| (a.make, a.host, &a.name).cmp(&(b.make, b.host, &b.name)));
    depends.dedup_by(|a, b| a.name == b.name && a.make == b.make && a.host == b.host);

    for x in depends.iter().filter(|x| !x.name.contains(':')) {
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
    // nothing rather than failing, so libbz2.so.$pkgver installs as libbz2.so
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
    for k in ["patch_args", "options"] {
        if let Some(a) = v.get(k).filter(|a| !a.is_empty()) {
            script.push_str(&format!("{k}=\"{a}\"\n"));
        }
    }
    script.push_str(&untested(&exports(&text)));

    // definitions rather than inlined bodies, which is what abuild runs too. 852 scripts
    // declare a local in a phase and ash refuses one outside a function. it also puts a
    // helper like _configure in scope wherever in the file it happens to be written
    let subs = subpackage_funcs(
        &name,
        v.get("subpackages").map(String::as_str).unwrap_or(""),
    );
    let mut wrote: Vec<String> = Vec::new();
    let unshipped = variants(&text, &subs);
    for f in functions(&text) {
        if subs.contains(&f) || SKIP.contains(&f.as_str()) || wrote.contains(&f) {
            continue;
        }
        let Some(mut b) = body(&text, &f) else { continue };
        if f == "build" && !unshipped.is_empty() {
            b = without(&b, &unshipped);
            notes.push(format!("build() leaves out the variant only a split installs: {}", unshipped.join(" ")));
        }
        wrote.push(f.clone());
        script.push_str(&format!(
            "\n{f}() {{\n{}}}\n",
            untested(
                &b.replace("$pkgdir", "$DESTDIR")
                    .replace("${pkgdir}", "$DESTDIR")
            )
        ));
    }
    // abuild runs default_prepare when an apkbuild defines no prepare() of its own, and
    // that is the only thing applying the patches for 1505 of them. leaving it out built
    // them unpatched and said nothing
    if !wrote.iter().any(|w| w == "prepare") {
        script.push_str("\nprepare() {\n\tdefault_prepare\n}\n");
        wrote.push("prepare".to_string());
    }
    script.push('\n');
    if wrote.iter().any(|w| w == "unpack") {
        script.push_str("cd \"$srcdir\"\nunpack\n");
    }

    let mut had = false;
    for f in PHASES {
        if wrote.iter().any(|w| w == f) {
            had = *f != "prepare" || had;
            script.push_str(&format!("cd \"$builddir\"\n{f}\n"));
        } else {
            notes.push(format!("no {f}() in the apkbuild"));
        }
    }
    if !had {
        return Err(format!("{name}: no build() or package() to convert"));
    }
    // a package() that installs nothing leaves the payload to the splits: lua-mpack builds
    // a module per lua and each lua$v-mpack installs its own. folded into one recipe, those
    // run into $DESTDIR. a split that moves files out of $pkgdir would have nothing to move
    if !installs(&text) {
        for entry in v.get("subpackages").map(String::as_str).unwrap_or("").split_whitespace() {
            let sub = entry.split(':').next().unwrap_or("");
            let Some(f) = subpackage_funcs(&name, entry).pop() else { continue };
            let Some(b) = body(&text, &f) else { continue };
            if !b.contains("subpkgdir") || b.contains("$pkgdir") || b.contains("${pkgdir}") || b.contains("amove") {
                continue;
            }
            if !wrote.contains(&f) {
                wrote.push(f.clone());
                script.push_str(&format!("\n{f}() {{\n{}}}\n", untested(&b)));
            }
            script.push_str(&format!("cd \"$builddir\"\nsubpkgname=\"{sub}\" subpkgdir=\"$DESTDIR\"\n{f}\n"));
        }
    }
    // what abuild strips after package(). the info index is generated from the pages
    // beside it, so 395 recipes would each claim the same file, and a .la names build
    // paths that are gone by the time anything reads them
    script.push_str(
        "\nrm -f \"$DESTDIR\"/usr/share/info/dir\n\
         options_has libtool || find \"$DESTDIR\" -name '*.la' -delete\n",
    );

    put(&d.join("version"), &format!("{ver} {rev}\n"))?;
    put(&d.join("targets"), "x86_64-musl\n")?;
    put(&d.join("sources"), &joined(&sources))?;
    // sources and checksums pair by position and an empty checksums file is the one
    // shape that says nothing rather than saying the wrong thing. a converted recipe
    // that cannot fetch would come out 73 sources to 72 checksums, which does not load
    // at all -- and the 72 are the patches beside the apkbuild, so the one missing is
    // the tarball
    let checksums = if unchecked > 0 { Vec::new() } else { checksums };
    put(&d.join("checksums"), &joined(&checksums))?;
    put(
        &d.join("depends"),
        &joined(&depends.iter().map(|x| x.to_string()).collect::<Vec<_>>()),
    )?;
    put(&d.join("build"), &script)?;

    if !absent.is_empty() {
        notes.push(format!(
            "{} sources the apkbuild names are not in the tree and were left out: {}",
            absent.len(),
            absent.join(" ")
        ));
    }
    Ok(Report { name, notes })
}

// build dirs build() makes that package() never reads and a split does: swayimg's
// output-full, vim's "$builddir"-gvim, pinentry's libsecret copy, rofi's output-wayland.
// one recipe here is what package() installs, so a variant only a split ships is a build
// whose deps discovery proves for nothing -- libheif, libjxl and librsvg for a swayimg
// that links none of them, gtk3 for a vim without a gui. fcitx5-gtk and the two-qt
// libraries install every dir in package() and keep them all
fn variants(text: &str, subs: &[String]) -> Vec<String> {
    let flat = |s: &str| s.replace(['"', '\''], "").replace("${builddir}", "$builddir");
    // lua-mpack's package() installs nothing and its splits are the payload
    if !installs(text) {
        return Vec::new();
    }
    let Some(pkg) = body(text, "package").map(|b| flat(&b)) else { return Vec::new() };
    let Some(build) = body(text, "build").map(|b| flat(&b)) else { return Vec::new() };
    let splits: Vec<String> = subs.iter().filter_map(|f| body(text, f)).map(|b| flat(&b)).collect();
    let fns = functions(text);
    let words: Vec<&str> = pkg.split(|c: char| c.is_whitespace() || ";|&()".contains(c)).collect();
    let mut out: Vec<String> = Vec::new();
    for w in build.split(|c: char| c.is_whitespace() || ";|&()".contains(c)) {
        let named = w.contains("build") || w.contains("output") || w.starts_with("$builddir");
        // a command or a helper is not a dir: abuild-meson, _build
        if w.starts_with("abuild") || fns.iter().any(|f| f == w) {
            continue;
        }
        if !named || w.starts_with('-') || w.contains('=') || ["build", "output", "$builddir"].contains(&w) {
            continue;
        }
        if pkg.contains(w) || !splits.iter().any(|s| s.contains(w)) || out.iter().any(|o| o == w) {
            continue;
        }
        // a sibling of a dir package() reads, by a plain word: output-full beside output,
        // $builddir-gvim beside $builddir, build-qt5 beside build-qt6. a path into the tree
        // or a dir per lua version is something else
        let sibling = w.match_indices('-').any(|(i, _)| {
            let (base, rest) = (&w[..i], &w[i + 1..]);
            let plain = !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            let read = base == "$builddir" || words.iter().any(|p| *p == base || p.starts_with(&format!("{base}-")));
            plain && !base.contains('/') && read
        });
        if sibling {
            out.push(w.to_string());
        }
    }
    out
}

// what lua-mpack's package() is: nothing, or only a mkdir of $pkgdir
fn installs(text: &str) -> bool {
    body(text, "package").is_some_and(|b| {
        !b.lines().all(|l| {
            let l = l.trim();
            l.is_empty() || l.starts_with('#') || (l.starts_with("mkdir -p") && l.contains("pkgdir"))
        })
    })
}

// build() without the statements that make those dirs. a statement is a line with its
// backslash continuations, or a whole if/for/while/case block, since vim builds gvim under
// if $BUILD_GVIM. a cd into one takes what follows with it up to the next cd: pinentry
// builds its libsecret copy by cd and then a bare _build
fn without(b: &str, dirs: &[String]) -> String {
    let flat = |s: &str| s.replace(['"', '\''], "").replace("${builddir}", "$builddir");
    let mut stmts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for l in b.lines() {
        cur.push_str(l);
        cur.push('\n');
        let first = l.split_whitespace().next().unwrap_or("");
        depth += match first {
            "if" | "for" | "while" | "until" | "case" => 1,
            "fi" | "done" | "esac" => -1,
            _ => 0,
        };
        if depth <= 0 && !l.trim_end().ends_with('\\') {
            stmts.push(std::mem::take(&mut cur));
            depth = 0;
        }
    }
    if !cur.is_empty() {
        stmts.push(cur);
    }
    let mut out = String::new();
    let mut away = false;
    for s in stmts {
        let f = flat(&s);
        let said = |w: &str| f.split(|c: char| c.is_whitespace() || ";|&()".contains(c)).any(|x| x == w);
        let hit = dirs.iter().any(|d| f.contains(d.as_str()));
        // nemo's mkdir build-qt5 && cd build-qt5 goes in too, and so does dnsdist's
        // [ -d "$builddir-jit" ] || return 0, which is the gate in front of the variant
        if said("cd") {
            away = hit;
        }
        if hit && said("return") {
            away = true;
        }
        if hit || away {
            continue;
        }
        out.push_str(&s);
    }
    out
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

// kiry never runs check(), and a test suite it builds anyway is one more thing to fail:
// mbedtls2, glm and cxxopts all stopped on clang's -Werror inside their tests. a word at a
// time, so the whitespace and the line continuations stay as they were. the same walk
// takes alpine's lto words and pinned clangs out
pub fn untested(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut word = String::new();
    for c in body.chars() {
        if c.is_whitespace() {
            out.push_str(&off(&unlto(&unpinned(&word))));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&off(&unlto(&unpinned(&word))));
    out
}

// an apkbuild's own environment, written above its functions: rdedup's
// SODIUM_USE_PKG_CONFIG=1, without which libsodium-sys wants a static libsodium. as
// written, since most say $GOFLAGS or $CFLAGS and mean the build's. one inside a case is
// indented and left, being for some other arch
fn exports(text: &str) -> String {
    let mut out = String::new();
    // a value can go on over lines inside quotes or after a backslash
    let (mut quoted, mut more) = (false, false);
    for line in text.lines() {
        if !quoted && !more && !line.starts_with("export ") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        quoted ^= line.matches('"').count() % 2 == 1;
        more = line.ends_with('\\');
    }
    out
}

// php sets CC=clang-$_clang_ver, a clang alpine keeps beside its default one. navi has
// only its own, and the wrapper the sandbox names in CC is what carries the flags
fn unpinned(word: &str) -> String {
    let bare = |p: &str| word.strip_prefix(p).map(|v| v.trim_matches(['"', '\'']));
    if bare("CC=").is_some_and(|v| v.starts_with("clang-")) {
        return "CC=$CC".to_string();
    }
    if bare("CXX=").is_some_and(|v| v.starts_with("clang++-")) {
        return "CXX=$CXX".to_string();
    }
    word.to_string()
}

// alpine's -flto=auto is gcc's full lto spelled for clang, and it rides on top of the LTO
// knob kiry's flags already carry: full lto where the knob says thin, and never through
// the thinlto cache. gone, the knob decides here like it does everywhere else. only the
// whole word, give or take its quotes: perl's `s| -flto=auto||g` has it inside a longer
// one, and taking it out of there turns the sed into one that strips every space.
// -Db_lto=true is muon's plain -flto ahead of the knob's: thin wins by coming last, and
// where filter-lto took the knob's out, full lto is what is left
fn unlto(word: &str) -> String {
    let inner = word.trim_start_matches(['"', '\'']);
    let bare = inner.trim_end_matches(['"', '\'']);
    match bare {
        "-flto=auto" | "-ffat-lto-objects" | "-Db_lto=true" => {
            format!("{}{}", &word[..word.len() - inner.len()], &inner[bare.len()..])
        }
        _ => word.to_string(),
    }
}

// the same word switched off, or as it was. an --enable- with a value is left alone: the
// value is what it asks for, and --disable-testing-mode=no is anybody's guess
fn off(word: &str) -> String {
    let inner = word.trim_start_matches(['"', '\'']);
    let bare = inner.trim_end_matches(['"', '\'']);
    let (lead, tail) = (&word[..word.len() - inner.len()], &inner[bare.len()..]);
    let flipped = if let Some(name) = bare.strip_prefix("--enable-") {
        (!name.contains('=') && (tests(name) || dropped(name)))
            .then(|| format!("--disable-{name}"))
    } else if let Some((name, val)) = bare.strip_prefix("-D").and_then(|r| r.split_once('='))
    {
        let no = match val {
            "ON" => Some("OFF"),
            "on" => Some("off"),
            "On" => Some("Off"),
            "TRUE" => Some("FALSE"),
            "True" => Some("False"),
            "true" => Some("false"),
            "YES" => Some("NO"),
            "Yes" => Some("No"),
            "yes" => Some("no"),
            "1" => Some("0"),
            "enabled" => Some("disabled"),
            _ => None,
        };
        let name_only = name.split(':').next().unwrap_or(name);
        no.filter(|_| tests(name_only) || dropped(name_only)).map(|no| format!("-D{name}={no}"))
    } else {
        None
    };
    match flipped {
        Some(f) => format!("{lead}{f}{tail}"),
        None => word.to_string(),
    }
}

// core/aliases maps gobject-introspection, vala and gtk-doc to nothing, so a switch that
// forces one on can only stop the configure: libgudev asks for g-ir-scanner and vapigen
fn dropped(name: &str) -> bool {
    let up = name.to_ascii_uppercase().replace('-', "_");
    up.contains("INTROSPECTION")
        || up.contains("GTK_DOC")
        || up.split('_').any(|s| matches!(s, "VAPI" | "VALA" | "GIR"))
}

// whether an option names a test suite. a bare singular TEST is too often a library the
// package installs -- SDL_TEST is libSDL2_test, orc-test is liborc-test, kcapi-test is the
// kcapi tool -- so it only counts on its own or right after BUILD, ENABLE, WITH or INCLUDE.
// a gtest in the name is the system gtest to link, and NO_TESTS=ON already says off
pub fn tests(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    let seg: Vec<&str> = up.split(['_', '-']).collect();
    if seg.iter().any(|s| {
        matches!(
            *s,
            "NO" | "DISABLE" | "DISABLED" | "SKIP" | "IGNORE" | "WITHOUT"
        ) || s.contains("GTEST")
    }) {
        return false;
    }
    seg.iter().any(|s| matches!(*s, "TESTS" | "TESTING" | "TESTSUITE"))
        || matches!(
            seg.as_slice(),
            ["TEST"] | [.., "BUILD" | "ENABLE" | "WITH" | "INCLUDE", "TEST"]
        )
}

// alpine deps carry three things kiry's do not: a version constraint, a ! meaning a
// conflict, and a -dev suffix for a split kiry does not do. so: pc: and cmd: name a
// file or a command rather than a package and have no equivalent at all
fn dep(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('!') {
        return None;
    }
    let cut = raw.find(['<', '>', '=', '~']).unwrap_or(raw.len());
    // whatever provides a library, a command or a pkg-config file. which package that is
    // gets decided when the recipe is read, against what is installed then. dropping it
    // takes the libGL librewolf dlopens with it, and nothing else would ever notice
    if let Some((kind, _)) = raw.split_once(':') {
        return matches!(kind, "so" | "cmd" | "pc").then(|| raw[..cut].to_string());
    }
    let mut n = &raw[..cut];
    for suffix in ["-dev", "-static", "-libs"] {
        if let Some(s) = n.strip_suffix(suffix) {
            n = s;
            break;
        }
    }
    (!n.is_empty()).then(|| n.to_string())
}

// a -dev package drags its depends_dev along for whatever builds against it. libraries
// in it come into the sandbox through their .pc files and tools and plugins are not
// headers, which leaves the header-only packages no .pc names: gtk+-3.0.pc never names
// wayland-protocols, which gtk+3.0-dev hands every consumer. taking the whole list was
// 12078 lines across 3003 recipes, openssl and python3 and qt plugins among them.
// linux-headers is in every build already, through the toolchain
fn depends_dev(apkbuild: &Path, raw: &str, alias: &HashMap<String, String>) -> Vec<String> {
    let cut = raw.find(['<', '>', '=', '~']).unwrap_or(raw.len());
    let Some(base) = raw[..cut].strip_suffix("-dev") else {
        return Vec::new();
    };
    // <aports>/<repo>/<pkg>/APKBUILD, and the dep's own is a sibling of it
    let Some(aports) = apkbuild.parent().and_then(Path::parent).and_then(Path::parent) else {
        return Vec::new();
    };
    let origin = [Some(base), alias.get(base).map(String::as_str)];
    for o in origin.into_iter().flatten() {
        for repo in ["main", "community", "testing"] {
            let a = aports.join(repo).join(o).join("APKBUILD");
            if !a.is_file() || a == apkbuild {
                continue;
            }
            let Ok((v, _)) = variables(&a) else { return Vec::new() };
            let list = v.get("depends_dev").map(String::as_str).unwrap_or("");
            return list
                .split_whitespace()
                .filter(|d| {
                    (d.ends_with("-protocols") || d.ends_with("-headers") || d.ends_with("proto"))
                        && *d != "linux-headers"
                })
                .map(str::to_string)
                .collect();
        }
    }
    Vec::new()
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("kiry-cv-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    // alpine ships libpulse out of the pulseaudio aport, so convert derives that pair,
    // but this box runs pipewire and carries a libpulse recipe of its own, and reading the
    // pair would send a dependency to the daemon nothing here runs
    #[test]
    fn a_generated_pair_does_not_shadow_a_recipe_a_repo_carries() {
        let at = scratch("shadow");
        fs::create_dir_all(at.join("libpulse")).unwrap();
        fs::write(
            at.join("aliases"),
            format!("{GENERATED}\nlibpulse\tpulseaudio\nlibpq\tpostgresql\n"),
        )
        .unwrap();

        let got = aliases(std::slice::from_ref(&at));
        assert_eq!(got.get("libpulse"), None, "{got:?}");
        // a pair naming something no repo carries is the whole point of the table
        assert_eq!(got.get("libpq").map(String::as_str), Some("postgresql"));
    }

    // a pair somebody typed is their word on it, carried recipe or not
    #[test]
    fn a_hand_kept_pair_still_wins_over_a_recipe_of_the_same_name() {
        let at = scratch("hand");
        fs::create_dir_all(at.join("pulseaudio")).unwrap();
        fs::write(at.join("aliases"), "pulseaudio\tlibpulse\n").unwrap();

        let got = aliases(std::slice::from_ref(&at));
        assert_eq!(got.get("pulseaudio").map(String::as_str), Some("libpulse"));
    }

    #[test]
    fn an_apkbuilds_own_exports_come_along() {
        let text = "pkgname=x\nexport SODIUM_USE_PKG_CONFIG=1\nexport CFLAGS=\"$CFLAGS\n\t-O2\"\n\
            export GOFLAGS=\"$GOFLAGS -trimpath\" \\\n\tGOAMD64=v3\n\
            case \"$CARCH\" in\nx86) export FOO=1 ;;\nesac\n\tbuild() {\n\texport BAR=1\n}\n";
        assert_eq!(
            exports(text),
            "export SODIUM_USE_PKG_CONFIG=1\nexport CFLAGS=\"$CFLAGS\n\t-O2\"\n\
             export GOFLAGS=\"$GOFLAGS -trimpath\" \\\n\tGOAMD64=v3\n"
        );
    }

    // php83 asked for clang-21 and its configure found no compiler at all
    #[test]
    fn a_pinned_clang_gives_way_to_the_sandboxs_cc() {
        let got = untested("\texport CC=clang-$_clang_ver\n\texport CXX=\"clang++-$_llvmver\"\n");
        assert_eq!(got, "\texport CC=$CC\n\texport CXX=$CXX\n");
        // llvm building itself names its own, and a plain clang is not pinned
        let kept = "\tCC=clang -DCMAKE_C_COMPILER=clang-$_llvmver CLANG=clang-$_llvmver\n";
        assert_eq!(untested(kept), kept);
    }
}
