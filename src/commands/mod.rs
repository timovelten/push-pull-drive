// TODO: Move this comment to an appropriate location.
//! We take the time, to document some standing assumptions. Unfortunately, we have to work
//! a lot with paths.
//!
//! In the following, we often speak of "relative names". By this, we mean a string of the form
//! abc/def/ceg
//! i.e. a slash separated path, that does not contain dots. In particular, it does not start with a dot.
pub mod init;
pub mod ls;
pub mod push_pull;
pub mod status;
