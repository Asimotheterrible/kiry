// the reader against the tool everyone else already trusts, over whatever this
// machine has installed. same reason the tar suite runs three tars: a fixture
// written by the understanding that wrote the parser agrees with it by
// construction and proves nothing

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
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
    for (p, bytes) in &corpus {
        let got = elf::parse(bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let want = &theirs[p];

        assert_eq!(got.soname, want.soname, "soname of {}", p.display());
        assert_eq!(got.needed, want.needed, "needed of {}", p.display());
        assert_eq!(got.rpath, want.rpath, "rpath of {}", p.display());
        assert_eq!(got.runpath, want.runpath, "runpath of {}", p.display());

        sonames += usize::from(got.soname.is_some());
        rpaths += usize::from(got.rpath.is_some() || got.runpath.is_some());
    }

    // if the corpus somehow held nothing interesting the comparison above is empty
    assert!(sonames > 10, "only {sonames} sonames in {}", corpus.len());
    assert!(rpaths > 0, "no library carried an rpath or runpath");
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
