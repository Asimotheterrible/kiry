#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt;
use std::io;
use std::path::PathBuf;

pub mod archive;
pub mod db;
pub mod install;
pub mod pkg;

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
    Conflict { path: String, owner: String },
    Targets(PathBuf),
    MissingDep { pkg: String, dep: String },
    Needed { pkg: String, by: String },
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
            Error::Conflict { path, owner } => write!(f, "{path} is owned by {owner}"),
            Error::Targets(p) => write!(f, "{} must hold exactly one target", p.display()),
            Error::MissingDep { pkg, dep } => write!(f, "{pkg} needs {dep}"),
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
