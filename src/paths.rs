use anyhow::bail;
use core::str;
use serde::de::Visitor;
use std::borrow::Borrow;
use std::fmt::{Debug, Display};
use std::iter::Peekable;
use std::ops::Deref;
use std::path::Path as StdPath;

/// 0xff does not appear in any valid utf-8 string
const SEP: u8 = 0xff;

#[derive(Clone, PartialEq, Eq, Hash)]
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
    pub fn push(&mut self, segment: &str) {
        assert!(!segment.is_empty(), "segment must not be empty");

        // If self is empty, no separators are needed. If self is not empty,
        // we need to separate the new segment from it
        if !self.is_empty() {
            self.inner.push(SEP);
        }

        self.inner.extend_from_slice(segment.as_bytes())
    }

    pub fn extend(&mut self, other: &RPath) {
        for comp in other.components() {
            self.push(comp);
        }
    }

    /// Note: All empty components will be ignored (silently)
    pub fn from_components<S: AsRef<str>, I: IntoIterator<Item = S>>(components: I) -> Self {
        let mut ret = Self::empty();
        for component in components {
            let component = component.as_ref();

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

    pub fn try_from_std_path<P: AsRef<StdPath>, Q: AsRef<StdPath>>(
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
        let segments = s.split('/').filter(|s| !s.is_empty());

        Self::from_components(segments)
    }

    pub fn deserialize(s: &str) -> anyhow::Result<Self> {
        if s.starts_with('/') || s.ends_with('/') {
            bail!("Malformed: Leading or trailing slash");
        }

        let mut ret = Self::empty();
        for segment in s.split('/') {
            if segment.is_empty() {
                bail!("Malformed: Consecutive slashes")
            }

            let decoded = decode(segment);
            ret.push(&decoded);
        }

        Ok(ret)
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
#[derive(Hash, PartialEq, Eq)]
pub struct RPath {
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

    /// The byte array here should satisfy the following requirements
    /// - s consists of valid utf8 strings separated by [SEP]
    /// - s does not have a beginning or trailing separator
    /// - Each component is non empty. In other words: the encoded string does not contain two or more
    ///   consecutive slashes
    /// 
    /// Violating these will not lead to unsafety, but it is considered a logic error.
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
    pub fn split_right(&self) -> Option<(&str, &RPath)> {
        if self.is_empty() {
            return None;
        }

        // Index of the right most separator
        let idx = memchr::memrchr(SEP, &self.inner);
        let (rem, segment) = match idx {
            Some(idx) => (&self.inner[0..idx], &self.inner[idx + 1..]),
            None => (&[] as &[u8], &self.inner),
        };

        Some((
            core::str::from_utf8(segment).unwrap(),
            Self::from_parts(rem),
        ))
    }

    pub fn split_left(&self) -> Option<(&str, &RPath)> {
        if self.is_empty() {
            return None;
        }

        // Index of the left most separator
        let idx = memchr::memchr(SEP, &self.inner);
        let (segment, rem) = match idx {
            Some(idx) => (&self.inner[0..idx], &self.inner[idx + 1..]),
            None => (&self.inner, &[] as &[u8]),
        };

        Some((
            core::str::from_utf8(segment).unwrap(),
            Self::from_parts(rem),
        ))
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        Segments { remaining: self }
    }

    pub fn filename(&self) -> Option<&str> {
        let (filename, _) = self.split_right()?;
        Some(filename)
    }

    pub fn parent(&self) -> Option<&RPath> {
        let (_, parent) = self.split_right()?;
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

        this.starts_with(other) && (this.len() == other.len() || this[other.len()] == SEP)
    }

    /// Emit a slash separated string, where each component is encoded such that it contains
    /// no slashes. No trailing or leading slashes are emitted.
    pub fn serialize(&self) -> String {
        let mut ret = String::new();
        let mut first = true;
        for comp in self.components() {
            if first {
                first = false;
            } else {
                ret.push('/');
            }

            ret.push_str(&encode(comp));
        }

        ret
    }
}

struct Segments<'a> {
    remaining: &'a RPath,
}

impl<'a> Iterator for Segments<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self.remaining.split_left() {
            Some((segment, rem)) => {
                self.remaining = rem;
                Some(segment)
            }
            None => None,
        }
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
            b'/' => ret.extend_from_slice(b"\\#"),
            b'\\' => ret.extend_from_slice(b"\\\\"),
            b => ret.push(b),
        }
    }

    String::from_utf8(ret).unwrap()
}

fn decode(s: &str) -> String {
    fn try_eat<I: Iterator<Item = u8>>(
        src: &mut Peekable<I>,
        bs: &[u8],
    ) -> Result<(), ()> {
        for b in bs {
            if src.next_if_eq(b).is_none() {
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
            continue;
        }

        // We have encountered the escape character. Expand it.
        if try_eat(&mut chars, b"\\").is_ok() {
            ret.push(b'\\')
        } else if try_eat(&mut chars, b"#").is_ok() {
            ret.push(b'/')
        } else {
            panic!("only '/' and '\\' maye be escaped")
        }
    }

    String::from_utf8(ret).expect("valid utf8 in valid utf8 out")
}

impl PartialOrd for RPathBuf {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RPathBuf {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_rpath().cmp(other)
    }
}

impl PartialOrd for RPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.components().cmp(other.components())
    }
}

