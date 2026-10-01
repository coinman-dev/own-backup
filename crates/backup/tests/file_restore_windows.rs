#![cfg(windows)]

use own_backup_backup::file_restore_win::{copy_tree, measure_tree, verify_tree};
use own_backup_common::cancel::CancelToken;

#[test]
fn verification_can_be_cancelled_within_one_file_stream() {
    use own_backup_backup::file_restore_win::verify_tree_with_progress;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let data = vec![0x5a; 4 * 1024 * 1024];
    std::fs::write(src.join("large.bin"), &data).unwrap();
    std::fs::write(dst.join("large.bin"), &data).unwrap();
    let cancel = CancelToken::default();
    let mut observed = 0;
    let error = verify_tree_with_progress(&src, &dst, &cancel, |p| {
        assert!(p.bytes_done >= observed);
        observed = p.bytes_done;
        if p.bytes_done >= 2 * 1024 * 1024 {
            cancel.cancel();
        }
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(
        error,
        own_backup_common::OwnBackupError::Cancelled
    ));
    assert!(
        observed < (data.len() * 2) as u64,
        "comparison finished after cancellation"
    );
    let mut previous = 0;
    let report = verify_tree_with_progress(&src, &dst, &CancelToken::default(), |p| {
        assert!(p.bytes_done >= previous);
        previous = p.bytes_done;
        Ok(())
    })
    .unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(report.bytes, data.len() as u64);
    assert!(previous >= (data.len() * 2) as u64);
}

#[test]
fn file_verification_compares_contents_names_and_alternate_streams() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("file.txt"), b"AAAAA").unwrap();
    std::fs::write(target.join("file.txt"), b"AAAAA").unwrap();
    let cancel = CancelToken::default();

    assert!(
        verify_tree(&source, &target, &cancel).is_ok(),
        "identical trees must pass"
    );

    std::fs::write(target.join("file.txt"), b"BBBBB").unwrap();
    assert!(
        verify_tree(&source, &target, &cancel).is_err(),
        "same size, different content"
    );
    std::fs::write(target.join("file.txt"), b"AAAAA").unwrap();

    std::fs::rename(target.join("file.txt"), target.join("renamed.txt")).unwrap();
    assert!(
        verify_tree(&source, &target, &cancel).is_err(),
        "same size, different name"
    );
    std::fs::rename(target.join("renamed.txt"), target.join("file.txt")).unwrap();

    std::fs::write(source.join("file.txt:audit"), b"secret one").unwrap();
    std::fs::write(target.join("file.txt:audit"), b"secret two").unwrap();
    assert!(
        verify_tree(&source, &target, &cancel).is_err(),
        "different ADS"
    );
    std::fs::write(target.join("file.txt:audit"), b"secret one").unwrap();
    assert!(
        verify_tree(&source, &target, &cancel).is_ok(),
        "unchanged tree after repair"
    );
}

#[test]
fn file_verification_detects_directory_acl_change() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::create_dir_all(target.join("nested")).unwrap();
    std::fs::write(source.join("nested/file.txt"), b"same contents").unwrap();
    std::fs::write(target.join("nested/file.txt"), b"same contents").unwrap();
    let cancel = CancelToken::default();
    assert!(verify_tree(&source, &target, &cancel).is_ok());
    let file_acl_before = std::process::Command::new("icacls")
        .arg(target.join("nested/file.txt"))
        .output()
        .unwrap();
    assert!(file_acl_before.status.success());

    let changed = std::process::Command::new("icacls")
        .arg(target.join("nested"))
        .arg("/grant")
        .arg("*S-1-5-32-545:F")
        .output()
        .unwrap();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let file_acl_after = std::process::Command::new("icacls")
        .arg(target.join("nested/file.txt"))
        .output()
        .unwrap();
    assert_eq!(
        file_acl_before.stdout, file_acl_after.stdout,
        "child file ACL changed"
    );
    assert!(
        verify_tree(&source, &target, &cancel).is_err(),
        "directory ACL changed"
    );
}

#[test]
fn file_restore_round_trip_preserves_ntfs_streams() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("nested/file.txt"), b"restored contents").unwrap();
    std::fs::write(source.join("nested/file.txt:note"), b"alternate stream").unwrap();
    let cancel = CancelToken::default();
    let total = measure_tree(&source, &cancel).unwrap();
    let result = copy_tree(&source, &target, total, &cancel, |_| Ok(())).unwrap();
    assert!(
        !result.cancelled && result.failures.is_empty(),
        "{result:?}"
    );
    assert!(
        verify_tree(&source, &target, &cancel).is_ok(),
        "restored tree must verify"
    );
    std::fs::write(target.join("nested/file.txt:note"), b"corrupted stream").unwrap();
    assert!(verify_tree(&source, &target, &cancel).is_err());
}

#[test]
fn file_verification_rejects_an_unreadable_nested_directory() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::create_dir_all(target.join("nested")).unwrap();
    std::fs::write(source.join("nested/file.txt"), b"same").unwrap();
    std::fs::write(target.join("nested/file.txt"), b"same").unwrap();
    let cancel = CancelToken::default();
    assert!(verify_tree(&source, &target, &cancel).is_ok());

    // FILE_FLAG_BACKUP_SEMANTICS lets OpenOptions open a directory. Denying
    // sharing prevents the verifier from reading its metadata or children.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(0x0200_0000)
        .open(target.join("nested"))
        .unwrap();
    assert!(verify_tree(&source, &target, &cancel).is_err());
    drop(held);
    assert!(verify_tree(&source, &target, &cancel).is_ok());
}

#[test]
#[ignore = "requires SeCreateSymbolicLinkPrivilege; run in the Windows lab as SYSTEM"]
fn file_verification_detects_reparse_target_change() {
    use std::os::windows::fs::symlink_file;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    for root in [&source, &target] {
        std::fs::write(root.join("a.txt"), b"same").unwrap();
        std::fs::write(root.join("b.txt"), b"same").unwrap();
        symlink_file("a.txt", root.join("link")).unwrap();
    }
    let cancel = CancelToken::default();
    assert!(verify_tree(&source, &target, &cancel).is_ok());
    std::fs::remove_file(target.join("link")).unwrap();
    symlink_file("b.txt", target.join("link")).unwrap();
    assert!(
        verify_tree(&source, &target, &cancel).is_err(),
        "reparse target changed"
    );
}
