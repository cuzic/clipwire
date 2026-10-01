use super::*;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const APPROVAL_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
pub(crate) const APPROVAL_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApprovalRecordAge {
    file_name: String,
    modified: SystemTime,
}

#[derive(Clone)]
pub(crate) struct Store {
    root: PathBuf,
    serial: Arc<tokio::sync::Mutex<()>>,
}

pub(crate) struct RegisterResult {
    pub(crate) reapproval: bool,
    pub(crate) unchanged: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) redisplay_required: bool,
    pub(crate) hash: String,
}

#[derive(serde::Serialize)]
pub(crate) struct TargetCheckResult {
    pub(crate) status: &'static str,
    pub(crate) hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pending_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pending_matches: Option<bool>,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn register(
        &self,
        name: String,
        target: StoredTarget,
        auto_approve: bool,
    ) -> Result<RegisterResult> {
        let _serial = self.serial.lock().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&root, LOCK_TIMEOUT, || {
                let pending_path = root.join("pending.toml");
                let registered_path = root.join("registered.toml");
                let mut registered = load_for_update(&registered_path)?;
                let canonical = canonical_json(&target);
                let hash = definition_hash(&canonical);
                let mut pending = load_for_update(&pending_path)?;
                refresh_existing_approval_record(&root, &hash, &canonical, SystemTime::now())?;

                if registered
                    .get(&name)
                    .is_some_and(|entry| definition_hash(&canonical_json(entry)) == hash)
                {
                    let pending_changed = pending.remove(&name).is_some();
                    if pending_changed {
                        atomic_save_target_map(&pending_path, &pending)?;
                    }
                    return Ok(RegisterResult {
                        reapproval: false,
                        unchanged: true,
                        redisplay_required: false,
                        hash,
                    });
                }

                // A registered -> B pending -> A registered again: the name is no
                // longer in registered.toml, so use the immutable approval record
                // to recognize the previously approved definition.
                if auto_approve
                    && pending.contains_key(&name)
                    && approval_record_matches(&root, &hash, &canonical)?
                {
                    let approved = approve_target(&root, target, &canonical, &hash)?;
                    registered.insert(name.clone(), approved);
                    pending.remove(&name);
                    atomic_save_target_map(&registered_path, &registered)?;
                    atomic_save_target_map(&pending_path, &pending)?;
                    return Ok(RegisterResult {
                        reapproval: false,
                        unchanged: true,
                        redisplay_required: false,
                        hash,
                    });
                }

                if auto_approve {
                    let approved = approve_target(&root, target, &canonical, &hash)?;
                    registered.insert(name.clone(), approved);
                    let pending_changed = pending.remove(&name).is_some();
                    atomic_save_target_map(&registered_path, &registered)?;
                    if pending_changed {
                        atomic_save_target_map(&pending_path, &pending)?;
                    }
                    return Ok(RegisterResult {
                        reapproval: false,
                        unchanged: false,
                        redisplay_required: false,
                        hash,
                    });
                }
                let reapproval = registered.remove(&name).is_some();
                let pending_same = pending
                    .get(&name)
                    .is_some_and(|entry| definition_hash(&canonical_json(entry)) == hash);
                if pending_same {
                    return Ok(RegisterResult {
                        reapproval,
                        unchanged: true,
                        redisplay_required: false,
                        hash,
                    });
                }
                pending.insert(name, target);
                atomic_save_target_map(&pending_path, &pending)?;
                if reapproval {
                    atomic_save_target_map(&registered_path, &registered)?;
                }
                Ok(RegisterResult {
                    reapproval,
                    unchanged: false,
                    redisplay_required: true,
                    hash,
                })
            })
        })
        .await
        .context("store worker panicked")?
    }

    pub(crate) fn pending(&self) -> Result<TargetMap> {
        with_store_lock(&self.root, LOCK_TIMEOUT, || {
            load_for_update(&self.root.join("pending.toml"))
        })
    }

    pub(crate) fn approve(&self, name: &str, expected_hash: &str) -> Result<()> {
        with_store_lock(&self.root, LOCK_TIMEOUT, || {
            let pending_path = self.root.join("pending.toml");
            let registered_path = self.root.join("registered.toml");
            let mut pending = load_for_update(&pending_path)?;
            let entry = pending
                .get(name)
                .cloned()
                .with_context(|| format!("'{name}' は pending にありません"))?;
            let canonical = canonical_json(&entry);
            let hash = definition_hash(&canonical);
            ensure_hash_matches(expected_hash, &hash)?;
            let entry = approve_target(&self.root, entry, &canonical, &hash)?;
            let mut registered = load_for_update(&registered_path)?;
            registered.insert(name.to_owned(), entry);
            pending.remove(name);
            atomic_save_target_map(&registered_path, &registered)?;
            atomic_save_target_map(&pending_path, &pending)
        })
    }

    pub(crate) fn deny(&self, name: &str, expected_hash: Option<&str>) -> Result<()> {
        with_store_lock(&self.root, LOCK_TIMEOUT, || {
            let pending_path = self.root.join("pending.toml");
            let mut pending = load_for_update(&pending_path)?;
            let entry = pending
                .get(name)
                .with_context(|| format!("'{name}' は pending にありません"))?;
            if let Some(expected_hash) = expected_hash {
                let hash = definition_hash(&canonical_json(entry));
                ensure_hash_matches(expected_hash, &hash)?;
            }
            pending.remove(name);
            atomic_save_target_map(&pending_path, &pending)
        })
    }

    pub(crate) async fn migrate(&self) -> Result<()> {
        let _serial = self.serial.lock().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&root, LOCK_TIMEOUT, || migrate_locked(&root))
        })
        .await
        .context("store migration worker panicked")?
    }

    pub(crate) async fn cleanup_approval_records(
        &self,
        retention: Duration,
        now: SystemTime,
    ) -> Result<usize> {
        let _serial = self.serial.lock().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&root, LOCK_TIMEOUT, || {
                cleanup_approval_records_locked(&root, retention, now)
            })
        })
        .await
        .context("approval cleanup worker panicked")?
    }

    pub(crate) fn verified_target(&self, name: &str) -> Result<Option<StoredTarget>> {
        with_store_lock(&self.root, LOCK_TIMEOUT, || {
            let registered = load_target_map(&self.root.join("registered.toml"))?;
            let Some(target) = registered.get(name) else {
                return Ok(None);
            };
            verify_approval(&self.root, target)?;
            Ok(Some(target.clone()))
        })
    }

    pub(crate) async fn check_targets(
        &self,
        targets: std::collections::BTreeMap<String, StoredTarget>,
    ) -> Result<std::collections::BTreeMap<String, TargetCheckResult>> {
        let _serial = self.serial.lock().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&root, LOCK_TIMEOUT, || {
                let registered = load_for_update(&root.join("registered.toml"))?;
                let pending = load_for_update(&root.join("pending.toml"))?;
                let mut results = std::collections::BTreeMap::new();
                for (name, target) in targets {
                    let hash = definition_hash(&canonical_json(&target));
                    let pending_hash = pending
                        .get(&name)
                        .map(|entry| definition_hash(&canonical_json(entry)));
                    let registered_hash = registered
                        .get(&name)
                        .map(|entry| definition_hash(&canonical_json(entry)));
                    let status = if pending_hash.is_some() {
                        "pending"
                    } else if let Some(registered_hash) = &registered_hash {
                        if registered_hash == &hash {
                            "ok"
                        } else {
                            "changed"
                        }
                    } else {
                        "unregistered"
                    };
                    let pending_matches = pending_hash.as_ref().map(|value| value == &hash);
                    results.insert(
                        name,
                        TargetCheckResult {
                            status,
                            hash,
                            pending_hash,
                            pending_matches,
                        },
                    );
                }
                for (name, target) in registered {
                    results.entry(name).or_insert_with(|| TargetCheckResult {
                        status: "remote-only",
                        hash: definition_hash(&canonical_json(&target)),
                        pending_hash: None,
                        pending_matches: None,
                    });
                }
                Ok(results)
            })
        })
        .await
        .context("store check worker panicked")?
    }
}

