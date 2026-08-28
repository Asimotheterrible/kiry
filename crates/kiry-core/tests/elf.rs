// the reader against the tool everyone else already trusts, over whatever this
// machine has installed. same reason the tar suite runs three tars: a fixture
// written by the understanding that wrote the parser agrees with it by
// construction and proves nothing

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use kiry_core::elf;

const DIRS: &[&str] = &[
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
    // debian and ubuntu keep them a level down, under the target triplet
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
    "/usr/bin",
    "/usr/libexec",
];

#[derive(Debug, Default, PartialEq, Eq)]
struct Dyn {
    verdef: bool,
    soname: Option<String>,
    needed: Vec<String>,
    rpath: Option<String>,
    runpath: Option<String>,
}

// the filter is on raw bytes, not on whether our own parser liked the file, or
// the corpus would quietly become "everything we already handle"
fn corpus() -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    // /lib64 and /usr/lib64 are the same directory on a usr-merged system, so the
    // same file turns up twice without this
    let mut seen = BTreeSet::new();
    for d in DIRS {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let real = p.canonicalize().unwrap_or_else(|_| p.clone());
            if !seen.insert(real) {
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            let x86_64 = b.starts_with(&elf::MAGIC)
                && b.get(4) == Some(&2)
                && b.get(5) == Some(&1)
                && b.get(18..20) == Some(&[62, 0][..]);
            if x86_64 {
                out.push((p, b));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn bracketed(line: &str) -> Option<String> {
    let open = line.rfind('[')?;
    let close = line.rfind(']')?;
    Some(line.get(open + 1..close)?.to_string())
}

fn readelf(paths: &[&PathBuf]) -> BTreeMap<PathBuf, Dyn> {
    let out = Command::new("readelf")
        .arg("-d")
        .args(paths)
        .output()
        .expect("readelf failed to run");
    let text = String::from_utf8_lossy(&out.stdout);

    let mut map = BTreeMap::new();
    // readelf prints no File: header when it was handed exactly one file
    let mut cur: Option<PathBuf> = match paths {
        [only] => Some((*only).clone()),
        _ => None,
    };
    let mut d = Dyn::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("File: ") {
            if let Some(p) = cur.take() {
                map.insert(p, std::mem::take(&mut d));
            }
            cur = Some(PathBuf::from(rest.trim()));
        } else if line.contains("(NEEDED)") {
            if let Some(v) = bracketed(line) {
                d.needed.push(v);
            }
        } else if line.contains("(SONAME)") {
            d.soname = bracketed(line);
        } else if line.contains("(RUNPATH)") {
            d.runpath = bracketed(line);
        } else if line.contains("(RPATH)") {
            d.rpath = bracketed(line);
        } else if line.contains("(VERDEF)") {
            d.verdef = true;
        }
    }
    if let Some(p) = cur {
        map.insert(p, d);
    }
    map
}

#[test]
fn agrees_with_readelf_on_every_library_here() {
    if Command::new("readelf").arg("-v").output().is_err() {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "readelf is not installed"
        );
        return;
    }

    let corpus = corpus();
    if corpus.len() < 100 {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "only found {} elf files in {DIRS:?}, which is not a corpus. set \
             KIRY_TEST_ALLOW_SKIP=1 if this machine really keeps them somewhere else",
            corpus.len()
        );
        return;
    }

    let paths: Vec<&PathBuf> = corpus.iter().map(|(p, _)| p).collect();
    let theirs = readelf(&paths);
    assert_eq!(theirs.len(), corpus.len(), "readelf skipped some of them");

    let mut sonames = 0;
    let mut rpaths = 0;
    let mut verdefs = 0;
    for (p, bytes) in &corpus {
        let got = elf::parse(bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let want = &theirs[p];

        assert_eq!(got.soname, want.soname, "soname of {}", p.display());
        assert_eq!(got.needed, want.needed, "needed of {}", p.display());
        assert_eq!(got.rpath, want.rpath, "rpath of {}", p.display());
        assert_eq!(got.runpath, want.runpath, "runpath of {}", p.display());
        assert_eq!(got.versioned, want.verdef, "verdef of {}", p.display());

        sonames += usize::from(got.soname.is_some());
        rpaths += usize::from(got.rpath.is_some() || got.runpath.is_some());
        verdefs += usize::from(got.versioned);
    }

    // if the corpus somehow held nothing interesting the comparison above is empty
    assert!(sonames > 10, "only {sonames} sonames in {}", corpus.len());
    assert!(rpaths > 0, "no library carried an rpath or runpath");
    assert!(verdefs > 10, "only {verdefs} libraries defined versions");
}

fn strtab_addr(p: &PathBuf) -> u64 {
    let out = Command::new("readelf").arg("-d").arg(p).output().unwrap();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("(STRTAB)") {
            if let Some(hex) = line.split_whitespace().last() {
                if let Some(h) = hex.strip_prefix("0x") {
                    if let Ok(v) = u64::from_str_radix(h, 16) {
                        return v;
                    }
                }
            }
        }
    }
    panic!("no STRTAB in {}", p.display());
}

