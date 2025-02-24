use anyhow::bail;
use core::str;
use std::iter::Peekable;
use rusqlite::types::{FromSql, ToSqlOutput};
use rusqlite::ToSql;
use serde::Deserialize;
use std::borrow::Borrow;
use std::fmt::{Debug, Display};
use std::ops::Deref;
use std::path::Path as StdPath;

/// 0xff does not appear in any valid utf-8 string
const SEP: u8 = 0xff;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RPathBuf {
    inner: Vec<u8>,
}

impl Debug for RPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.as_rpath(), f)
    }
}

impl RPathBuf {
    pub fn empty() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    /// This will panic when called with an empty string
    pub fn push(&mut self, s: &str) {
        // This does not introduce a trailing separator as s is non-empty
        if !self.is_empty() {
            self.inner.push(SEP);
        }

        self.inner.extend_from_slice(s.as_bytes())
    }

    pub fn extend(&mut self, other: &RPath) {
        for comp in other.components() {
            self.push(comp);
        }
    }

    /// Note: All empty components will be ignored (silently)
    pub fn from_components<'a, I: IntoIterator<Item = &'a str>>(components: I) -> Self {
        let mut ret = Self::empty();
        for component in components {
            if !component.is_empty() {
                ret.push(component);
            }
        }
        ret
    }

    /// This may panic
    pub fn from_sep(s: &str, sep: char) -> Self {
        if s.is_empty() {
            Self::empty()
        } else {
            Self::from_components(s.split(sep))
        }
    }

