use std::path::Path;
use crate::Error;

fn strip_leading_separator(path: &Path) -> &Path {
    let ret = match path.strip_prefix("/") {
        Ok(path) => path,
        Err(_) => path,
    };

    // TODO: If we ever support windows, also strip backslashes here.

    ret
}

/// Get the relative name
pub fn relative_name<P: AsRef<Path>, Q: AsRef<Path>>(
    base_path: P,
    path: Q,
) -> crate::Result<String> {
    let base_path = base_path.as_ref();
    let path = path.as_ref();

    if !base_path.is_absolute() || !path.is_absolute() {
        return Err(Error::PathNotAbsolute(path.to_owned()));
    }

    if let Ok(relative_path) = path.strip_prefix(base_path) {
        // There is definitely still a leading separator
        let relative_path = strip_leading_separator(relative_path);

        Ok(relative_path
            .to_str()
            .ok_or(Error::PathContainsInvalidUnicode)?
            .to_owned())
    } else {
        return Err(Error::PathIsNotSubpathOf {
            base: base_path.to_owned(),
            path: path.to_owned(),
        });
    }
}

pub fn append_to_relative_name(relative_name: &str, filename: &str) -> String {
    if filename.len() == 0 {
        return relative_name.to_string()
    }

    if relative_name.len() == 0 {
        filename.to_owned()
    } else {
        format!("{}/{}", relative_name, filename)
    }
}

pub fn split_relative_name(relative_name: &str) -> (&str, &str) {
    match relative_name.rsplit_once('/') {
        Some((base, last)) => (base, last),
        None => ("", relative_name),
    }
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
}