fn ensure_hash_matches(prefix: &str, hash: &str) -> Result<()> {
    let prefix = prefix.strip_prefix("sha256:").unwrap_or(prefix);
    let digest = hash.strip_prefix("sha256:").unwrap_or(hash);
    if !prefix.is_empty()
        && prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        && digest.starts_with(&prefix.to_ascii_lowercase())
    {
        Ok(())
    } else {
        bail!("指定されたハッシュ prefix は現在の pending 定義と一致しません")
    }
}

fn approved_path(root: &Path, hash: &str) -> Result<PathBuf> {
    let digest = hash
        .strip_prefix("sha256:")
        .context("承認ハッシュの形式が不正です")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("承認ハッシュの形式が不正です");
    }
    Ok(root.join("approved").join(format!("{digest}.json")))
}

fn write_approval_record(root: &Path, hash: &str, canonical: &[u8]) -> Result<()> {
    let path = approved_path(root, hash)?;
    let parent = path.parent().context("approved path has no parent")?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        verify_existing_approval(&path, canonical)?;
        return touch_approval_record(&path, SystemTime::now());
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".approval.tmp.{}.{}", std::process::id(), sequence));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(canonical)?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&temp, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_existing_approval(&path, canonical)
            }
            Err(error) => Err(error)
                .with_context(|| format!("承認レコード {} を公開できません", path.display())),
        }
    })();
    let _ = fs::remove_file(&temp);
    result
}

