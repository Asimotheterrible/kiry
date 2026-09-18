#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

pub mod archive;
pub mod db;
pub mod elf;
pub mod install;
pub mod pkg;

pub fn sha256<R: Read>(mut r: R) -> Result<String, io::Error> {
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    // a file every package must have
    Required(PathBuf),
    Empty(PathBuf),
    // carries the line that would not parse, not the path, because the caller
    // printing this already knows which package it asked for
    Version(String),
    Counts { sources: usize, checksums: usize },
    Name(PathBuf),
    NoPackage(PathBuf),
    Manifest { line: usize, why: &'static str },
    // a path the manifest format cannot represent, so it never gets written
    BadPath(String),
    // an archive, or one member of it, that kiry refuses to touch
    Archive { path: String, why: &'static str },
    // an elf kiry could not make sense of
    Elf { path: String, why: &'static str },
    // every collision in the batch, not the first: a busybox carrying three applets
    // real packages own answered one rebuild at a time otherwise
    Conflict { at: Vec<(String, String)> },
    Targets(PathBuf),
    MissingDep { at: Vec<(String, String)> },
    Needed { pkg: String, by: String },
}

// one finding per line, so a batch with three of them says so once rather than over three
// runs. die prefixes every line it is handed
fn rows(
    f: &mut fmt::Formatter<'_>,
    at: &[(String, String)],
    say: impl Fn(&str, &str) -> String,
) -> fmt::Result {
    let lines: Vec<String> = at.iter().map(|(a, b)| say(a, b)).collect();
    write!(f, "{}", lines.join("\n"))
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Error::Required(p) => write!(f, "{}: required file is missing", p.display()),
            Error::Empty(p) => write!(f, "{} has nothing in it", p.display()),
            Error::Version(s) => write!(f, "expected \"<upstream> <revision>\", got {s:?}"),
            Error::Counts { sources, checksums } => {
                write!(f, "{sources} sources but {checksums} checksums")
            }
            Error::Name(p) => write!(f, "cannot tell the package name from {}", p.display()),
            Error::NoPackage(p) => write!(f, "no package at {}", p.display()),
            Error::Manifest { line, why } => write!(f, "manifest line {line}: {why}"),
            Error::BadPath(p) => write!(f, "cannot record this path: {p:?}"),
            Error::Archive { path, why } => write!(f, "{path}: {why}"),
            Error::Elf { path, why } => write!(f, "{path}: {why}"),
            Error::Conflict { at } => rows(f, at, |p, o| format!("{p} is owned by {o}")),
            Error::Targets(p) => write!(f, "{} must hold exactly one target", p.display()),
            Error::MissingDep { at } => rows(f, at, |p, d| format!("{p} needs {d}")),
            Error::Needed { pkg, by } => write!(f, "{by} still needs {pkg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(_, e) => Some(e),
            _ => None,
        }
    }
}