    pub fn try_from_path<P: AsRef<StdPath>, Q: AsRef<StdPath>>(
        base_path: P,
        path: Q,
    ) -> anyhow::Result<Self> {
        let base_path = base_path.as_ref();
        let path = path.as_ref();

        if !base_path.is_absolute() || !path.is_absolute() {
            bail!("{path:?} is not absolute");
        }

        let base_components = base_path
            .components()
            .map(Some)
            .chain(std::iter::repeat(None));
        let components = path.components();

        let mut ret = RPathBuf::empty();
        for (a, b) in base_components.zip(components) {
            match a {
                Some(a) if a != b => {
                    bail!("The path {base_path:?} is not a subpath of {path:?}");
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

    pub fn as_rpath(&self) -> &RPath {
        self
    }

    pub fn deserialize_sloppy(s: &str) -> Self {
        if !s.ends_with('/') {
            Self::deserialize(&format!("{s}/"))
        } else {
            Self::deserialize(s)
        }
    }

    pub fn deserialize(s: &str) -> Self {
        let mut ret = Self::empty();
        for segment in s.split('/') {
            let decoded = decode(segment);
            ret.push(&decoded);
        }

        ret
    }
}

impl Deref for RPathBuf {
    type Target = RPath;

    #[inline]
    fn deref(&self) -> &Self::Target {
        RPath::from_parts(&self.inner)
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

#[repr(transparent)]
#[derive(Hash, PartialEq, Eq, Ord, PartialOrd)]
pub struct RPath {
    /// We consider it a logic error for any one String to be empty
    inner: [u8],
}

impl Debug for RPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\"", self.serialize())
    }
}

impl RPath {
    pub const fn empty() -> &'static Self {
        RPath::from_parts(&[])
    }

    /// The byte array here should satisfy the following requirements (everything else is considered
    /// a logic error - we currently do not rely on these for saftey)
    /// - s consists of valid utf8 strings separated by [SEP]
    /// - s does not have a beginning or trailing separator
    /// - Each component is non empty. In other words: the encoded string does not contain two or more
    ///     consecutive slashes
    const fn from_parts(s: &[u8]) -> &RPath {
        unsafe {
            // SAFTEY: RPath is repr(transparent)
            std::mem::transmute(s)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    // Basic operations

    /// Returns all the subpaths of a path. Note that this ALWAYS included the empty path as
    /// the first subpath.
    pub fn subpaths(&self) -> impl DoubleEndedIterator<Item = &RPath> {
        let seps = (0..=self.inner.len())
            .filter(|&i| i == 0 || i == self.inner.len() || self.inner[i] == SEP);

        seps.map(|i| Self::from_parts(&self.inner[0..i]))
    }

    /// This returns `None` if and only if self is empty. If this is called on a path
    /// with only one segment, that is segment is returned as a str, and the first
    /// return value is empty
    pub fn split_right(&self) -> Option<(&RPath, &str)> {
        if self.is_empty() {
            return None;
        }

        // Index of the right most separator
        let idx = (0..self.inner.len())
            .rev()
            .position(|i| self.inner[i] == SEP)
            .unwrap_or(0);

        let base = Self::from_parts(&self.inner[0..idx]);
        let last = core::str::from_utf8(&self.inner[idx..]).unwrap();

        Some((base, last))
    }

    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        // We could use unsafe here, because we do enforce that the components are valid utf 8
        self.inner
            .split(|x| *x == SEP)
            .map(|s| core::str::from_utf8(s).unwrap())
    }

    pub fn filename(&self) -> Option<&str> {
        let (_, filename) = self.split_right()?;
        Some(filename)
    }

    pub fn parent(&self) -> Option<&RPath> {
        let (parent, _) = self.split_right()?;
        Some(parent)
    }

    pub fn join(&self, other: &RPath) -> RPathBuf {
        let mut buf = self.to_owned();
        buf.extend(other);

        buf
    }

    pub fn join_single(&self, other: &str) -> RPathBuf {
        let mut buf = self.to_owned();
        buf.push(other);

        println!("Join single {self:?} + {other} --> {buf:?}");

        buf
    }

    /// Determine if there is a relationship between these two paths (i.e. one is a child of the other),
    /// according to starts_with.
    pub fn is_related_to(&self, other: &RPath) -> bool {
        self.starts_with(other) || other.starts_with(self)
    }

    /// Length is the number of components
    pub fn len(&self) -> usize {
        self.components().count()
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

    pub fn starts_with(&self, other: &Self) -> bool {
        if other.is_empty() {
            return true;
        }

        let this = &self.inner;
        let other = &other.inner;

        this.starts_with(&other)
            && (this.len() == other.len() || this[other.len()] == SEP)
    }

    /// Emit a slash separated string, where each component is encoded such that it contains
    /// no slashes. Note that we do emit a leading slash.
    pub fn serialize(&self) -> String {
        let mut ret = String::new();
        for comp in self.components() {
            ret.push('/');
            ret.push_str(&encode(comp));
        }

        ret
    }
}

/// Encode a string so that the resulting string does not contain the slash character.
fn encode(s: &str) -> String {
    // Currently, we do this by
    // 1) '/' --> '\#'
    // 2) '\' --> '\\'
    let mut ret = Vec::new();

    for &b in s.as_bytes() {
        match b {
            b'/' => ret.extend_from_slice(&[b'\\', b'x', b'2', b'f']),
            b'\\' => ret.extend_from_slice(&[b'\\', b'\\']),
            b => ret.push(b)
        }
    }

    String::from_utf8(ret).unwrap()
}

fn decode(s: &str) -> String {
    fn try_eat<const N: usize, I: Iterator<Item=u8>>(src: &mut Peekable<I>, bs: [u8; N]) -> Result<(), ()> {
        for b in bs {
            if src.next_if_eq(&b).is_none() {
                return Err(());
            }
        }

        Ok(())
    }

    let mut ret = Vec::new();

    let mut chars = s.as_bytes().iter().copied().peekable();
    while let Some(c) = chars.next() {
        if c != b'\\' {
            ret.push(c);
            continue
        }

        // We have encountered the escape character. Expand it.
        if try_eat(&mut chars, [b'\\']).is_ok() {
            ret.push(b'\\')
        } else if try_eat(&mut chars, [b'x', b'2', b'f']).is_ok() {
            ret.push(b'/')
        } else {
            panic!("only '/' and '\\' maye be escaped")
        }
    }

    String::from_utf8(ret).expect("valid utf8 in valid utf8 out")
}

impl<'de> serde::Deserialize<'de> for RPathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let serialized: String = Deserialize::deserialize(deserializer)?;
        Ok(Self::deserialize(&serialized))
    }
}

impl serde::Serialize for RPathBuf {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.as_rpath(), serializer)
    }
}

impl serde::Serialize for &RPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let serialized = RPath::serialize(&self);
        serialized.serialize(serializer)
    }
}

#[cfg(test)]
impl From<&'static str> for &'static RPath {
    /// This is useful in testing to materialze &'static RPaths "out of thin air"
    fn from(value: &'static str) -> Self {
        use ahash::HashMap;
        use std::sync::{LazyLock, Mutex};

        static CACHE: LazyLock<Mutex<HashMap<&'static str, &'static RPath>>> =
            LazyLock::new(Default::default);
        let mut map = CACHE.lock().unwrap();

        map.entry(value)
            .or_insert_with(|| Box::leak(Box::new(RPathBuf::from(value))))
    }
}

#[cfg(test)]
impl From<&str> for RPathBuf {
    fn from(value: &str) -> Self {
        RPathBuf::from_sep(value, '/')
    }
}

impl Display for RPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self.as_rpath(), f)
    }
}

impl Display for RPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.serialize())
    }
}

impl FromSql for RPathBuf {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(RPathBuf::deserialize(value.as_str()?))
    }
}

impl ToSql for RPath {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let serialized = self.serialize();
        Ok(ToSqlOutput::Owned(rusqlite::types::Value::Text(serialized)))
    }
}

