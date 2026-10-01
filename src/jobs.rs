//! Durable job metadata and the side-effect-free job state machine.

use crate::config::Concurrency;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const RETAIN: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobStatus {
    Running,
    Orphaned,
    Succeeded,
    Failed,
    Timeout,
    Killed,
    Lost,
}

impl JobStatus {
    fn active(self) -> bool {
        matches!(self, Self::Running | Self::Orphaned)
    }

    fn failure(self) -> bool {
        matches!(self, Self::Failed | Self::Timeout)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Succeed,
    Fail,
    Timeout,
    Kill,
    RecoverAlive,
    ChildGone,
}

fn transition(state: JobStatus, event: Event) -> Option<JobStatus> {
    use Event::*;
    use JobStatus::*;
    match (state, event) {
        (Running, Succeed) => Some(Succeeded),
        (Running, Fail) => Some(Failed),
        (Running, Event::Timeout) => Some(JobStatus::Timeout),
        (Running | Orphaned, Kill) => Some(Killed),
        (Running, RecoverAlive) => Some(Orphaned),
        (Running | Orphaned, ChildGone) => Some(Lost),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChildIdentity {
    pub(crate) pid: u32,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct JobMeta {
    pub(crate) id: String,
    pub(crate) target: String,
    pub(crate) def_hash: String,
    pub(crate) started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<u64>,
    pub(crate) state: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requester: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) child: Option<ChildIdentity>,
    #[serde(default)]
    pub(crate) detached: bool,
}

#[derive(Clone)]
pub(crate) struct JobRegistry {
    root: PathBuf,
    jobs: Arc<Mutex<HashMap<String, JobMeta>>>,
    controls: Arc<Mutex<HashMap<String, Arc<JobControl>>>>,
    lost_events: Arc<Mutex<Vec<JobMeta>>>,
}

#[derive(Debug, Default)]
pub(crate) struct JobControl {
    cancelled: Arc<AtomicBool>,
    killed: AtomicBool,
}

impl JobControl {
    pub(crate) fn cancelled(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub(crate) fn was_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum KillDecision {
    Accepted,
    Missing,
    Finished,
}

fn decide_kill(status: Option<JobStatus>, has_control: bool) -> KillDecision {
    match status {
        None => KillDecision::Missing,
        Some(status) if !status.active() || !has_control => KillDecision::Finished,
        Some(_) => KillDecision::Accepted,
    }
}

#[derive(Debug)]
pub(crate) struct Conflict(pub(crate) String);

impl JobRegistry {
    pub(crate) fn new(config_dir: impl Into<PathBuf>) -> Result<Self> {
        let root = config_dir.into().join("jobs");
        fs::create_dir_all(&root)?;
        let mut jobs = HashMap::new();
        let mut lost_events = Vec::new();
        for entry in fs::read_dir(&root)? {
            let path = entry?.path().join("meta.json");
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(mut meta) = serde_json::from_slice::<JobMeta>(&bytes) else {
                continue;
            };
            if meta.state == JobStatus::Running {
                let event = if meta.child.as_ref().is_some_and(process_alive) {
                    Event::RecoverAlive
                } else {
                    Event::ChildGone
                };
                meta.state = transition(meta.state, event).expect("recovery transition");
                if meta.state == JobStatus::Lost {
                    meta.ended_at = Some(now_millis());
                    lost_events.push(meta.clone());
                }
                write_meta(&root, &meta)?;
            }
            jobs.insert(meta.id.clone(), meta);
        }
        let registry = Self {
            root,
            jobs: Arc::new(Mutex::new(jobs)),
            controls: Arc::new(Mutex::new(HashMap::new())),
            lost_events: Arc::new(Mutex::new(lost_events)),
        };
        registry.prune();
        Ok(registry)
    }

    pub(crate) fn start(
        &self,
        target: &str,
        def_hash: &str,
        requester: Option<String>,
        concurrency: Concurrency,
    ) -> std::result::Result<String, Conflict> {
        let mut jobs = self.jobs.lock().unwrap();
        if concurrency == Concurrency::Reject {
            let active: Vec<String> = jobs
                .values()
                .filter(|job| job.target == target && job.state.active())
                .map(|job| job.id.clone())
                .collect();
            for id in active {
                let dead = jobs.get(&id).is_some_and(|job| {
                    job.state == JobStatus::Orphaned
                        && !job.child.as_ref().is_some_and(process_alive)
                });
                if dead {
                    let job = jobs.get_mut(&id).unwrap();
                    job.state = transition(job.state, Event::ChildGone).unwrap();
                    job.ended_at = Some(now_millis());
                    if let Err(error) = write_meta(&self.root, job) {
                        tracing::error!("lost ジョブの保存に失敗しました: {error:#}");
                    }
                    self.lost_events.lock().unwrap().push(job.clone());
                } else {
                    return Err(Conflict(id));
                }
            }
        }
        let id = new_ulid().map_err(|error| Conflict(format!("job id error: {error}")))?;
        let meta = JobMeta {
            id: id.clone(),
            target: target.into(),
            def_hash: def_hash.into(),
            started_at: now_millis(),
            ended_at: None,
            state: JobStatus::Running,
            exit_code: None,
            requester,
            child: None,
            detached: false,
        };
        write_meta(&self.root, &meta).map_err(|error| Conflict(error.to_string()))?;
        jobs.insert(id.clone(), meta);
        drop(jobs);
        self.prune();
        Ok(id)
    }

    pub(crate) fn set_child(&self, id: &str, child: Option<ChildIdentity>) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.child = child;
            if let Err(error) = write_meta(&self.root, job) {
                tracing::error!("ジョブ子プロセス情報の保存に失敗しました: {error:#}");
            }
        }
    }

    pub(crate) fn register_control(&self, id: &str, control: Arc<JobControl>) {
        self.controls
            .lock()
            .unwrap()
            .insert(id.to_string(), control);
    }

    pub(crate) fn request_kill(&self, id: &str) -> KillDecision {
        let jobs = self.jobs.lock().unwrap();
        let control = self.controls.lock().unwrap().get(id).cloned();
        let decision = decide_kill(jobs.get(id).map(|job| job.state), control.is_some());
        if decision != KillDecision::Accepted {
            return decision;
        }
        let control = control.expect("accepted kill has a control");
        control.killed.store(true, Ordering::Release);
        control.cancelled.store(true, Ordering::Release);
        KillDecision::Accepted
    }

    pub(crate) fn finish(&self, id: &str, status: JobStatus, exit_code: Option<i32>) {
        let event = match status {
            JobStatus::Succeeded => Event::Succeed,
            JobStatus::Timeout => Event::Timeout,
            JobStatus::Killed => Event::Kill,
            _ => Event::Fail,
        };
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            if let Some(next) = transition(job.state, event) {
                job.state = next;
                job.ended_at = Some(now_millis());
                job.exit_code = exit_code;
                job.child = None;
                if let Err(error) = write_meta(&self.root, job) {
                    tracing::error!("ジョブ完了情報の保存に失敗しました: {error:#}");
                }
            }
        }
        drop(jobs);
        self.controls.lock().unwrap().remove(id);
        self.prune();
    }

    pub(crate) fn get(&self, id: &str) -> Option<JobMeta> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn list(&self, state: Option<JobStatus>) -> Vec<JobMeta> {
        let mut jobs: Vec<_> = self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|job| state.is_none_or(|state| job.state == state))
            .cloned()
            .collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.started_at));
        jobs
    }

    pub(crate) fn take_lost_events(&self) -> Vec<JobMeta> {
        std::mem::take(&mut *self.lost_events.lock().unwrap())
    }

    pub(crate) fn set_detached(&self, id: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.detached = true;
            if let Err(error) = write_meta(&self.root, job) {
                tracing::error!("detach 情報の保存に失敗しました: {error:#}");
            }
        }
    }

    pub(crate) fn write_log(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        fs::write(self.root.join(id).join("log"), bytes)
    }

    pub(crate) fn read_log(&self, id: &str, offset: usize) -> io::Result<Option<Vec<u8>>> {
        if !self.jobs.lock().unwrap().contains_key(id) {
            return Ok(None);
        }
        let bytes = match fs::read(self.root.join(id).join("log")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Some(bytes.get(offset.min(bytes.len())..).unwrap().to_vec()))
    }

    fn prune(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        let completed = jobs.values().filter(|job| !job.state.active()).count();
        if completed <= RETAIN {
            return;
        }
        let mut candidates: Vec<_> = jobs
            .values()
            .filter(|job| !job.state.active())
            .cloned()
            .collect();
        candidates.sort_by_key(|job| (job.state.failure() || job.detached, job.started_at));
        for job in candidates.into_iter().take(completed - RETAIN) {
            match fs::remove_dir_all(self.root.join(&job.id)) {
                Ok(()) => {
                    jobs.remove(&job.id);
                }
                Err(error) => {
                    tracing::warn!(job_id = %job.id, "古いジョブを削除できませんでした。次回再試行します: {error}")
                }
            }
        }
    }
}

