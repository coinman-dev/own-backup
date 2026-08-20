use crate::types::BackupStatus;

#[derive(Debug, Clone, Default)]
pub struct BackupProgress {
    pub backup_id: String,
    pub status: BackupStatus,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
    pub speed_bps: u64,
    /// Разбивка ОСТАВШИХСЯ файлов по типам, если она уже известна.
    ///
    /// Сканер видит каждый файл с его размером ещё ДО копирования, то есть
    /// сводка готова через минуту после старта — а показывалась она только в
    /// конце, когда копия уже снята. Здесь она едет в интерфейс сразу после
    /// скана; в остальных сообщениях о ходе поле пустое, чтобы не гонять одно
    /// и то же по десять раз в секунду.
    ///
    /// Пары «имя категории → байты», а не структура: `common` не знает про
    /// `backup::exclusions::CategorySizes` и знать не должен.
    pub categories: Option<Vec<(String, u64)>>,
}

impl BackupProgress {
    pub fn percent(&self) -> f64 {
        if self.blocks_total == 0 {
            return 0.0;
        }
        (self.blocks_done as f64 / self.blocks_total as f64) * 100.0
    }

    pub fn eta_seconds(&self) -> Option<u64> {
        if self.speed_bps == 0 || self.blocks_done == 0 {
            return None;
        }
        let remaining_blocks = self.blocks_total.saturating_sub(self.blocks_done);
        let avg_bytes_per_block = self.bytes_written / self.blocks_done;
        let remaining_bytes = remaining_blocks * avg_bytes_per_block;
        Some(remaining_bytes / self.speed_bps)
    }
}

#[derive(Debug, Clone)]
pub struct RestoreProgress {
    pub restore_id: String,
    pub status: BackupStatus,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
    pub speed_bps: u64,
}
