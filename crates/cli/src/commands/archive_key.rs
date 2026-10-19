//! Turning archive encryption on for a job, from the command line (§7.5).
//!
//! Two steps, and the order is the point: the recovery key is shown once and
//! never stored, so the service commits the authorization only after it has
//! been typed back. Everything before that is in the service's memory and
//! disappears if this command is interrupted.
//!
//! The password is asked for twice because it cannot be read back either — a
//! typo in it, confirmed by nobody, is an archive that only the recovery key
//! can ever open.

use clap::Args;

use own_backup_client::BackupClient;
use own_backup_common::{OwnBackupError, Result};

use crate::secret_prompt::{ask_secret, ask_secret_twice};

/// Ask for whatever an image needs to be opened, and nothing when it needs
/// nothing.
///
/// The question is asked BEFORE the operation starts, which is why the service
/// has [`BackupClient::inspect_image`]: the alternative is to start, fail, and
/// then ask — and a long restore that gets that far has already taken the
/// target apart.
///
/// The service refuses a manual operation on an encrypted copy that arrives
/// without a secret, so a caller that skips this is refused, not served.
pub async fn ask_for_image(
    client: &mut BackupClient,
    image_path: &str,
) -> Result<own_backup_client::ArchiveSecret> {
    use own_backup_client::ArchiveSecret;

    let info = client
        .inspect_image(image_path)
        .await
        .map_err(OwnBackupError::Config)?;
    if !info.encrypted {
        return Ok(ArchiveSecret::none());
    }
    println!("Копия зашифрована (задание {}).", info.job_id);
    println!("  Введите пароль архива, либо оставьте пустым и введите ключ восстановления.");
    let password = ask_secret("  Пароль (не отображается): ")
        .map_err(|e| OwnBackupError::Config(e.to_string()))?;
    if !password.is_empty() {
        return Ok(ArchiveSecret::password(password));
    }
    let recovery =
        ask_secret("  Ключ восстановления: ").map_err(|e| OwnBackupError::Config(e.to_string()))?;
    if recovery.trim().is_empty() {
        return Err(OwnBackupError::Config(
            "для зашифрованной копии нужен пароль или ключ восстановления".into(),
        ));
    }
    Ok(ArchiveSecret::recovery_key(recovery))
}

/// The same question, for a command that runs the engine in this process
/// rather than asking the service.
///
/// `Ok(None)` means the image is not encrypted. Opening the envelope is
/// Argon2id at the production profile — seconds — so this is deliberately a
/// blocking call made before any work starts.
pub fn open_keys_for_image(
    image_path: &std::path::Path,
) -> Result<Option<own_backup_backup::seal::context::ArchiveKeys>> {
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::obk::OBK_EXTENSION;
    use own_backup_backup::seal::recovery::RecoveryKey;

    let container = own_backup_backup::sidecar::detect_container_format(image_path)?;
    if container != OBK_EXTENSION {
        return Ok(None);
    }
    println!("Копия зашифрована.");
    println!("  Введите пароль архива, либо оставьте пустым и введите ключ восстановления.");
    let password = ask_secret("  Пароль (не отображается): ")
        .map_err(|e| OwnBackupError::Config(e.to_string()))?;
    if !password.is_empty() {
        println!("  Открываю ключ (это несколько секунд)…");
        return ArchiveKeys::from_image_password(image_path, &password).map(Some);
    }
    let typed =
        ask_secret("  Ключ восстановления: ").map_err(|e| OwnBackupError::Config(e.to_string()))?;
    if typed.trim().is_empty() {
        return Err(OwnBackupError::Config(
            "для зашифрованной копии нужен пароль или ключ восстановления".into(),
        ));
    }
    let recovery = RecoveryKey::parse(&typed)?;
    println!("  Открываю ключ (это несколько секунд)…");
    ArchiveKeys::from_image_recovery(image_path, &recovery).map(Some)
}

#[derive(Args)]
pub struct ArchiveKeyArgs {
    /// Job whose copies should be encrypted from now on.
    #[arg(long)]
    pub job: String,

    /// Service endpoint (foreground/dev TCP; the installed service is reached
    /// over the named pipe automatically).
    #[arg(long, default_value = "http://127.0.0.1:50051")]
    pub endpoint: String,

    /// Bearer token (defaults to the service's token file).
    #[arg(long, default_value = "")]
    pub token: String,
}

pub async fn run(args: ArchiveKeyArgs) -> Result<()> {
    let token = crate::commands::mount::resolve_token(&args.token);
    let mut client = BackupClient::connect(args.endpoint.clone(), token)
        .await
        .map_err(OwnBackupError::Config)?;

    let job = client
        .get_job(&args.job)
        .await
        .map_err(OwnBackupError::Config)?
        .ok_or_else(|| OwnBackupError::Config(format!("задание {} не найдено", args.job)))?;
    if job.archive_encryption.is_some() {
        return Err(OwnBackupError::Config(format!(
            "у задания «{}» уже есть ключ архива",
            job.name
        )));
    }

    println!("Задание: {} ({})", job.name, job.id);
    println!(
        "После включения копии этого задания пишутся в наш контейнер .obk. \
         Прочитать их можно будет только этим паролем или ключом восстановления —\n\
         ни служба, ни мы не сможем открыть их иначе."
    );
    let password = ask_secret_twice("  Пароль архива (не отображается): ", "  Ещё раз: ")
        .map_err(|e| OwnBackupError::Config(e.to_string()))?;

    println!("  Готовлю ключ (это несколько секунд)…");
    let begun = client
        .begin_archive_encryption(&job, &password)
        .await
        .map_err(OwnBackupError::Config)?;

    println!();
    println!("  Ключ восстановления — запишите его сейчас:");
    println!();
    println!("      {}", begun.recovery_key);
    println!();
    println!("  Он показывается ОДИН раз и нигде не сохраняется.");
    println!("  Отпечаток ключа задания: {}", begun.fingerprint);
    match begun.key_protection.as_str() {
        "tpm2" => println!("  На этой машине ключ задания защищён TPM2 (systemd-creds)."),
        "host" => println!(
            "  На этой машине ключ задания защищён ключом хоста (systemd-creds): TPM2 нет."
        ),
        "dpapi" => println!("  На этой машине ключ задания защищён DPAPI."),
        _ => {}
    }
    println!();

    let echo = ask_secret("  Введите ключ восстановления для подтверждения: ")
        .map_err(|e| OwnBackupError::Config(e.to_string()))?;
    if echo.trim().is_empty() {
        // Nothing was typed: cancel rather than spend the service's attempts.
        let _ = client.cancel_archive_encryption(&begun.token).await;
        return Err(OwnBackupError::Config(
            "ключ восстановления не введён — шифрование не включено".into(),
        ));
    }

    match client.confirm_archive_encryption(&begun.token, &echo).await {
        Ok(saved) => {
            println!();
            println!("Шифрование включено для «{}».", saved.name);
            println!("Формат образов: .obk, следующая копия будет зашифрована.");
            Ok(())
        }
        Err(e) => Err(OwnBackupError::Config(e)),
    }
}