fn write_meta(root: &Path, meta: &JobMeta) -> Result<()> {
    let dir = root.join(&meta.id);
    fs::create_dir_all(&dir)?;
    let path = dir.join("meta.json");
    let temp = dir.join("meta.json.tmp");
    let mut bytes = serde_json::to_vec_pretty(meta)?;
    bytes.push(b'\n');
    fs::write(&temp, bytes)?;
    fs::rename(&temp, &path).with_context(|| format!("{} を更新できません", path.display()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_ulid() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&now_millis().to_be_bytes()[2..]);
    getrandom::fill(&mut bytes[6..]).map_err(|error| io::Error::other(error.to_string()))?;
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = u128::from_be_bytes(bytes);
    let mut out = [b'0'; 26];
    for (index, slot) in out.iter_mut().enumerate() {
        let shift = 125 - index * 5;
        *slot = ALPHABET[((value >> shift) & 31) as usize];
    }
    Ok(String::from_utf8(out.to_vec()).expect("ULID alphabet is ASCII"))
}

#[cfg(target_os = "linux")]
pub(crate) fn child_identity(pid: u32) -> io::Result<ChildIdentity> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid /proc stat"))?
        .1;
    let created_at = fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::other("missing starttime"))?
        .parse()
        .map_err(io::Error::other)?;
    Ok(ChildIdentity { pid, created_at })
}

