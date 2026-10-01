use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const AUDIT_FILE_NAME: &str = "audit.jsonl";
pub(crate) const AUDIT_ROTATED_FILE_NAME: &str = "audit.1.jsonl";
pub(crate) const AUDIT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AuditEventKind {
    Register,
    Approve,
    Deny,
    Start,
    End,
    Kill,
    Timeout,
    Lost,
    #[allow(dead_code)]
    AutoApprove,
    ServeStart,
}

impl AuditEventKind {
    fn is_approval(self) -> bool {
        matches!(
            self,
            Self::Register | Self::Approve | Self::Deny | Self::AutoApprove
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuditEvent {
    pub(crate) timestamp: u64,
    pub(crate) event: AuditEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) def_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requester_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) approver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auto_approve: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auth: Option<String>,
}

impl AuditEvent {
    pub(crate) fn new(event: AuditEventKind) -> Self {
        Self::at(event, audit_timestamp(SystemTime::now()))
    }

    pub(crate) fn at(event: AuditEventKind, timestamp: u64) -> Self {
        Self {
            timestamp,
            event,
            job_id: None,
            target: None,
            def_hash: None,
            requester_ip: None,
            args: None,
            exit_code: None,
            duration_ms: None,
            approver: None,
            auto_approve: None,
            auth: None,
        }
    }

    pub(crate) fn target(mut self, name: &str, hash: &str) -> Self {
        self.target = Some(name.to_owned());
        self.def_hash = Some(hash.to_owned());
        self
    }

    pub(crate) fn requester(mut self, requester_ip: Option<String>) -> Self {
        self.requester_ip = requester_ip;
        self
    }
}

pub(crate) fn serve_start_event(timestamp: u64, auto_approve: bool, has_token: bool) -> AuditEvent {
    let mut event = AuditEvent::at(AuditEventKind::ServeStart, timestamp);
    event.auto_approve = Some(auto_approve);
    event.auth = Some(if has_token { "token" } else { "none" }.to_owned());
    event
}

pub(crate) fn audit_timestamp(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub(crate) fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn format_jsonl(event: &AuditEvent) -> serde_json::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    Ok(line)
}

pub(crate) fn should_rotate(current_len: u64, next_line_len: usize) -> bool {
    current_len.saturating_add(next_line_len as u64) > AUDIT_ROTATE_BYTES
}

pub(crate) trait ApprovalAuditWarning: Send + Sync {
    fn warn(&self, message: &str);
}

struct DefaultApprovalAuditWarning;

impl ApprovalAuditWarning for DefaultApprovalAuditWarning {
    fn warn(&self, message: &str) {
        // The manual-approval UI can replace this boundary with a Windows toast.
        tracing::warn!("承認監査ログ警告: {message}");
    }
}

#[derive(Clone)]
pub(crate) struct AuditLog {
    root: PathBuf,
    serial: Arc<Mutex<()>>,
    warning: Arc<dyn ApprovalAuditWarning>,
}

impl AuditLog {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self::with_warning(root, Arc::new(DefaultApprovalAuditWarning))
    }

    pub(crate) fn with_warning(root: PathBuf, warning: Arc<dyn ApprovalAuditWarning>) -> Self {
        Self {
            root,
            serial: Arc::new(Mutex::new(())),
            warning,
        }
    }

    pub(crate) fn record(&self, event: AuditEvent) {
        let approval_event = event.event.is_approval();
        if let Err(error) = self.append(&event) {
            tracing::error!(event = ?event.event, "監査ログの書き込みに失敗しました: {error}");
            if approval_event {
                self.warning.warn(&format!(
                    "監査ログに {:?} を記録できませんでした: {error}",
                    event.event
                ));
            }
        }
    }

    fn append(&self, event: &AuditEvent) -> io::Result<()> {
        let line = format_jsonl(event).map_err(io::Error::other)?;
        let _guard = self
            .serial
            .lock()
            .map_err(|_| io::Error::other("audit lock poisoned"))?;
        append_jsonl(&self.root, &line)
    }
}

fn append_jsonl(root: &Path, line: &[u8]) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join(AUDIT_FILE_NAME);
    let current_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if should_rotate(current_len, line.len()) {
        let rotated = root.join(AUDIT_ROTATED_FILE_NAME);
        match fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if path.exists() {
            fs::rename(&path, rotated)?;
        }
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line)
}

pub(crate) fn tail(path: &Path, count: usize) -> io::Result<String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let lines: Vec<_> = contents.lines().collect();
    let mut result = lines[lines.len().saturating_sub(count)..].join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}