impl<'de> serde::Deserialize<'de> for RPathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RPathVisitor;

        impl<'de> Visitor<'de> for RPathVisitor {
            type Value = RPathBuf;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a valid serialized path")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match RPathBuf::deserialize(v) {
                    Ok(ret) => Ok(ret),
                    Err(err) => Err(E::custom(format!("{err}"))),
                }
            }
        }

        deserializer.deserialize_str(RPathVisitor)
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
        let serialized = RPath::serialize(self);
        serialized.serialize(serializer)
    }
}

#[cfg(test)]
impl From<&'static str> for &'static RPath {
    /// This is useful in testing to materialze &'static RPaths "out of thin air"
    fn from(value: &'static str) -> Self {
        Box::leak(Box::new(RPathBuf::from(value)))
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
    use std::assert_matches;
    use rand::{distr::Uniform, prelude::*, Rng};

    use super::*;

    #[test]
    pub fn to_relative_name_test() {
        assert_eq!(
            RPathBuf::try_from_std_path("/tmp/test", "/tmp/test/hallo/123").unwrap(),
            RPathBuf::from_components(["hallo", "123"])
        );
        assert_eq!(
            RPathBuf::try_from_std_path("/tmp//test", "/tmp/test/hallo/123").unwrap(),
            RPathBuf::from_components(["hallo", "123"])
        );
    }

    #[test]
    pub fn identity() {
        // Make sure that passing in the same path twice works.
        // We unwrap that at some points.
        let res = RPathBuf::try_from_std_path("/tmp/test", "/tmp/test").unwrap();
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

        assert_eq!(RPathBuf::deserialize(&serialized).unwrap(), rpath);
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
            .map(|s| RPathBuf::deserialize(&s).unwrap())
            .collect::<Vec<_>>();

        let mut should_be = vec![rpath1, rpath2, rpath3];
        should_be.sort();

        assert_eq!(deserialized, should_be);
    }

    #[test]
    pub fn ord_is_lexiographic() {
        // We need the order to work on a component level, so that paths with the same
        // components end up being adjacent when sorted
        let mut vecs: Vec<_> = vec![
            vec!["a"],
            vec!["a", "b"],
            vec!["a", "b", "c"],
            vec!["a2"],
            vec!["a2", "b"],
            vec!["a2", "b", "c"],
            vec!["a2", "c"],
        ];
        vecs.sort();

        let paths = vecs
            .iter()
            .map(|segments| RPathBuf::from_components(segments.iter().copied()))
            .collect::<Vec<_>>();

        let mut sorted = paths.clone();
        sorted.sort();

        assert_eq!(paths, sorted)
    }

    #[test]
    #[should_panic]
    pub fn illegally_escaped_char() {
        RPathBuf::deserialize("escpaing_the_wrong_character_\\a").unwrap();
    }

    #[test]
    #[should_panic]
    pub fn ends_unexpected() {
        RPathBuf::deserialize("there_should_be_at_least_one_more_char_\\").unwrap();
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
            assert_eq!(path, RPathBuf::deserialize(&path.serialize()).unwrap());
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
        assert_matches!(a.split_right(), Some(("abc", path)) if path.is_empty());
    }

    #[test]
    pub fn split_right_works() {
        let a = RPathBuf::from_components(["abc", "def", "ghj"]);
        assert_matches!(a.split_right(), Some(("ghj", path)) if path.len() == 2);
    }

    #[test]
    pub fn serde_works() {
        let path = RPathBuf::from_components([
            "a",
            "complicated/",
            "path/a",
            "that_contains",
            "weird # $ $ @ ä ü",
            "characters",
        ]);
        let json = serde_json::to_string(&path).unwrap();
        let deserialized: RPathBuf = serde_json::from_str(&json).unwrap();

        assert_eq!(path, deserialized);
    }

    #[test]
    pub fn empty_path_has_no_segments() {
        let segments = RPathBuf::empty().components().count();
        assert_eq!(segments, 0)
    }

    #[test]
    pub fn segments_works() {
        let components = [
            "a",
            "complicated/",
            "path/a",
            "that_contains",
            "weird # $ $ @ ä ü",
            "characters",
        ];
        let path = RPathBuf::from_components(components.iter().copied());

        let res: [&str; 6] = path.components().collect::<Vec<_>>().try_into().unwrap();
        assert_eq!(components, res)
    }

    #[test]
    pub fn len_works() {
        assert_eq!(RPathBuf::empty().len(), 0);
        assert_eq!(RPathBuf::from_components(["abc"]).len(), 1);
        assert_eq!(RPathBuf::from_components(["abc", "def", "hjk"]).len(), 3);
    }
}
