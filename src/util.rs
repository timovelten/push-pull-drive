use std::fmt::Write;

use time::OffsetDateTime;

pub fn u8_slice_to_hex_string(slice: &[u8]) -> String {
    let mut ret = String::with_capacity(slice.len() * 2);
    for b in slice {
        write!(ret, "{:x}", b).unwrap();
    }

    ret
}

pub fn datetime_to_filetime(time: OffsetDateTime) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(time.unix_timestamp(), time.nanosecond())
}

#[allow(dead_code)]
pub mod assert {
    pub const fn send<T: Send>() {}
    pub const fn sync<T: Send>() {}
    pub const fn clone<T: Clone>() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_hex_lowercase() {
        assert_eq!(u8_slice_to_hex_string(&[0xde, 0xad, 0xca, 0xfe, 0x1a, 0x2b, 0x3c, 0x4d]), "deadcafe1a2b3c4d")
    }
}