#![cfg(windows)]

use own_backup_backup::letters_win::assign_letter;
use own_backup_backup::volume_win::{volume_drive_letters, volume_root_at_offset};

#[test]
#[ignore = "requires the dedicated Windows lab disk and OWN_BACKUP_A09_SOURCE_OFFSET"]
fn assigning_a_letter_is_idempotent_and_a_taken_letter_leaves_the_old_one() {
    let offset: u64 = std::env::var("OWN_BACKUP_A09_SOURCE_OFFSET")
        .expect("fixture offset")
        .parse()
        .unwrap();
    let volume = volume_root_at_offset(1, offset).expect("source E: volume");
    let original = volume_drive_letters(&volume);
    assert!(original.iter().any(|s| s == "E:"), "{original:?}");
    assign_letter(&volume, 'E').unwrap();
    assert_eq!(volume_drive_letters(&volume), original);
    assert!(
        assign_letter(&volume, 'C').is_err(),
        "system letter is occupied"
    );
    assert_eq!(volume_drive_letters(&volume), original);
}