fn refresh_existing_approval_record(
    root: &Path,
    hash: &str,
    canonical: &[u8],
    now: SystemTime,
) -> Result<()> {
    let path = approved_path(root, hash)?;
    if path.exists() {
        verify_existing_approval(&path, canonical)?;
        touch_approval_record(&path, now)?;
    }
    Ok(())
}

fn touch_approval_record(path: &Path, now: SystemTime) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .open(path)?
        .set_modified(now)
        .with_context(|| format!("承認レコード {} の mtime を更新できません", path.display()))
}

fn verify_existing_approval(path: &Path, canonical: &[u8]) -> Result<()> {
    let existing = fs::read(path)?;
    if existing == canonical {
        Ok(())
    } else {
        bail!("既存の承認レコード {} の内容が一致しません", path.display())
    }
}

fn approval_record_matches(root: &Path, hash: &str, canonical: &[u8]) -> Result<bool> {
    let path = approved_path(root, hash)?;
    if !path.exists() {
        return Ok(false);
    }
    verify_existing_approval(&path, canonical)?;
    Ok(true)
}

fn approval_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn approve_target(
    root: &Path,
    mut target: StoredTarget,
    canonical: &[u8],
    hash: &str,
) -> Result<StoredTarget> {
    write_approval_record(root, hash, canonical)?;
    target.hash = Some(hash.to_owned());
    target.approved_at = Some(approval_timestamp());
    Ok(target)
}

fn migrate_locked(root: &Path) -> Result<()> {
    let path = root.join("registered.toml");
    if !path.exists() {
        return Ok(());
    }
    let mut registered = load_for_update(&path)?;
    let needs_migration = registered
        .values()
        .any(|target| target.hash.is_none() || target.approved_at.is_none());
    if !needs_migration {
        return Ok(());
    }
    let backup = root.join("registered.toml.pre-migrate.bak");
    if !backup.exists() {
        fs::copy(&path, &backup)?;
    }
    for target in registered.values_mut() {
        let canonical = canonical_json(target);
        let hash = definition_hash(&canonical);
        write_approval_record(root, &hash, &canonical)?;
        target.hash = Some(hash);
        target.approved_at.get_or_insert_with(approval_timestamp);
    }
    atomic_save_target_map(&path, &registered)
}

fn verify_approval(root: &Path, target: &StoredTarget) -> Result<()> {
    let hash = target.hash.as_deref().context("承認ハッシュがありません")?;
    let canonical = canonical_json(target);
    if definition_hash(&canonical) != hash {
        bail!("登録本文と承認ハッシュが一致しません");
    }
    let path = approved_path(root, hash)?;
    let approved = fs::read(&path)
        .with_context(|| format!("承認レコード {} を読み込めません", path.display()))?;
    if definition_hash(&approved) != hash || approved != canonical {
        bail!("承認レコードと登録本文が一致しません");
    }
    Ok(())
}

fn approval_records_to_delete(
    records: &[ApprovalRecordAge],
    registered_digests: &std::collections::HashSet<String>,
    retention: Duration,
    now: SystemTime,
) -> Vec<String> {
    records
        .iter()
        .filter(|record| !registered_digests.contains(&record.file_name))
        .filter(|record| {
            now.duration_since(record.modified)
                .is_ok_and(|age| age > retention)
        })
        .map(|record| record.file_name.clone())
        .collect()
}

