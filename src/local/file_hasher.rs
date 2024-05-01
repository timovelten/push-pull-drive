use sha1::Digest;
use sha1::Sha1;
use std::io;

pub trait FileHasher: Copy + Send + Sync + std::fmt::Debug + 'static {
    fn hash<R: io::Read>(self, src: &mut R) -> io::Result<Vec<u8>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sha1Hasher;

impl FileHasher for Sha1Hasher {
    fn hash<R: io::Read>(self, src: &mut R) -> io::Result<Vec<u8>> {
        let mut hasher = Sha1::new();
        std::io::copy(src, &mut hasher)?;
        Ok(hasher.finalize().to_vec())
    }
}
