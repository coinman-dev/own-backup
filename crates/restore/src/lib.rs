pub mod guard;

pub use guard::{
    check_whole_disk_restore, guard_target_disk, guard_target_volume, is_boot_media, protection,
    protected_partitions, system_disk_number, Protection,
};