fn cleanup_approval_records_locked(
    root: &Path,
    retention: Duration,
    now: SystemTime,
) -> Result<usize> {
    let registered = load_for_update(&root.join("registered.toml"))?;
    let registered_digests = registered
        .values()
        .filter_map(|target| target.hash.as_deref())
        .filter_map(|hash| hash.strip_prefix("sha256:"))
        .map(|digest| format!("{digest}.json"))
        .collect();
    let approved_dir = root.join("approved");
    let entries = match fs::read_dir(&approved_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let valid_name = file_name.len() == 69
            && file_name.ends_with(".json")
            && file_name[..64].bytes().all(|byte| byte.is_ascii_hexdigit());
        if valid_name && entry.file_type()?.is_file() {
            records.push(ApprovalRecordAge {
                file_name,
                modified: entry.metadata()?.modified()?,
            });
        }
    }
    let selected = approval_records_to_delete(&records, &registered_digests, retention, now);
    for file_name in &selected {
        fs::remove_file(approved_dir.join(file_name))?;
    }
    Ok(selected.len())
}

fn load_for_update(path: &Path) -> Result<TargetMap> {
    if !path.exists() {
        return Ok(TargetMap::default());
    }
    let bytes = fs::read(path).with_context(|| format!("{} を読み込めません", path.display()))?;
    let parsed = std::str::from_utf8(&bytes)
        .context("UTF-8 ではありません")
        .and_then(|text| toml::from_str(text).context("TOML を解析できません"));
    match parsed {
        Ok(map) => Ok(map),
        Err(error) => {
            let corrupt = corrupt_path(path);
            fs::rename(path, &corrupt).with_context(|| {
                format!(
                    "破損ファイル {} を {} へ退避できません",
                    path.display(),
                    corrupt.display()
                )
            })?;
            bail!(
                "{} が破損しています。{} へ退避しました: {error:#}",
                path.display(),
                corrupt.display()
            )
        }
    }
}

fn corrupt_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{file}.corrupt.{stamp}.{}", std::process::id()))
}

