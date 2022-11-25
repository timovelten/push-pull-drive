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