impl ToSql for RPathBuf {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.as_rpath().to_sql()
    }
}

// This is useful for keeping testing code short mostly
impl<'a, const N: usize> From<[&'a str; N]> for RPathBuf {
    fn from(value: [&'a str; N]) -> Self {
        let mut ret = Self::empty();
        for component in value {
            ret.push(component);
        }
        ret
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use rand::{distr::Uniform, prelude::*, Rng};

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
        assert_eq!(&*res, RPath::empty());
    }

    #[test]
    pub fn subpaths_works() {
        let rpath: RPathBuf = ["videos", "2024", "good_ones", "test.mkv"].into();
        let res = rpath.subpaths().map(|x| x.to_owned()).collect::<Vec<_>>();
        let should_be: Vec<RPathBuf> = vec![
            RPathBuf::empty(),
            ["videos"].into(),
            ["videos", "2024"].into(),
            ["videos", "2024", "good_ones"].into(),
            ["videos", "2024", "good_ones", "test.mkv"].into(),
        ];

        assert_eq!(res, should_be);
    }

    #[test]
    pub fn subpath_works_for_empty() {
        let res = RPath::empty().subpaths().collect::<Vec<_>>();
        assert_eq!(res, vec![RPath::empty()]);
    }

    #[test]
    pub fn serialize_basic() {
        let rpath = RPathBuf::from_components(["test", "path", "without", "anything", "weird"]);
        let serialized = rpath.serialize();

        assert_eq!(RPathBuf::deserialize(&serialized), rpath);
    }

    #[test]
    pub fn serialize_ord() {
        // Sorting these, and sorting their serialized representations should give the same result
        let rpath1 = RPathBuf::from_components(["with_a_/", "dir1", "dir2", "file_with_/"]);
        let rpath2 = RPathBuf::from_components(["with_a_/", "dir1", "dir2"]);
        let rpath3 = RPathBuf::from_components(["with_a_/", "dir1"]);

        // Sorting their serialized representations should - at least in this case - give the same
        // order
        let mut serialized = [&rpath1, &rpath2, &rpath3]
            .into_iter()
            .map(|rpath| rpath.serialize())
            .collect::<Vec<_>>();
        serialized.sort();

        let deserialized = serialized
            .into_iter()
            .map(|s| RPathBuf::deserialize(&s))
            .collect::<Vec<_>>();

        let mut should_be = vec![rpath1, rpath2, rpath3];
        should_be.sort();

        assert_eq!(deserialized, should_be);
    }

    #[test]
    #[should_panic]
    pub fn illegally_escaped_char() {
        RPathBuf::deserialize("escpaing_the_wrong_character_\\a");
    }

    #[test]
    #[should_panic]
    pub fn ends_unexpected() {
        RPathBuf::deserialize("there_should_be_at_least_one_more_char_\\");
    }

    fn random_path(alphabet: &[char], rng: &mut impl Rng) -> RPathBuf {
        let mut ret = RPathBuf::empty();
        let uniform = Uniform::new(0, alphabet.len()).unwrap();

        for _ in 0..8 {
            let word = uniform
                .sample_iter(&mut *rng)
                .take(10)
                .map(|i| alphabet[i])
                .collect::<String>();
            ret.push(&word);
        }

        ret
    }

    #[test]
    pub fn deserialize_inverts_serialize() {
        let alphabet = "abcdef/\\".chars().collect::<Vec<_>>();
        let mut rng = SmallRng::seed_from_u64(123456);
        for _ in 0..1000 {
            let path = random_path(&alphabet, &mut rng);
            assert_eq!(path, RPathBuf::deserialize(&path.serialize()));
        }
    }

    #[test]
    pub fn trailing_separator_is_ignored() {
        assert_eq!(
            RPathBuf::from_sep("abc/def/fgh/", '/'),
            RPathBuf::from_sep("abc/def/fgh", '/')
        )
    }

    #[test]
    pub fn starts_with_checks_components() {
        let base = RPathBuf::from_components(["abc", "def"]);
        let a = RPathBuf::from_components(["abc", "def", "ghj"]);
        let b = RPathBuf::from_components(["abc", "def", "ghjk"]);

        assert!(a.starts_with(&base));
        assert!(b.starts_with(&base));

        assert!(!b.starts_with(&a));
    }

    #[test]
    pub fn starts_with_is_compatible_with_subpaths() {
        let path = RPathBuf::from_components(["abc", "def", "ghi", "jkl", "opq"]);
        for subpath in path.subpaths() {
            assert!(path.starts_with(subpath));
        }
    }

    #[test]
    pub fn split_right_one_component() {
        let a = RPathBuf::from_components(["abc"]);
        assert_matches!(a.split_right(), Some((path, "abc")) if path.is_empty());
    }
}
