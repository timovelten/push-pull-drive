use core::str;
use anyhow::bail;
use rusqlite::types::{FromSql, ToSqlOutput};
use rusqlite::ToSql;
use std::borrow::Borrow;
use std::fmt::Display;
use std::ops::Deref;
use std::path::Path;

#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct RPathBuf {
    inner: Vec<String>,
}

impl RPathBuf {
    pub fn empty() -> Self {
        Self { inner: Vec::new() }
    }

    /// This will panic when called with an empty string
    pub fn push(&mut self, s: impl Into<String>) {
        let s: String = s.into();
        assert!(!s.is_empty());
        self.inner.push(s)
    }

    pub fn extend(&mut self, other: &RPath) {
        self.inner.extend(other.inner.iter().cloned())
    }

    /// This will panic if the iterator emits an empty string
    pub fn from_components<'a, T: Into<String>, I: IntoIterator<Item = T>>(components: I) -> Self {
        let mut ret = Self::empty();
        for component in components {
            ret.push(component.into());
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

    pub fn try_from_path<P: AsRef<Path>, Q: AsRef<Path>>(
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

    pub fn deserialize(s: &str) -> Self {
        let mut ret = Self::empty();
        let mut current_component = Vec::new();

        let mut is_escaped = false;

        // I benchmarked this, and this seems to be faster than the version I wrote
        // up that used the optimized memchr to search for / or \
        for b in s.as_bytes().iter().copied() {
            // At this point is_escaped indicates whether the character b is escaped or not
            match (b, is_escaped) {
                (c @ b'\\', true) | (c @ b'/', true) => {
                    current_component.push(c);
                    is_escaped = false;
                }
                (c, true) => panic!("Illegally escaped character {}", c),
                (b'\\', false) => {
                    is_escaped = true;
                }
                (b'/', false) => {
                    ret.push(String::from_utf8(current_component).unwrap());
                    is_escaped = false;
                    current_component = Default::default();
                }
                (c, false) => {
                    current_component.push(c);
                    is_escaped = false;
                }
            }
        }

        debug_assert!(current_component.is_empty());
        assert!(!is_escaped, "Expected escaped character");

        ret
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

#[repr(transparent)]
#[derive(Hash, PartialEq, Eq, Ord, PartialOrd, Debug)]
pub struct RPath {
    /// We consider it a logic error for any one String to be empty
    inner: [String],
}

impl RPath {
    pub const fn empty() -> &'static Self {
        RPath::new(&[])
    }

    #[inline(always)]
    pub const fn new(s: &[String]) -> &RPath {
        unsafe {
            // SAFTEY: RPath is repr(transparent) (see also the impl of str::from_utf8_unchecked)
            std::mem::transmute(s)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn split_right(&self) -> Option<(&RPath, &str)> {
        let (last, rest) = self.inner.split_last()?;
        Some((RPath::new(rest), last))
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

        buf
    }

    /// Returns all the subpaths of a path. Note that this ALWAYS included the empty path as
    /// the first subpath.
    pub fn subpaths(&self) -> impl DoubleEndedIterator<Item = &RPath> {
        (0..=self.inner.len()).map(|i| {
            let new_components = &self.inner[0..i];
            Self::new(&new_components)
        })
    }

    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.inner.iter().map(|s| s.as_str())
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
        let mut ret = RPath::empty();
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

    /// Represent this path as a string. This is done as follows:
    ///
    /// 1) Escape slashes within each component. That is, if a component is "a/component" it is
    ///        turned into "a\/component"
    /// 2) Emit these escaped components separated by slashes
    /// 3) Place a trailing slash at the end
    pub fn serialize(&self) -> String {
        fn next_delimiter(s: &[u8]) -> Option<usize> {
            // This could use memchr if we wanted to!
            s.iter().position(|x| *x == b'/' || *x == b'\\')
        }

        let mut ret: Vec<u8> = Vec::new();
        for comp in self.components() {
            // For now, let's do this naivly. This
            let mut rem = comp.as_bytes();
            while let Some(idx) = next_delimiter(rem) {
                ret.extend(&rem[..idx]);
                ret.push(b'\\');
                ret.push(rem[idx]);

                rem = &rem[idx + 1..]
            }

            ret.extend(rem);
            ret.push(b'/')
        }

        // We could make this unsafe if we wanted to
        String::from_utf8(ret).expect("valid utf8 in valid utf8 out")
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
            Err(res) => write!(f, "[Component contained separator '/'] {res} ")?,
        }

        Ok(())
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
impl<const N: usize, T: Into<String>> From<[T; N]> for RPathBuf {
    fn from(value: [T; N]) -> Self {
        let mut ret = Self::empty();
        for component in value {
            ret.push(component);
        }
        ret
    }
}

#[cfg(test)]
mod test {
    use rand::{distributions::Uniform, prelude::*, Rng};

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
        assert_eq!(serialized, "test/path/without/anything/weird/");
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
        let uniform = Uniform::new(0, alphabet.len());
    
        for _ in 0..8 {
            let word = uniform
                .sample_iter(&mut *rng)
                .take(10)
                .map(|i| alphabet[i])
                .collect::<String>();
            ret.push(word);
        }

        ret
    }

    #[test]
    pub fn deserialize_inverts_serialize() {
        let alphabet = "abcdef/\\".chars().collect::<Vec<_>>();
        let mut rng = SmallRng::seed_from_u64(123456);
        for _ in 0..1000 {
            let path =  random_path(&alphabet, &mut rng);
            assert_eq!(path, RPathBuf::deserialize(&path.serialize()));
        }
    }
}
