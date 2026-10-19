//! The platform-independent half of the backup engine.
//!
//! What lives here does not care which system it runs on: reading an image
//! back, checking it against its checksums, the image targets, the volume
//! backup pipeline (`volume`, run on an [`platform::EnginePlatform`]) and, as
//! the shared engine grows, the whole-disk building blocks. Both engines use
//! it — `engine_win` and `engine_linux` re-export it under the names their
//! callers already know — so a fix made here is made once.

pub(crate) mod chain;
// The whole-disk building blocks run on Windows only until phase E6.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod disk;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod disk_image;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod disk_prepare;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod disk_target;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod partition_prep;
pub(crate) mod platform;
pub(crate) mod read;
pub(crate) mod source;
pub(crate) mod target;
pub(crate) mod verify;
pub(crate) mod volume;
pub(crate) mod volume_exclusions;
pub(crate) mod volume_maps;
pub(crate) mod volume_write;

#[cfg(test)]
pub(crate) mod test_platform;
#[cfg(test)]
pub(crate) mod testkit;
