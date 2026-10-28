//! Что из сторонних средств эта машина может положить на носитель.
//!
//! Тот же поиск, который мастер сборки показывает галочками, — здесь он
//! доступен отдельно, чтобы ответ можно было получить без интерфейса и без
//! сборки образа. Ничего не копируется и не скачивается: команда только
//! смотрит, что на машине установлено. В Linux искать нечего: носитель —
//! Debian, и средства ставятся при сборке из его пакетов; команда показывает
//! их список.

use clap::Args;
use own_backup_common::Result;

#[derive(Args)]
pub struct MediaToolsArgs {
    /// Вывести JSON вместо таблицы.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: MediaToolsArgs) -> Result<()> {
    // Linux: nothing is looked for on this machine. The medium is Debian, and
    // these are Debian's packages, installed by the build itself.
    #[cfg(not(target_os = "windows"))]
    {
        use own_backup_common::media_tools::LINUX_MEDIA_TOOLS;
        if args.json {
            let rows: Vec<_> = LINUX_MEDIA_TOOLS
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "name": t.name,
                        "packages": t.packages,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).unwrap_or_default()
            );
            return Ok(());
        }
        println!("Средства, которые можно положить на носитель Linux");
        println!();
        println!("  {:<12}  {:<12}  Пакеты Debian", "Код", "Название");
        println!("  {:-<12}  {:-<12}  {:-<40}", "", "", "");
        for t in LINUX_MEDIA_TOOLS {
            println!("  {:<12}  {:<12}  {}", t.id, t.name, t.packages.join(" "));
        }
        println!();
        println!("  Ставятся из архива Debian при сборке носителя:");
        println!("  own-backup-cli media build-linux --out DIR --tools krusader,featherpad");
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        let found = own_backup_backup::media_tools_win::find_tools();

        if args.json {
            let rows: Vec<_> = found
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "name": t.name,
                        "source": t.source.display().to_string(),
                        "size_bytes": t.size_bytes,
                        "files": t.files,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).unwrap_or_default()
            );
            return Ok(());
        }

        if found.is_empty() {
            println!("На этой машине ничего из списка не найдено.");
            println!("Носитель от этого не пострадает — это обычный случай.");
            println!();
        }
        println!("Средства, которые можно положить на носитель");
        println!();
        println!(
            "  {:<18}  {:>9}  {:>7}  {}",
            "Название", "Размер", "Файлов", "Откуда"
        );
        println!("  {:-<18}  {:->9}  {:->7}  {:-<40}", "", "", "", "");
        for tool in &found {
            println!(
                "  {:<18}  {:>9}  {:>7}  {}",
                tool.name,
                own_backup_common::units::human_bytes(tool.size_bytes),
                tool.files,
                tool.source.display()
            );
        }
        if !found.is_empty() {
            let total: u64 = found.iter().map(|t| t.size_bytes).sum();
            println!();
            println!("  Всего: {}", own_backup_common::units::human_bytes(total));
            println!();
            println!("  Копия делается с этой машины, из вашей же установки, и");
            println!("  попадает только на ваш носитель. Ничего не скачивается.");
        }
        Ok(())
    }
}
