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
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct Store {
    root: PathBuf,
    serial: Arc<tokio::sync::Mutex<()>>,
}

pub(crate) struct RegisterResult {
    pub(crate) reapproval: bool,
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
                if auto_approve {
                    registered.insert(name, target);
                    atomic_save_target_map(&registered_path, &registered)?;
                    return Ok(RegisterResult { reapproval: false });
                }
                let reapproval = registered.remove(&name).is_some();
                let mut pending = load_for_update(&pending_path)?;
                pending.insert(name, target);
                atomic_save_target_map(&pending_path, &pending)?;
                atomic_save_target_map(&registered_path, &registered)?;
                Ok(RegisterResult { reapproval })
            })
        })
        .await
        .context("store worker panicked")?
    }

    pub(crate) fn approve(&self, name: &str, dir: Option<&str>) -> Result<()> {
        with_store_lock(&self.root, LOCK_TIMEOUT, || {
            let pending_path = self.root.join("pending.toml");
            let registered_path = self.root.join("registered.toml");
            let mut pending = load_for_update(&pending_path)?;
            let mut entry = pending
                .remove(name)
                .with_context(|| format!("'{name}' は pending にありません"))?;
            if let Some(dir) = dir {
                entry.dir = Some(dir.to_owned());
            }
            let mut registered = load_for_update(&registered_path)?;
            registered.insert(name.to_owned(), entry);
            atomic_save_target_map(&registered_path, &registered)?;
            atomic_save_target_map(&pending_path, &pending)
        })
    }
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
}
