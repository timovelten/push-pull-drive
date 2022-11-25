use std::fmt::Write;

pub fn u8_slice_to_hex_string(slice: &[u8]) -> String {
    let mut ret = String::with_capacity(slice.len() * 2);
    for b in slice {
        write!(ret, "{:x}", b).unwrap();
    }

    ret
}