// DT_STRTAB is an address, not a file offset, and on a PIE binary the two happen
// to be equal - so a reader that never maps one to the other agrees with readelf
// on every shared library installed here. -no-pie is what pulls them apart
#[test]
fn maps_a_string_table_that_is_not_where_its_address_says() {
    let dir = std::env::temp_dir().join(format!("kiry-elf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.c");
    std::fs::write(&src, "int main(void){return 0;}\n").unwrap();
    let bin = dir.join("t");

    let built = Command::new("cc")
        .arg("-no-pie")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a -no-pie binary"
            );
            return;
        }
    }

    let b = std::fs::read(&bin).unwrap();
    let addr = strtab_addr(&bin);
    assert!(
        addr > b.len() as u64,
        "-no-pie put the string table at {addr:#x} inside a {} byte file, so this \
         test no longer proves the address ever gets mapped",
        b.len()
    );

    let got = elf::parse(&b).unwrap();
    let want = readelf(&[&bin]);
    assert_eq!(
        got.needed, want[&bin].needed,
        "needed of the -no-pie binary"
    );
    assert!(
        !got.needed.is_empty(),
        "a dynamic binary with no DT_NEEDED?"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[derive(Debug, PartialEq, Eq)]
struct RSym {
    name: String,
    version: Option<String>,
    default: bool,
    weak: bool,
    object: bool,
    size: u64,
    undefined: bool,
}

// Num: Value Size Type Bind Vis Ndx Name[@ver|@@ver][ (n)]
fn dyn_syms(paths: &[&PathBuf]) -> BTreeMap<PathBuf, Vec<RSym>> {
    let out = Command::new("readelf")
        .args(["--dyn-syms", "-W"])
        .args(paths)
        .output()
        .expect("readelf failed to run");
    let text = &String::from_utf8_lossy(&out.stdout);

    let mut map: BTreeMap<PathBuf, Vec<RSym>> = BTreeMap::new();
    // no File: header comes back when readelf was handed exactly one file
    let mut cur: Option<PathBuf> = match paths {
        [only] => {
            map.insert((*only).clone(), Vec::new());
            Some((*only).clone())
        }
        _ => None,
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("File: ") {
            cur = Some(PathBuf::from(rest.trim()));
            map.entry(cur.clone().unwrap()).or_default();
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || !f[0].ends_with(':') {
            continue;
        }
        // readelf prints st_size in decimal up to 99999 and hex past it, so parsing
        // only decimal silently drops every symbol over that line
        let size = match f[2].strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16),
            None => f[2].parse(),
        };
        let Ok(size) = size else { continue };
        let (kind, bind, ndx) = (f[3], f[4], f[6]);
        let raw = f[7];
        // a trailing "(3)" means readelf took the version from verneed rather than
        // verdef, and it prints those with one @ regardless of the hidden bit, so
        // the punctuation says nothing about default there
        let from_verneed = f.get(8).is_some_and(|t| t.starts_with('('));
        let (name, version, default) = match raw.split_once("@@") {
            Some((n, v)) => (n.to_string(), Some(v.to_string()), true),
            None => match raw.split_once('@') {
                Some((n, v)) => (n.to_string(), Some(v.to_string()), from_verneed),
                None => (raw.to_string(), None, true),
            },
        };
        if name.is_empty() {
            continue;
        }
        let s = RSym {
            name,
            version,
            default,
            weak: bind == "WEAK",
            object: kind == "OBJECT",
            size,
            undefined: ndx == "UND",
        };
        if let Some(p) = &cur {
            map.entry(p.clone()).or_default().push(s);
        }
    }
    map
}

fn mine(e: &elf::Elf) -> Vec<RSym> {
    let f = |s: &elf::Sym, undefined: bool| RSym {
        name: s.name.clone(),
        // glibc exports every version name as an ABS symbol pointed at its own verdef
        // entry, and get_symbol_version_string guards the print on st_name != vda_name,
        // so readelf never writes GLIBC_2.10@GLIBC_2.10
        version: s.version.clone().filter(|v| *v != s.name),
        default: s.default,
        weak: s.weak,
        object: s.object,
        size: s.size,
        undefined,
    };
    let mut v: Vec<RSym> = e.exports.iter().map(|s| f(s, false)).collect();
    v.extend(e.undefined.iter().map(|s| f(s, true)));
    v
}

