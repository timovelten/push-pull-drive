// impl<'a> From<(io::Error, &str)> for Error {
//     fn from((err, msg): (io::Error, &str)) -> Self {
//         let msg = msg.to_owned();

//         if err.kind() == io::ErrorKind::NotFound {
//             Error::NotFound(msg)
//         } else if err.kind() == io::ErrorKind::PermissionDenied {
//             Error::NotAllowed(msg)
//         } else {
//             Error::IoError(err)
//         }
//     }
// }
