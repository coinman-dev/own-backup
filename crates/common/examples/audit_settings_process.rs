//! Exercise settings edits from separate Windows processes on a disposable path.
use own_backup_common::config::AppConfig;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "worker") {
        let path = std::path::Path::new(args.get(2).expect("config path"));
        let field = args.get(3).expect("field");
        for _ in 0..40 {
            AppConfig::update_file(path, |config| match field.as_str() {
                "language" => config.ui_language = "en".into(),
                "log" => config.log_level = "debug".into(),
                "images" => config.remembered_backups = vec![r"C:\Lab\audit.vhdx".into()],
                _ => panic!("unknown field"),
            })
            .expect("update settings");
        }
        return;
    }

    let dir = std::env::temp_dir().join(format!("own-backup-a13-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("config.toml");
    AppConfig::default().save_to_file(&path).unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for field in ["language", "log", "images"] {
        children.push(
            std::process::Command::new(&exe)
                .arg("worker")
                .arg(&path)
                .arg(field)
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let after = AppConfig::load_from_file(&path).unwrap();
    assert_eq!(after.ui_language, "en");
    assert_eq!(after.log_level, "debug");
    assert_eq!(after.remembered_backups, vec![r"C:\Lab\audit.vhdx"]);
    println!("independent_changes_preserved=true processes=3 updates_each=40");

    let original = std::fs::read(&path).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();
    let write_failed = AppConfig::update_file(&path, |c| c.log_level = "trace".into()).is_err();
    let unchanged = std::fs::read(&path).unwrap() == original;
    println!("readonly_write_failed={write_failed} original_unchanged={unchanged}");
    assert!(write_failed && unchanged);
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();

    std::fs::write(&path, b"broken TOML = =").unwrap();
    assert!(AppConfig::update_file(&path, |c| c.ui_language = "ru".into()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken TOML = =");
    println!("corrupt_toml_not_overwritten=true");
    std::fs::remove_dir_all(&dir).unwrap();
}
