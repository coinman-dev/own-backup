pub mod guard;

pub use guard::{
    check_whole_disk_restore, guard_target_disk, is_boot_media, protected_partitions,
    system_disk_number,
};
