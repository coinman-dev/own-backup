//! The platform-independent half of the backup engine.
//!
//! What lives here does not care which system it runs on: reading an image
//! back, checking it against its checksums, and (as the shared engine grows)
//! the image targets and whole-disk building blocks. Both engines use it —
//! `engine_win` and `engine_linux` re-export it under the names their callers
//! already know — so a fix made here is made once.

pub(crate) mod disk_image;
pub(crate) mod read;
pub(crate) mod target;
pub(crate) mod verify;

#[cfg(test)]
pub(crate) mod testkit;
