use crate::api::{Drive, Version};
use crate::cleaner::{ExecSummary, Plan};

/// Messages envoyés par les tâches de fond à la boucle de l'interface.
#[derive(Debug)]
pub enum AppEvent {
    Drives(Result<Vec<Drive>, String>),
    ScanProgress {
        files: usize,
        dirs_done: usize,
        dirs_total: usize,
        current: String,
    },
    ScanDone(Result<(), String>),
    Versions {
        file_id: i64,
        result: Result<Vec<Version>, String>,
    },
    PrepareProgress {
        done: usize,
        total: usize,
    },
    PrepareDone(Result<Plan, String>),
    ExecProgress {
        done: usize,
        total: usize,
        freed: i64,
        failed: usize,
    },
    ExecDone(ExecSummary),
}