pub(crate) fn atomic_save_target_map(path: &Path, map: &TargetMap) -> Result<()> {
    let parent = path.parent().context("store path has no parent")?;
    fs::create_dir_all(parent)?;
    let data = toml::to_string(map)?;
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{file}.tmp.{}.{}", std::process::id(), sequence));
    let result = (|| {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("一時ファイル {} を作成できません", temp.display()))?;
        output.write_all(data.as_bytes())?;
        output.sync_all()?;
        drop(output);
        if path.exists() {
            fs::copy(path, path.with_file_name(format!("{file}.bak")))?;
        }
        replace_file(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(not(windows))]
fn with_store_lock<T>(
    root: &Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use std::os::fd::AsRawFd;
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".store.lock"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock || Instant::now() >= deadline {
            return Err(error).context("clipwire store lock を取得できません");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let result = operation();
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

#[cfg(windows)]
fn with_store_lock<T>(
    root: &Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0},
            System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        },
    };
    fs::create_dir_all(root)?;
    let name: Vec<u16> = "Global\\clipwire_store\0".encode_utf16().collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? };
    let wait = unsafe { WaitForSingleObject(handle, timeout.as_millis() as u32) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        unsafe { CloseHandle(handle)? };
        bail!("Global\\clipwire_store mutex を取得できません: wait={wait:?}");
    }
    // WAIT_ABANDONED でも load_for_update の parse 検証後にだけ書き込む。
    let result = operation();
    unsafe {
        ReleaseMutex(handle)?;
        CloseHandle(handle)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_count(root: &Path) -> usize {
        fs::read_dir(root.join("approved"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }

    fn target(script: &str) -> StoredTarget {
        StoredTarget {
            script: Some(script.into()),
            ..StoredTarget::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ac_t3_3_2_corrupt_store_is_quarantined_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.toml");
        fs::write(&path, "not = [valid").unwrap();
        let store = Store::new(dir.path().to_owned());
        assert!(store
            .register("new".into(), target("echo new"), false)
            .await
            .is_err());
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("pending.toml.corrupt.")
                && fs::read_to_string(entry.path()).unwrap() == "not = [valid"
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ac_t3_3_3_fifty_concurrent_registers_are_all_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut tasks = Vec::new();
        for index in 0..50 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .register(
                        format!("target-{index}"),
                        target(&format!("echo {index}")),
                        false,
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(
            load_target_map(&dir.path().join("pending.toml"))
                .unwrap()
                .len(),
            50
        );
    }

    #[test]
    fn ac_t3_3_4_backup_is_the_previous_valid_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registered.toml");
        let first = TargetMap::from([("one".into(), target("echo one"))]);
        let second = TargetMap::from([("two".into(), target("echo two"))]);
        atomic_save_target_map(&path, &first).unwrap();
        atomic_save_target_map(&path, &second).unwrap();
        assert_eq!(
            load_target_map(&path.with_file_name("registered.toml.bak")).unwrap(),
            first
        );
        assert_eq!(load_target_map(&path).unwrap(), second);
    }

    #[test]
    fn ac_t3_3_1_interrupted_before_replace_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.toml");
        let original = TargetMap::from([("old".into(), target("echo old"))]);
        atomic_save_target_map(&path, &original).unwrap();
        fs::write(
            dir.path().join(".pending.toml.tmp.interrupted"),
            "truncated = [",
        )
        .unwrap();
        assert_eq!(load_target_map(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn ac_t3_4_1_2_and_4_legacy_store_migrates_idempotently_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registered.toml");
        let legacy = TargetMap::from([
            ("one".into(), target("let x = 1;")),
            ("two".into(), target("let x = 2;")),
        ]);
        atomic_save_target_map(&path, &legacy).unwrap();
        let before = fs::read(&path).unwrap();
        let store = Store::new(dir.path().to_owned());

        store.migrate().await.unwrap();
        let first = fs::read(&path).unwrap();
        assert_eq!(
            fs::read(dir.path().join("registered.toml.pre-migrate.bak")).unwrap(),
            before
        );
        assert_eq!(approval_count(dir.path()), 2);
        for name in ["one", "two"] {
            let migrated = store.verified_target(name).unwrap().unwrap();
            assert!(migrated.hash.is_some());
            assert!(migrated.approved_at.is_some());
            assert_eq!(migrated.script, legacy[name].script);
        }

        store.migrate().await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
        assert_eq!(approval_count(dir.path()), 2);

        // A crash after writing one content-addressed record is resumable.
        let mut interrupted = legacy.clone();
        let canonical = canonical_json(&interrupted["one"]);
        let hash = definition_hash(&canonical);
        write_approval_record(dir.path(), &hash, &canonical).unwrap();
        interrupted.get_mut("one").unwrap().hash = None;
        interrupted.get_mut("one").unwrap().approved_at = None;
        atomic_save_target_map(&path, &interrupted).unwrap();
        store.migrate().await.unwrap();
        assert!(store.verified_target("one").unwrap().is_some());
    }

    #[tokio::test]
    async fn ac_t3_4_3_and_10_tampered_missing_or_mismatched_records_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("item".into(), target("let x = 1;"), true)
            .await
            .unwrap();
        let registered = load_target_map(&dir.path().join("registered.toml")).unwrap();
        let path = approved_path(dir.path(), registered["item"].hash.as_deref().unwrap()).unwrap();

        fs::write(&path, b"x").unwrap();
        assert!(store.verified_target("item").is_err());
        fs::remove_file(&path).unwrap();
        assert!(store.verified_target("item").is_err());

        let mut changed = registered;
        changed.get_mut("item").unwrap().script = Some("let x = 2;".into());
        atomic_save_target_map(&dir.path().join("registered.toml"), &changed).unwrap();
        assert!(store.verified_target("item").is_err());
    }

    #[tokio::test]
    async fn ac_t3_4_6_and_7_auto_approve_writes_one_idempotent_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let first = store
            .register("item".into(), target("let x = 1;"), true)
            .await
            .unwrap();
        assert!(!first.unchanged);
        assert_eq!(approval_count(dir.path()), 1);
        let entry = store.verified_target("item").unwrap().unwrap();
        assert_eq!(entry.hash.as_deref(), Some(first.hash.as_str()));
        assert!(entry.approved_at.is_some());

        let second = store
            .register("item".into(), target("let x = 1;"), true)
            .await
            .unwrap();
        assert!(second.unchanged);
        assert_eq!(approval_count(dir.path()), 1);
        assert!(store.verified_target("item").unwrap().is_some());
    }

    #[tokio::test]
    async fn ac_t3_4_8_bulk_auto_approve_finishes_with_one_record_per_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let started = Instant::now();
        let fixture = std::env::var_os("CLIPWIRE_TARGETS_FIXTURE");
        let targets = fixture
            .map(|path| load_target_map(Path::new(&path)).unwrap())
            .unwrap_or_else(|| {
                (0..200)
                    .map(|index| {
                        (
                            format!("target-{index}"),
                            target(&format!("let x = {index};")),
                        )
                    })
                    .collect()
            });
        let unique: std::collections::HashSet<_> = targets
            .values()
            .map(|target| definition_hash(&canonical_json(target)))
            .collect();
        for (name, target) in targets {
            store.register(name, target, true).await.unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(60));
        assert_eq!(approval_count(dir.path()), unique.len());
    }

    #[tokio::test]
    async fn ac_t3_4_9_p2_style_metadata_loss_is_repaired_without_losing_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        for (name, script) in [("old", "let x = 1;"), ("new", "let x = 2;")] {
            store
                .register(name.into(), target(script), true)
                .await
                .unwrap();
        }
        let mut p2_round_trip = load_target_map(&dir.path().join("registered.toml")).unwrap();
        for entry in p2_round_trip.values_mut() {
            entry.hash = None;
            entry.approved_at = None;
        }
        atomic_save_target_map(&dir.path().join("registered.toml"), &p2_round_trip).unwrap();
        store.migrate().await.unwrap();
        for name in ["old", "new"] {
            assert!(store.verified_target(name).unwrap().is_some());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn ac_t3_4_9_c_windows_store_repairs_a_p2_style_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("windows-round-trip".into(), target("let x = 1;"), true)
            .await
            .unwrap();
        let path = dir.path().join("registered.toml");
        let mut p2 = load_target_map(&path).unwrap();
        p2.get_mut("windows-round-trip").unwrap().hash = None;
        p2.get_mut("windows-round-trip").unwrap().approved_at = None;
        atomic_save_target_map(&path, &p2).unwrap();
        store.migrate().await.unwrap();
        assert!(store
            .verified_target("windows-round-trip")
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn ac_t3_5_1_to_3_same_hash_is_noop_and_changed_hash_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("item".into(), target("A"), true)
            .await
            .unwrap();
        let same = store
            .register("item".into(), target("A"), false)
            .await
            .unwrap();
        assert!(same.unchanged);
        assert!(load_target_map(&dir.path().join("pending.toml"))
            .unwrap()
            .is_empty());
        assert!(store.verified_target("item").unwrap().is_some());

        store
            .register("item".into(), target("B"), false)
            .await
            .unwrap();
        assert!(!load_target_map(&dir.path().join("pending.toml"))
            .unwrap()
            .is_empty());
        assert!(load_target_map(&dir.path().join("registered.toml"))
            .unwrap()
            .is_empty());

        // In auto-approve mode, returning to A restores the content-addressed
        // approval and removes B. Manual mode must never restore registered via
        // HTTP register (T5.6).
        store
            .register("item".into(), target("A"), true)
            .await
            .unwrap();
        store
            .register("item".into(), target("B"), false)
            .await
            .unwrap();
        let back_to_a = store
            .register("item".into(), target("A"), true)
            .await
            .unwrap();
        assert!(back_to_a.unchanged);
        assert!(load_target_map(&dir.path().join("pending.toml"))
            .unwrap()
            .is_empty());
        assert!(store.verified_target("item").unwrap().is_some());
    }

    #[tokio::test]
    async fn ac_t3_8_1_3_approve_is_hash_bound_and_rechecks_current_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("demo".into(), target("echo first"), false)
            .await
            .unwrap();
        let first_hash = definition_hash(&canonical_json(
            store.pending().unwrap().get("demo").unwrap(),
        ));

        assert!(store.approve("demo", "deadbeef").is_err());
        assert!(store.pending().unwrap().contains_key("demo"));

        // Simulate a re-register while the CLI confirmation prompt is open.
        store
            .register("demo".into(), target("echo second"), false)
            .await
            .unwrap();
        assert!(store.approve("demo", &first_hash).is_err());
        assert_eq!(
            store
                .pending()
                .unwrap()
                .get("demo")
                .unwrap()
                .script
                .as_deref(),
            Some("echo second")
        );

        let current_hash = definition_hash(&canonical_json(
            store.pending().unwrap().get("demo").unwrap(),
        ));
        store.approve("demo", &current_hash[7..19]).unwrap();
        assert!(!store.pending().unwrap().contains_key("demo"));
        assert!(store.verified_target("demo").unwrap().is_some());
    }

    #[tokio::test]
    async fn ac_t3_8_5_deny_only_removes_matching_pending_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("kept".into(), target("echo approved"), true)
            .await
            .unwrap();
        store
            .register("denied".into(), target("echo pending"), false)
            .await
            .unwrap();

        assert!(store.deny("denied", Some("badbad")).is_err());
        assert!(store.pending().unwrap().contains_key("denied"));
        let hash = definition_hash(&canonical_json(
            store.pending().unwrap().get("denied").unwrap(),
        ));
        store.deny("denied", Some(&hash)).unwrap();
        assert!(!store.pending().unwrap().contains_key("denied"));
        assert!(store.verified_target("kept").unwrap().is_some());
    }

    #[test]
    fn ac_t5_7_selection_is_pure_and_keeps_registered_or_recent_records() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let old = now - Duration::from_secs(101);
        let recent = now - Duration::from_secs(99);
        let records = vec![
            ApprovalRecordAge {
                file_name: "registered.json".into(),
                modified: old,
            },
            ApprovalRecordAge {
                file_name: "expired.json".into(),
                modified: old,
            },
            ApprovalRecordAge {
                file_name: "recent.json".into(),
                modified: recent,
            },
        ];
        let registered = std::collections::HashSet::from(["registered.json".into()]);
        assert_eq!(
            approval_records_to_delete(&records, &registered, Duration::from_secs(100), now),
            ["expired.json"]
        );
    }

    #[tokio::test]
    async fn ac_t5_7_1_and_2_cleanup_keeps_registered_and_recent_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("kept".into(), target("echo kept"), true)
            .await
            .unwrap();
        store
            .register("expired".into(), target("echo expired"), true)
            .await
            .unwrap();
        store
            .register("recent".into(), target("echo recent"), true)
            .await
            .unwrap();
        let mut registered = load_target_map(&dir.path().join("registered.toml")).unwrap();
        let kept_hash = registered["kept"].hash.clone().unwrap();
        let expired_hash = registered.remove("expired").unwrap().hash.unwrap();
        let recent_hash = registered.remove("recent").unwrap().hash.unwrap();
        atomic_save_target_map(&dir.path().join("registered.toml"), &registered).unwrap();

        let now = UNIX_EPOCH + Duration::from_secs(2_000_000);
        let old = now - Duration::from_secs(101);
        touch_approval_record(&approved_path(dir.path(), &kept_hash).unwrap(), old).unwrap();
        touch_approval_record(&approved_path(dir.path(), &expired_hash).unwrap(), old).unwrap();
        touch_approval_record(
            &approved_path(dir.path(), &recent_hash).unwrap(),
            now - Duration::from_secs(99),
        )
        .unwrap();

        assert_eq!(
            store
                .cleanup_approval_records(Duration::from_secs(100), now)
                .await
                .unwrap(),
            1
        );
        assert!(approved_path(dir.path(), &kept_hash).unwrap().exists());
        assert!(!approved_path(dir.path(), &expired_hash).unwrap().exists());
        assert!(approved_path(dir.path(), &recent_hash).unwrap().exists());
    }

    #[tokio::test]
    async fn ac_t5_7_2_reregister_refreshes_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let item = target("echo same");
        let result = store
            .register("same".into(), item.clone(), true)
            .await
            .unwrap();
        let path = approved_path(dir.path(), &result.hash).unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        touch_approval_record(&path, old).unwrap();
        store.register("same".into(), item, true).await.unwrap();
        assert!(fs::metadata(path).unwrap().modified().unwrap() > old);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ac_t5_7_3_cleanup_and_exec_verification_share_the_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        store
            .register("live".into(), target("echo live"), true)
            .await
            .unwrap();
        let cleanup = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .cleanup_approval_records(Duration::ZERO, SystemTime::now())
                    .await
                    .unwrap()
            })
        };
        let verified = tokio::task::spawn_blocking(move || store.verified_target("live").unwrap());
        assert_eq!(cleanup.await.unwrap(), 0);
        assert!(verified.await.unwrap().is_some());
    }
}
