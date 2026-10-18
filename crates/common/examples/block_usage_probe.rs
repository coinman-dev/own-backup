//! Print what is using each given block device (`linux_block::device_usage`).
//! `cargo run -p own-backup-common --example block_usage_probe -- /dev/nvme0n1p3 …`

fn main() {
    #[cfg(unix)]
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        let name = own_backup_common::linux_block::kernel_name(path);
        match own_backup_common::linux_block::device_usage(path) {
            Ok(Some(u)) => println!(
                "{arg} ({}) idle={} in_use={} {u:?}",
                name.unwrap_or_default(),
                u.idle(),
                u.in_use()
            ),
            Ok(None) => println!("{arg}: not a block device"),
            Err(e) => println!("{arg}: {e}"),
        }
    }
}
