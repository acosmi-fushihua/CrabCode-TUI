use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::status::{build_status, BoxError, MemoryOrchestratorStatus, StatusRequest};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerScope {
    Session,
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    pub status_request: StatusRequest,
    pub interval: Duration,
    pub run_startup_scan: bool,
    pub scope: SchedulerScope,
}

impl SchedulerConfig {
    #[must_use]
    pub fn session(status_request: StatusRequest, interval: Duration) -> Self {
        Self {
            status_request,
            interval,
            run_startup_scan: true,
            scope: SchedulerScope::Session,
        }
    }

    #[must_use]
    pub fn without_startup_scan(mut self) -> Self {
        self.run_startup_scan = false;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerRunKind {
    Startup,
    Periodic,
}

#[derive(Clone, Debug)]
pub struct SchedulerRunReport {
    pub scope: SchedulerScope,
    pub kind: SchedulerRunKind,
    pub status: MemoryOrchestratorStatus,
    pub emitted_llm_trigger_methods: Vec<String>,
    pub pending_triggers_written: bool,
}

pub fn run_startup_scan(config: &SchedulerConfig) -> Result<SchedulerRunReport, BoxError> {
    run_deterministic_analysis(config, SchedulerRunKind::Startup)
}

pub fn run_periodic_scan(config: &SchedulerConfig) -> Result<SchedulerRunReport, BoxError> {
    run_deterministic_analysis(config, SchedulerRunKind::Periodic)
}

pub fn spawn_session_scheduler(
    config: SchedulerConfig,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<Result<u64, BoxError>> {
    tokio::spawn(async move {
        let mut completed_runs = 0;
        if config.run_startup_scan {
            run_startup_scan(&config)?;
            completed_runs += 1;
        }

        let mut interval = tokio::time::interval(config.interval);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    run_periodic_scan(&config)?;
                    completed_runs += 1;
                }
            }
        }

        Ok(completed_runs)
    })
}

fn run_deterministic_analysis(
    config: &SchedulerConfig,
    kind: SchedulerRunKind,
) -> Result<SchedulerRunReport, BoxError> {
    let status = build_status(&config.status_request)?;

    Ok(SchedulerRunReport {
        scope: config.scope.clone(),
        kind,
        status,
        emitted_llm_trigger_methods: Vec::new(),
        pending_triggers_written: false,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::UNIX_EPOCH;

    use tempfile::TempDir;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn memory_doc(description: &str, body: &str) -> String {
        format!("---\ntype: project\ndescription: {description}\n---\n{body}")
    }

    fn request(dir: &TempDir) -> StatusRequest {
        StatusRequest::new(
            dir.path().join("memory"),
            dir.path().join("workspace"),
            dir.path(),
            dir.path().join("transcripts"),
        )
        .with_now(UNIX_EPOCH + Duration::from_secs(2_000_000_000))
    }

    #[test]
    fn scheduler_startup_scan_runs_deterministic_status_only() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir.path().join("memory/a.md"),
            &memory_doc("a", "same body\n"),
        );
        write_file(
            &dir.path().join("memory/b.md"),
            &memory_doc("b", "same body\n"),
        );
        let config = SchedulerConfig::session(request(&dir), Duration::from_secs(60));

        let report = run_startup_scan(&config).unwrap();

        assert_eq!(report.scope, SchedulerScope::Session);
        assert_eq!(report.kind, SchedulerRunKind::Startup);
        assert_eq!(report.status.dedup.duplicate_group_count, 1);
        assert!(report.emitted_llm_trigger_methods.is_empty());
        assert!(!report.pending_triggers_written);
        assert!(!dir
            .path()
            .join(".memory-rust-derived/pending-triggers")
            .exists());
    }

    #[tokio::test]
    async fn scheduler_loop_stops_with_session_shutdown() {
        let dir = TempDir::new().unwrap();
        let config = SchedulerConfig::session(request(&dir), Duration::from_millis(10));
        let (tx, rx) = watch::channel(false);
        let handle = spawn_session_scheduler(config, rx);

        tokio::time::sleep(Duration::from_millis(25)).await;
        tx.send(true).unwrap();
        let completed_runs = handle.await.unwrap().unwrap();

        assert!(completed_runs >= 1);
        assert!(!dir
            .path()
            .join(".memory-rust-derived/pending-triggers")
            .exists());
    }

    #[test]
    fn scheduler_periodic_scan_has_no_trigger_side_effects() {
        let dir = TempDir::new().unwrap();
        let config = SchedulerConfig::session(request(&dir), Duration::from_secs(60));

        let report = run_periodic_scan(&config).unwrap();

        assert!(report.emitted_llm_trigger_methods.is_empty());
        assert!(!report.pending_triggers_written);
        assert!(!dir
            .path()
            .join(".memory-rust-derived/pending-triggers")
            .exists());
    }
}
