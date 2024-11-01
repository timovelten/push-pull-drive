use rusqlite::types::FromSql;
use rusqlite::ToSql;
use std::borrow::Borrow;
use std::convert::Infallible;
use std::fmt::Display;
use std::ops::Deref;
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

#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct RPathBuf {
    inner: String,
}

impl RPathBuf {
    const SEP: char = RPath::SEP;

    pub fn from_sep(s: &str, sep: char) -> Self {
        Self::from_components(s.split(sep))
    }

    pub fn from_components<'a, I: IntoIterator<Item = &'a str>>(components: I) -> Self {
        let mut ret = Self::new();
        for component in components {
            ret.push(component);
        }
        ret
    }

    pub fn extend(&mut self, other: &RPath) {
        // Make sure to not add empty segments (we disallow those)
        if other.is_empty() {
            return;
        }

        if !self.is_empty() {
            self.inner.push(Self::SEP);
        }

        self.inner.push_str(&other.inner)
    }

    pub fn push(&mut self, other: &str) {
        self.extend(RPath::single(other));
    }

    pub fn new() -> Self {
        Self {
            inner: String::new(),
        }
    }

    pub fn try_from_path<P: AsRef<Path>, Q: AsRef<Path>>(
        base_path: P,
        path: Q,
    ) -> Result<Self, Error> {
        let base_path = base_path.as_ref();
        let path = path.as_ref();

        if !base_path.is_absolute() || !path.is_absolute() {
            return Err(Error::PathNotAbsolute(path.to_owned()));
        }

        let base_components = base_path
            .components()
            .map(Some)
            .chain(std::iter::repeat(None));
        let components = path.components();

        let mut ret = RPathBuf::new();
        for (a, b) in base_components.zip(components) {
            match a {
                Some(a) if a != b => {
                    return Err(Error::PathIsNotSubpathOf {
                        base: base_path.to_owned(),
                        path: path.to_owned(),
                    })
                }
                Some(_) => {}
                None => {
                    // Base path is exhausted, this is the part we care about
                    ret.push(b.as_os_str().to_str().unwrap())
                }
            }
        }

        Ok(ret)
    }

    pub fn into_string(self) -> String {
        self.inner
    }

    pub fn as_rpath(&self) -> &RPath {
        self
    }
}

impl Deref for RPathBuf {
    type Target = RPath;

    #[inline]
    fn deref(&self) -> &Self::Target {
        RPath::new(&self.inner)
    }
}

impl Borrow<RPath> for RPathBuf {
    fn borrow(&self) -> &RPath {
        self.deref()
    }
}

impl ToOwned for RPath {
    type Owned = RPathBuf;

    fn to_owned(&self) -> Self::Owned {
        RPathBuf {
            inner: self.inner.to_owned(),
        }
    }
}

/// A valid RPath (relative path) is has "components" each of which
/// - Does not contain a null byte
/// - Is valid utf8
/// - Is not .. or .
/// - Is not empty
/// We enforce these rules in the "Builder" type RPathBuf.
#[repr(transparent)]
#[derive(Hash, PartialEq, Eq, Ord, PartialOrd, Debug)]
pub struct RPath {
    inner: str,
}

impl RPath {
    pub const ROOT: &'static Self = RPath::new_unchecked("");
    pub const SEP: char = '\x01';
    pub const SEP_STR: &'static str = "\x01";

    // The "unchecked" will not lead to unsaftey in the rust sense. It might lead to incorrect program
    // behavior though.
    #[inline(always)]
    pub const fn new_unchecked(s: &str) -> &RPath {
        unsafe {
            // SAFTEY: RPath is repr(transparent) (see also the impl of str::from_utf8_unchecked)
            std::mem::transmute(s)
        }
    }

    pub fn new(s: &str) -> &RPath {
        Self::new_unchecked(s)
    }

    pub fn single(s: &str) -> &RPath {
        assert!(!s.contains(Self::SEP), "A single component must not contain the separator character {:x}", Self::SEP as u32);
        Self::new(s)
    }

    pub fn split_right(&self) -> (&RPath, &str) {
        match self.inner.rsplit_once(Self::SEP) {
            Some((base, last)) => (RPath::new(base), last),
            None => (RPath::new(""), &self.inner),
        }
    }

