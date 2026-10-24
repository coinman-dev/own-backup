// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod guard;

pub use guard::{
    check_whole_disk_restore, guard_target_disk, guard_target_volume, is_boot_media,
    protected_partitions, protection, system_disk_number, Protection,
};
#[cfg(target_os = "linux")]
pub use guard::{guard_plan_volume, guard_target_disk_device, protection_of_device};