#[cfg(all(not(target_os = "linux"), not(windows)))]
pub(crate) fn child_identity(pid: u32) -> io::Result<ChildIdentity> {
    Ok(ChildIdentity { pid, created_at: 0 })
}

#[cfg(windows)]
pub(crate) fn child_identity(pid: u32) -> io::Result<ChildIdentity> {
    Ok(ChildIdentity { pid, created_at: 0 })
}

#[cfg(target_os = "linux")]
fn process_alive(child: &ChildIdentity) -> bool {
    child_identity(child.pid).is_ok_and(|current| current.created_at == child.created_at)
}

#[cfg(all(not(target_os = "linux"), not(windows)))]
fn process_alive(child: &ChildIdentity) -> bool {
    unsafe { libc::kill(child.pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_alive(_child: &ChildIdentity) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::Command;
    use std::{thread, time::Duration};

    #[test]
    fn transitions_are_table_driven() {
        use Event::*;
        use JobStatus::*;
        let cases = [
            (Running, Succeed, Some(Succeeded)),
            (Running, Fail, Some(Failed)),
            (Running, Event::Timeout, Some(JobStatus::Timeout)),
            (Running, Kill, Some(Killed)),
            (Running, RecoverAlive, Some(Orphaned)),
            (Running, ChildGone, Some(Lost)),
            (Orphaned, ChildGone, Some(Lost)),
            (Succeeded, Fail, None),
        ];
        for (state, event, expected) in cases {
            assert_eq!(transition(state, event), expected);
        }
    }

    #[test]
    fn generated_ids_are_crockford_ulids() {
        let id = new_ulid().unwrap();
        assert_eq!(id.len(), 26);
        assert!(id
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ac_t6_1_3_recovery_and_orphan_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let registry = JobRegistry::new(dir.path()).unwrap();
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let live = registry
            .start("same", "hash", None, Concurrency::Reject)
            .unwrap();
        registry.set_child(&live, child_identity(child.id()).unwrap().into());
        drop(registry);

        let recovered = JobRegistry::new(dir.path()).unwrap();
        assert_eq!(recovered.get(&live).unwrap().state, JobStatus::Orphaned);
        assert_eq!(
            recovered
                .start("same", "hash", None, Concurrency::Reject)
                .unwrap_err()
                .0,
            live
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let next = recovered
            .start("same", "hash", None, Concurrency::Reject)
            .unwrap();
        assert_eq!(recovered.get(&live).unwrap().state, JobStatus::Lost);
        assert_ne!(next, live);

        let dead = recovered
            .start("dead", "hash", None, Concurrency::Reject)
            .unwrap();
        recovered.set_child(
            &dead,
            Some(ChildIdentity {
                pid: u32::MAX,
                created_at: 1,
            }),
        );
        drop(recovered);
        let recovered = JobRegistry::new(dir.path()).unwrap();
        assert_eq!(recovered.get(&dead).unwrap().state, JobStatus::Lost);
    }

    #[test]
    fn ac_t6_1_5_retains_running_and_failures_longer_than_successes() {
        let dir = tempfile::tempdir().unwrap();
        let registry = JobRegistry::new(dir.path()).unwrap();
        let running = registry
            .start("running", "hash", None, Concurrency::Allow)
            .unwrap();
        let failed = registry
            .start("failed", "hash", None, Concurrency::Allow)
            .unwrap();
        registry.finish(&failed, JobStatus::Failed, Some(1));
        thread::sleep(Duration::from_millis(2));
        let mut successes = Vec::new();
        for index in 0..51 {
            let id = registry
                .start(&format!("ok-{index}"), "hash", None, Concurrency::Allow)
                .unwrap();
            registry.finish(&id, JobStatus::Succeeded, Some(0));
            successes.push(id);
        }
        assert!(registry.get(&running).is_some());
        assert!(registry.get(&failed).is_some());
        assert!(registry.get(&successes[0]).is_none());
        assert_eq!(
            registry
                .jobs
                .lock()
                .unwrap()
                .values()
                .filter(|job| !job.state.active())
                .count(),
            RETAIN
        );
    }
}