// libc alone carries forty-odd version definitions and most of the non-default
// symbols on the machine, so the sample never gets to skip it
const ALWAYS: &[&str] = &["libc.so.6", "libm.so.6"];

// hashing the path keeps a file in or out for good. an index into a directory
// listing would re-roll the whole sample every time a package lands
fn sampled(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if ALWAYS.contains(&name) {
        return true;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h.is_multiple_of(21)
}

#[test]
fn agrees_with_readelf_on_dynamic_symbols() {
    let corpus = corpus();
    if corpus.len() < 100 {
        assert!(std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(), "no corpus");
        return;
    }
    let sample: Vec<&(PathBuf, Vec<u8>)> = corpus.iter().filter(|(p, _)| sampled(p)).collect();
    let paths: Vec<&PathBuf> = sample.iter().map(|(p, _)| p).collect();

    let theirs = dyn_syms(&paths);

    let mut compared = 0;
    let mut versioned = 0;
    let mut nondefault = 0;
    let mut selfnamed = 0;
    for (p, bytes) in &sample {
        let got = elf::parse(bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let want = theirs
            .get(p)
            .unwrap_or_else(|| panic!("readelf skipped {}", p.display()));

        let mut a = mine(&got);
        let mut b: Vec<&RSym> = want.iter().collect();
        a.sort_by(|x, y| (&x.name, &x.version).cmp(&(&y.name, &y.version)));
        b.sort_by(|x, y| (&x.name, &x.version).cmp(&(&y.name, &y.version)));

        assert_eq!(a.len(), b.len(), "symbol count for {}", p.display());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(&x, y, "symbol mismatch in {}", p.display());
        }

        compared += a.len();
        versioned += a.iter().filter(|s| s.version.is_some()).count();
        nondefault += a
            .iter()
            .filter(|s| s.version.is_some() && !s.default)
            .count();
        // mine() drops these to match readelf, so count them before it does or
        // the reader could stop resolving them and nothing here would notice
        selfnamed += got
            .exports
            .iter()
            .filter(|s| s.version.as_deref() == Some(s.name.as_str()))
            .count();
    }

    assert!(compared > 5000, "only compared {compared} symbols");
    assert!(versioned > 100, "only {versioned} versioned symbols");
    assert!(
        nondefault > 0,
        "no non-default versioned symbol in the sample"
    );
    assert!(selfnamed > 0, "no version-definition symbol in the sample");
}

// nothing sampled here has an object big enough to cross readelf's decimal cutoff,
// so a fixture is the only way to reach that branch on purpose
#[test]
fn reads_a_symbol_too_big_for_readelf_to_print_in_decimal() {
    let dir = std::env::temp_dir().join(format!("kiry-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.c");
    std::fs::write(&src, "char big[200000];\n").unwrap();
    let so = dir.join("libt.so");

    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a shared library"
            );
            return;
        }
    }

    let raw = Command::new("readelf")
        .args(["--dyn-syms", "-W"])
        .arg(&so)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&raw.stdout);
    let hex = text.lines().any(|l| {
        l.split_whitespace().last() == Some("big")
            && l.split_whitespace()
                .nth(2)
                .is_some_and(|s| s.starts_with("0x"))
    });
    assert!(
        hex,
        "readelf printed the size in decimal, so the fixture no longer reaches the \
         hex branch and this test proves nothing"
    );

    let got = mine(&elf::parse(&std::fs::read(&so).unwrap()).unwrap());
    let want = &dyn_syms(&[&so])[&so];
    assert_eq!(got.len(), want.len(), "symbol count for the fixture");
    let big = got
        .iter()
        .find(|s| s.name == "big")
        .unwrap_or_else(|| panic!("lost the symbol readelf prints in hex"));
    assert_eq!(big.size, 200_000);

    let _ = std::fs::remove_dir_all(&dir);
}