    pub fn filename(&self) -> &str {
        let (_, filename) = self.split_right();
        filename
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn join(&self, other: &RPath) -> RPathBuf {
        let mut buf = self.to_owned();
        buf.extend(other);

        buf
    }

    pub fn join_single(&self, other: &str) -> RPathBuf {
        let mut buf = self.to_owned();
        buf.push(other);

        buf
    }

    /// Returns all the subpaths of a path. Note that this ALWAYS included the empty path as
    /// the first subpath.
    pub fn subpaths(&self) -> impl DoubleEndedIterator<Item = &RPath> {
        // I really want to avoid having to write the next_back implementation myself,
        // so we have to leverage the std libraray in a kind of awkward way

        // We emit the empty string once at the beginning. Avoid emitting it again at the end
        // in case the string is empty.
        let end = match self.is_empty() {
            true => None,
            false => Some(self.len()),
        };

        let indices_raw = self.inner.match_indices(Self::SEP).map(|(i, _)| Some(i));
        let indices = [Some(0)]
            .into_iter()
            .chain(indices_raw)
            .chain([end])
            .flatten();

        indices.map(|i| RPath::new_unchecked(&self.inner[..i]))
    }

    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.inner.split(Self::SEP)
    }

    pub fn with_separator(&self, sep: char) -> Result<String, String> {
        let mut ret = String::new();

        let mut failed = false;
        for c in self.components() {
            failed |= c.contains(sep);

            if !ret.is_empty() {
                ret.push(sep);
            }

            ret.push_str(c);
        }

        if failed {
            Err(ret)
        } else {
            Ok(ret)
        }
    }

    /// This is the longest subpath of self (as defined by [Self::subpaths]) that is also
    /// a subpath of other. In that sense, the path returned by this will be a directory
    pub fn intersect<'a>(&'a self, other: &Self) -> &'a Self {
        let mut ret = RPath::ROOT;
        for (x, y) in self.subpaths().zip(other.subpaths()) {
            if x == y {
                ret = x;
            } else {
                break;
            }
        }

        ret
    }

    pub fn starts_with(&self, other: &Self) -> bool {
        self.inner.starts_with(&other.inner)
    }
}

impl Display for RPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for comp in self.components() {
            if !first {
                write!(f, "/")?;
            }

            write!(f, "{}", comp)?;

            first = false;
        }

        Ok(())
    }
}

impl Display for RPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.with_separator('/') {
            Ok(res) => write!(f, "{res}")?,
            Err(res) => write!(f, "[Component contained separator '/'] {res} ")?
        }

        Ok(())
    }
}

impl FromSql for RPathBuf {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let inner: String = <String as FromSql>::column_result(value)?;
        Ok(RPathBuf { inner })
    }
}

impl ToSql for RPath {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        <str as ToSql>::to_sql(&self.inner)
    }
}

impl ToSql for RPathBuf {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.as_rpath().to_sql()
    }
}

impl std::str::FromStr for RPathBuf {
    type Err = Infallible;

    /// This function takes a string that has '/' separated components and turns it into a
    /// a valid RPathBuf. Do note the following restrictions:
    /// - The string must not start with a '/'
    /// - We use backslash to escape slashes. From left to right \/ is replaced by a slash, \\ is replaced by a single backslash. 
    ///   If no such substitution can be done anymore, and a free-standing backslash is left over, this is a syntax error.
    /// - We normalize away occurrences of . and .. if possible
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(RPathBuf::from_sep(s, '/'))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn to_relative_name_test() {
        assert_eq!(
            RPathBuf::try_from_path("/tmp/test", "/tmp/test/hallo/123").unwrap(),
            RPathBuf::from_components(["hallo", "123"])
        );
        assert_eq!(
            RPathBuf::try_from_path("/tmp//test", "/tmp/test/hallo/123").unwrap(),
            RPathBuf::from_components(["hallo", "123"])
        );
    }

    #[test]
    pub fn identity() {
        // Make sure that passing in the same path twice works.
        // We unwrap that at some points.
        let res = RPathBuf::try_from_path("/tmp/test", "/tmp/test").unwrap();
        assert_eq!(&*res, RPath::ROOT);
    }

    #[test]
    pub fn subpaths_works() {
        let rpath: RPathBuf = "videos/2024/good_ones/test.mkv".parse().unwrap();
        let res = rpath.subpaths().map(|x| x.to_owned()).collect::<Vec<_>>();
        let should_be: Vec<RPathBuf> = vec![
            "".parse().unwrap(),
            "videos".parse().unwrap(),
            "videos/2024".parse().unwrap(),
            "videos/2024/good_ones".parse().unwrap(),
            "videos/2024/good_ones/test.mkv".parse().unwrap(),
        ];

        assert_eq!(res, should_be);
    }

    #[test]
    pub fn subpath_works_for_empty() {
        let res = RPath::ROOT.subpaths().collect::<Vec<_>>();
        assert_eq!(res, vec![RPath::ROOT]);
    }
}
