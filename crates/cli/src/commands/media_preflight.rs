//! Проверить окружение до запуска интерфейса и дождаться, что он поднялся.
//!
//! Две команды одного сюжета, обе зовутся из `start-media.cmd`:
//!
//! * `media-preflight` — до запуска чего-либо. Если WebView2 нет или на `X:`
//!   нет места, интерфейс поднимать бессмысленно: код возврата 1, и носитель
//!   остаётся с рабочей консолью вместо пустого экрана.
//! * `media-wait` — после запуска. Ждёт маркер, который GUI пишет ПОСЛЕ того,
//!   как окно построено. Ждать живого процесса нельзя: с этого всё и началось —
//!   процесс жил, окна не было, а консоль была занята им же.
//!
//! Обе печатают шпаргалку по CLI, когда интерфейса не будет: этим набором
//! команд копия снимается и возвращается целиком, так что «нет интерфейса» —
//! это неудобство, а не конец спасения.

use clap::Args;
use own_backup_backup::media_preflight_win as pf;
use own_backup_common::Result;

#[derive(Args)]
pub struct MediaPreflightArgs {
    /// Только напечатать вердикт; код возврата всегда 0.
    #[arg(long)]
    pub report_only: bool,
}

#[derive(Args)]
pub struct MediaWaitArgs {
    /// Сколько секунд ждать окна.
    #[arg(long, default_value_t = 20)]
    pub timeout: u64,
}

/// Каталог, откуда запущена программа (рядом с ним лежит папка `WebView2`).
fn program_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn print_checks(checks: &[pf::Check]) {
    for c in checks {
        let mark = match c.level {
            pf::Level::Ok => "[+]",
            pf::Level::Warn => "[~]",
            pf::Level::Fail => "[!]",
        };
        println!("{mark} {}", c.message);
    }
}

pub async fn run(args: MediaPreflightArgs) -> Result<()> {
    let checks = pf::run_preflight(&program_dir());
    print_checks(&checks);
    let verdict = pf::worst(&checks);
    for c in &checks {
        tracing::info!(check = c.name, level = ?c.level, "media preflight");
    }
    if verdict == pf::Level::Fail && !args.report_only {
        println!();
        println!("Интерфейс не запускаю — сначала устраните причину выше.");
        println!("{}", pf::cli_cheat_sheet());
        // Не Err: это не сбой программы, а вердикт. Ошибка напечатала бы
        // «Error: …» поверх уже сказанного и ничего не добавила.
        std::process::exit(1);
    }
    Ok(())
}

pub async fn run_wait(args: MediaWaitArgs) -> Result<()> {
    let marker = pf::gui_up_marker();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout);
    while std::time::Instant::now() < deadline {
        if marker.exists() {
            tracing::info!(marker = %marker.display(), "gui window is up");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    println!();
    println!("[!] Интерфейс не поднялся за {} с — окна нет.", args.timeout);
    println!("    Логи:");
    println!("      {}", program_dir().join("own-backup-gui.log").display());
    println!("      {}", std::env::temp_dir().join("webview2.log").display());
    println!();
    println!("{}", pf::cli_cheat_sheet());
    tracing::warn!(timeout = args.timeout, "gui did not come up");
    std::process::exit(1);
}