// nothing installed here carries DT_HASH any more, the linker has defaulted to the
// gnu table for years. --hash-style=sysv is the only way to reach that path
#[test]
fn counts_symbols_out_of_the_old_hash_table_too() {
    let dir = std::env::temp_dir().join(format!("kiry-sysv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("s.c");
    std::fs::write(
        &src,
        "int alpha(int x){return x+1;}\nint beta(int x){return x+2;}\ndouble gamma_data = 3.0;\n",
    )
    .unwrap();
    let lib = dir.join("libsysv.so");

    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,--hash-style=sysv", "-o"])
        .arg(&lib)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a sysv-hash library"
            );
            return;
        }
    }

    let b = std::fs::read(&lib).unwrap();
    // prove the fixture reaches the branch it exists for
    let tags = Command::new("readelf")
        .arg("-d")
        .arg(&lib)
        .output()
        .unwrap();
    let tags = String::from_utf8_lossy(&tags.stdout);
    assert!(tags.contains("(HASH)"), "no DT_HASH, fixture is pointless");
    assert!(
        !tags.contains("GNU_HASH"),
        "GNU_HASH present too, so the gnu branch wins and this proves nothing"
    );

    let got = elf::parse(&b).unwrap();
    let want = dyn_syms(&[&lib]);
    let want = &want[&lib];

    assert_eq!(got.exports.len() + got.undefined.len(), want.len());
    for n in ["alpha", "beta", "gamma_data"] {
        assert!(
            got.exports.iter().any(|s| s.name == n),
            "{n} missing from {:?}",
            got.exports.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
    assert!(got
        .exports
        .iter()
        .any(|s| s.name == "gamma_data" && s.object && s.size == 8));

    let _ = std::fs::remove_dir_all(&dir);
}

// the comparison has no oracle: no tool on the system will say whether two builds of one
// library broke their consumers. so the compiler makes each change real and the test
// asserts the verdict
fn pair(
    name: &str,
    before: &str,
    after: &str,
    script: Option<&str>,
) -> Option<(elf::Elf, elf::Elf)> {
    let dir = std::env::temp_dir().join(format!("kiry-abi-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut built = Vec::new();
    for (which, body) in [("old", before), ("new", after)] {
        let src = dir.join(format!("{which}.c"));
        std::fs::write(&src, body).unwrap();
        let so = dir.join(format!("lib{which}.so"));
        let mut c = Command::new("cc");
        c.args(["-shared", "-fPIC", "-nostdlib"]);
        if let Some(v) = script {
            let map = dir.join("v.map");
            std::fs::write(&map, v).unwrap();
            c.arg(format!("-Wl,--version-script,{}", map.display()));
        }
        let ok = c.arg("-o").arg(&so).arg(&src).status();
        match ok {
            Ok(st) if st.success() => {}
            _ => {
                assert!(
                    std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                    "cc cannot build a shared library"
                );
                return None;
            }
        }
        built.push(elf::parse(&std::fs::read(&so).unwrap()).unwrap());
    }
    let new = built.pop().unwrap();
    Some((built.pop().unwrap(), new))
}

#[test]
fn a_symbol_that_left_is_gone() {
    let Some((old, new)) = pair(
        "gone",
        "void p(void){}\nvoid q(void){}\n",
        "void p(void){}\n",
        None,
    ) else {
        return;
    };
    let seen = elf::compare(&old.exports, &new.exports, false);
    assert!(seen.contains(&elf::Change::Gone("q".into())), "{seen:?}");
}

// still resolves, still links, and the caller now gets NULL where it used to get an
// address. no symbol set difference at all
#[test]
fn global_to_weak_is_a_change() {
    let Some((old, new)) = pair(
        "weak",
        "void p(void){}\n",
        "__attribute__((weak)) void p(void){}\n",
        None,
    ) else {
        return;
    };
    let seen = elf::compare(&old.exports, &new.exports, false);
    assert!(
        seen.contains(&elf::Change::Weakened("p".into())),
        "{seen:?}"
    );
}

// an exported object's size is the object's size, which is how a c++ vtable growing by
// one virtual method shows up while every name stays identical
#[test]
fn an_object_that_grew_is_a_change() {
    let Some((old, new)) = pair("grew", "char t[8];\n", "char t[16];\n", None) else {
        return;
    };
    let seen = elf::compare(&old.exports, &new.exports, false);
    assert!(seen.contains(&elf::Change::Grew("t".into())), "{seen:?}");
}

// glibc's own pattern: keep the old version, make a new one default. every symbol is
// still present and everything linked afterwards binds somewhere else
#[test]
fn moving_the_default_version_is_a_change_on_gnu_and_not_on_musl() {
    let Some((old, new)) = pair(
        "default",
        "void p_impl(void){}\n__asm__(\".symver p_impl,p@@V1\");\n",
        "void p_impl(void){}\n__asm__(\".symver p_impl,p@V1\");\n\
         void p_new(void){}\n__asm__(\".symver p_new,p@@V2\");\n",
        Some("V1 { global: p; local: *; };\nV2 { global: p; local: *; } V1;\n"),
    ) else {
        return;
    };

    let gnu = elf::compare(&old.exports, &new.exports, true);
    assert!(
        gnu.contains(&elf::Change::Undefaulted("p@V1".into())),
        "{gnu:?}"
    );

    // musl's loader ignores versions, so the same two builds changed nothing it can see
    let musl = elf::compare(&old.exports, &new.exports, false);
    assert!(musl.is_empty(), "{musl:?}");
}
