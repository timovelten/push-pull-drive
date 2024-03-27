use crate::Result;
use anyhow::bail;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // Path related issues
    #[error("encountered a path that does not consist of valid unicode characters")]
    PathContainsInvalidUnicode,
    #[error("path needs to be absolute here")]
    PathNotAbsolute(PathBuf),
    #[error("{path:?} is not a subpath of {base:?}")]
    PathIsNotSubpathOf { base: PathBuf, path: PathBuf },
}

fn strip_leading_separator(path: &Path) -> &Path {
    let ret = match path.strip_prefix("/") {
        Ok(path) => path,
        Err(_) => path,
    };

    // TODO: If we ever support windows, also strip backslashes here.

    ret
}

/// Convert a path to what we refer to as a "relative name"
/// relative to the given base_path.
pub fn relative_name<P: AsRef<Path>, Q: AsRef<Path>>(base_path: P, path: Q) -> Result<String> {
    // TODO: Make this more robust, i.e. perhaps handle paths that contains dots etc.
    let base_path = base_path.as_ref();
    let path = path.as_ref();

    if !base_path.is_absolute() || !path.is_absolute() {
        bail!(Error::PathNotAbsolute(path.to_owned()));
    }

    if let Ok(relative_path) = path.strip_prefix(base_path) {
        // There is definitely still a leading separator
        let relative_path = strip_leading_separator(relative_path);

        Ok(relative_path
            .to_str()
            .ok_or(Error::PathContainsInvalidUnicode)?
            .to_owned())
    } else {
        bail!(Error::PathIsNotSubpathOf {
            base: base_path.to_owned(),
            path: path.to_owned(),
        });
    }
}

pub fn append_to_cpath_in_place(base_cpath: &mut String, s: &str) {
    if base_cpath.is_empty() {
        write!(base_cpath, "{}", s).unwrap();
        return;
    }

    if s.is_empty() {
        return;
    }

    write!(base_cpath, "/{}", s).unwrap();
}

pub fn append_to_relative_name(relative_name: &str, filename: &str) -> String {
    let mut ret = relative_name.to_owned();
    append_to_cpath_in_place(&mut ret, filename);
    ret
}

pub fn pop_cpath(cpath: &str) -> (&str, &str) {
    match cpath.rsplit_once('/') {
        Some((base, last)) => (base, last),
        None => ("", cpath),
    }
}

pub fn filename(relative_name: &str) -> &str {
    let (_, ret) = pop_cpath(relative_name);
    ret
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn to_relative_name_test() {
        assert_eq!(
            relative_name("/tmp/test", "/tmp/test/hallo/123").unwrap(),
            "hallo/123"
        );
        assert_eq!(relative_name("/tmp/test", "/tmp/test").unwrap(), "");
        assert_eq!(
            relative_name("/tmp//test", "/tmp/test/hallo/123").unwrap(),
            "hallo/123"
        );
    }

    #[test]
    pub fn identity() {
        // Make sure that passing in the same path twice works.
        // We unwrap that at some points.
        assert_eq!(relative_name("/tmp/test", "/tmp/test").unwrap(), "");
    }
}
