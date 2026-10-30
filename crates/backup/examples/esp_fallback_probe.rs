//! Runs the restore's fallback-loader step on a mounted ESP, for the stand:
//! `esp_fallback_probe <esp mount point> [filesystem uuid of /boot]`.

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("the ESP's mount point");
    let uuids: Vec<String> = args.collect();
    let out =
        own_backup_backup::esp_fallback_tree::ensure_at_root(root.as_ref(), &uuids, "probe")
            .expect("the step failed");
    println!(
        "copied={} replaced={} from={:?}\n{}",
        out.copied, out.replaced, out.from, out.message
    );
}
