use super::*;
use crate::runner::{JobSpec, OrderedOutput, Runner};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// Local debug measurements put the two runaway AC probes together below 250 ms
// at 1M operations / 1 MiB strings, while every existing script test remains
// below the limits. Collections get similarly conservative entry caps and the
// 64-level call cap is well above the non-recursive registered targets. Sizes
// are bytes for strings, entry counts for arrays/maps, and stack depth.
const MAX_OPERATIONS: u64 = 1_000_000;
const MAX_STRING_SIZE: usize = 1_048_576;
const MAX_ARRAY_SIZE: usize = 100_000;
const MAX_MAP_SIZE: usize = 10_000;
const MAX_CALL_LEVELS: usize = 64;
const MAX_SLEEP_MS: i64 = 60_000;
const SLEEP_SLICE_MS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RhaiLimits {
    operations: u64,
    string_size: usize,
    array_size: usize,
    map_size: usize,
    call_levels: usize,
}

const fn rhai_limits() -> RhaiLimits {
    RhaiLimits {
        operations: MAX_OPERATIONS,
        string_size: MAX_STRING_SIZE,
        array_size: MAX_ARRAY_SIZE,
        map_size: MAX_MAP_SIZE,
        call_levels: MAX_CALL_LEVELS,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn exec_rhai(script: &str, dir: Option<&str>) -> Result<(Vec<u8>, i32)> {
    exec_rhai_inner(script, dir, Arc::new(AtomicBool::new(false)), true, false)
}

#[allow(dead_code)]
pub(crate) fn exec_rhai_cancelable(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<(Vec<u8>, i32)> {
    exec_rhai_inner(script, dir, cancelled, true, false)
}

pub(crate) fn exec_rhai_with_deadline_output(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
    child_changed: Arc<dyn Fn(Option<crate::jobs::ChildIdentity>) + Send + Sync>,
    output: OrderedOutput,
    relay: std::thread::JoinHandle<std::io::Result<()>>,
) -> Result<(Vec<u8>, i32)> {
    exec_rhai_with_output(
        script,
        dir,
        cancelled,
        false,
        true,
        Some(child_changed),
        output,
        relay,
    )
}

fn exec_rhai_inner(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
    operation_limit: bool,
    timeout_exit: bool,
) -> Result<(Vec<u8>, i32)> {
    exec_rhai_inner_with_child(script, dir, cancelled, operation_limit, timeout_exit, None)
}

fn exec_rhai_inner_with_child(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
    operation_limit: bool,
    timeout_exit: bool,
    child_changed: Option<Arc<dyn Fn(Option<crate::jobs::ChildIdentity>) + Send + Sync>>,
) -> Result<(Vec<u8>, i32)> {
    let (output, relay) = OrderedOutput::new()?;
    exec_rhai_with_output(
        script,
        dir,
        cancelled,
        operation_limit,
        timeout_exit,
        child_changed,
        output,
        relay,
    )
}

#[allow(clippy::too_many_arguments)]
fn exec_rhai_with_output(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
    operation_limit: bool,
    timeout_exit: bool,
    child_changed: Option<Arc<dyn Fn(Option<crate::jobs::ChildIdentity>) + Send + Sync>>,
    output: OrderedOutput,
    relay: std::thread::JoinHandle<std::io::Result<()>>,
) -> Result<(Vec<u8>, i32)> {
    let runner = Arc::new(Runner::with_output(
        std::env::temp_dir().join("clipwire-runner"),
        output.clone(),
    ));
    let dir = dir.map(std::path::PathBuf::from);
    let mut engine = rhai::Engine::new();
    let limits = rhai_limits();
    if operation_limit {
        engine.set_max_operations(limits.operations);
    }
    engine
        .set_max_string_size(limits.string_size)
        .set_max_array_size(limits.array_size)
        .set_max_map_size(limits.map_size)
        .set_max_call_levels(limits.call_levels);

    {
        let cancelled = Arc::clone(&cancelled);
        engine.on_progress(move |_| {
            cancelled
                .load(Ordering::Relaxed)
                .then(|| "execution cancelled".into())
        });
    }
    {
        let output = output.clone();
        engine.on_print(move |text| output.write_line(text));
    }
    {
        let output = output.clone();
        engine.on_debug(move |text, source, position| {
            output.write_line(&format!(
                "{} @ {position:?}: {text}",
                source.unwrap_or("<script>")
            ));
        });
    }

    {
        let runner = Arc::clone(&runner);
        let dir = dir.clone();
        let cancelled = Arc::clone(&cancelled);
        let child_changed = child_changed.clone();
        engine.register_fn(
            "run",
            move |args: rhai::Array| -> Result<(), Box<rhai::EvalAltResult>> {
                let args = string_args(args);
                if args.is_empty() {
                    return Ok(());
                }
                let mut spec = JobSpec::new(&args[0], "rhai");
                spec.args = args[1..].iter().map(Into::into).collect();
                spec.cwd = dir.clone();
                let mut job = runner.spawn(spec).map_err(|e| e.to_string())?;
                if let Some(error) = job.spawn_error() {
                    return Err(error.to_string().into());
                }
                if let Some(callback) = &child_changed {
                    callback(job.child_identity().map_err(|e| e.to_string())?);
                }
                job.wait_cancelable(&cancelled).map_err(|e| e.to_string())?;
                if let Some(callback) = &child_changed {
                    callback(None);
                }
                if job.exit_code() != Some(0) {
                    return Err(format!("exit code {}", job.exit_code().unwrap_or(-1)).into());
                }
                Ok(())
            },
        );
    }

    {
        let runner = Arc::clone(&runner);
        let output = output.clone();
        let dir = dir.clone();
        let cancelled = Arc::clone(&cancelled);
        let child_changed = child_changed.clone();
        engine.register_fn("run_ok", move |args: rhai::Array| -> bool {
            let args = string_args(args);
            if args.is_empty() {
                return true;
            }
            let mut spec = JobSpec::new(&args[0], "rhai");
            spec.args = args[1..].iter().map(Into::into).collect();
            spec.cwd = dir.clone();
            match runner.spawn(spec) {
                Ok(mut job) if job.spawn_error().is_none() => {
                    if let Some(callback) = &child_changed {
                        callback(job.child_identity().ok().flatten());
                    }
                    let ok = job.wait_cancelable(&cancelled).is_ok() && job.exit_code() == Some(0);
                    if let Some(callback) = &child_changed {
                        callback(None);
                    }
                    ok
                }
                Ok(job) => {
                    output.write_all(
                        format!(
                            "run_ok: {} の起動に失敗しました: {}\n",
                            args[0],
                            job.spawn_error().unwrap()
                        )
                        .as_bytes(),
                    );
                    false
                }
                Err(error) => {
                    output.write_all(
                        format!("run_ok: {} の起動に失敗しました: {error}\n", args[0]).as_bytes(),
                    );
                    false
                }
            }
        });
    }

    {
        let dir = dir.clone();
        engine.register_fn("file_exists", move |path: &str| -> bool {
            resolve_path(dir.as_deref(), path).exists()
        });
    }
    {
        let cancelled = Arc::clone(&cancelled);
        engine.register_fn(
            "sleep",
            move |milliseconds: i64| -> Result<(), Box<rhai::EvalAltResult>> {
                let slices = sleep_slices(milliseconds).map_err(|error| error.to_string())?;
                for slice in slices {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err("execution cancelled".into());
                    }
                    std::thread::sleep(slice);
                }
                if cancelled.load(Ordering::Relaxed) {
                    return Err("execution cancelled".into());
                }
                Ok(())
            },
        );
    }
    {
        let runner = Arc::clone(&runner);
        let dir = dir.clone();
        engine.register_fn(
            "start_detached",
            move |args: rhai::Array| -> Result<i64, Box<rhai::EvalAltResult>> {
                let args = string_args(args);
                if args.is_empty() {
                    return Err("start_detached requires a command".into());
                }
                let mut spec = JobSpec::new(&args[0], "rhai-detached");
                spec.args = args[1..].iter().map(Into::into).collect();
                spec.cwd = dir.clone();
                runner
                    .start_detached(spec)
                    .map(i64::from)
                    .map_err(|error| error.to_string().into())
            },
        );
    }
    engine.register_fn("notify", move |message: &str| {
        notify(message);
    });
    {
        let dir = dir.clone();
        engine.register_fn("rm", move |path: &str| -> bool {
            std::fs::remove_file(resolve_path(dir.as_deref(), path)).is_ok()
        });
    }

    let code = match engine.eval::<()>(script) {
        Ok(_) => 0,
        Err(error) => {
            output.write_all(format!("script error: {error}\n").as_bytes());
            1
        }
    };
    let code = if timeout_exit && cancelled.load(Ordering::Relaxed) {
        124
    } else {
        code
    };
    drop(engine);
    drop(runner);
    let bytes = output.finish(relay)?;
    Ok((bytes, code))
}

fn sleep_slices(milliseconds: i64) -> std::result::Result<Vec<std::time::Duration>, &'static str> {
    if !(0..=MAX_SLEEP_MS).contains(&milliseconds) {
        return Err("sleep は 0..=60000 ms で指定してください");
    }
    let mut remaining = milliseconds as u64;
    let mut slices = Vec::new();
    while remaining != 0 {
        let slice = remaining.min(SLEEP_SLICE_MS);
        slices.push(std::time::Duration::from_millis(slice));
        remaining -= slice;
    }
    Ok(slices)
}

#[cfg(windows)]
fn notify(message: &str) {
    crate::win_clip::show_balloon(message);
}

#[cfg(not(windows))]
fn notify(_message: &str) {}

fn string_args(args: rhai::Array) -> Vec<String> {
    args.into_iter()
        .map(|arg| {
            arg.clone()
                .try_cast::<String>()
                .unwrap_or_else(|| arg.to_string())
        })
        .collect()
}

fn resolve_path(dir: Option<&std::path::Path>, path: &str) -> std::path::PathBuf {
    dir.map_or_else(|| path.into(), |dir| dir.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(script: &str) -> (String, i32) {
        let (bytes, code) = exec_rhai(script, None).unwrap();
        (String::from_utf8(bytes).unwrap(), code)
    }

    #[test]
    fn resource_limit_configuration_is_stable() {
        assert_eq!(
            rhai_limits(),
            RhaiLimits {
                operations: 1_000_000,
                string_size: 1_048_576,
                array_size: 100_000,
                map_size: 10_000,
                call_levels: 64,
            }
        );
    }

    #[test]
    fn sleep_policy_splits_at_100_ms_and_rejects_over_limit() {
        assert!(sleep_slices(0).unwrap().is_empty());
        assert_eq!(
            sleep_slices(250).unwrap(),
            [
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(50),
            ]
        );
        assert!(sleep_slices(60_001).is_err());
        assert!(sleep_slices(-1).is_err());
    }

    #[test]
    fn ac_t7_4_1_sleep_waits_and_enforces_the_limit() {
        let started = std::time::Instant::now();
        let (output, code) = text("sleep(200);");
        let elapsed = started.elapsed();
        assert_eq!(code, 0, "{output}");
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "{elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");

        let (output, code) = text("sleep(61000);");
        assert_eq!(code, 1);
        assert!(output.contains("0..=60000"), "{output:?}");
    }

    #[test]
    fn ac_t7_4_2_sleep_cancellation_returns_within_200_ms() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            exec_rhai_cancelable("sleep(30000);", None, worker_cancelled)
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        let started = std::time::Instant::now();
        cancelled.store(true, Ordering::Relaxed);
        let (output, code) = worker.join().unwrap().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(code, 1);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("execution cancelled"));
    }

    #[test]
    fn notify_xml_escaping_is_stable() {
        assert_eq!(crate::xml_escape("a<b&c"), "a&lt;b&amp;c");
    }

    #[test]
    fn ac_t7_4_4_rejected_apis_are_not_registered() {
        for script in [
            r#"env("X");"#,
            "retry(1, 1);",
            "wait_port(1, 1);",
            r#"read_text("x");"#,
            r#"http_get("https://example.invalid");"#,
        ] {
            let (output, code) = text(script);
            assert_eq!(code, 1, "{script}: {output}");
            assert!(output.contains("Function not found"), "{script}: {output}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn ac_t7_4_3_detached_child_survives_job_kill_and_returns_pid() {
        struct DetachedCleanup(libc::pid_t);
        impl Drop for DetachedCleanup {
            fn drop(&mut self) {
                // SAFETY: the PID is the detached session/process-group leader.
                unsafe {
                    libc::killpg(self.0, libc::SIGKILL);
                }
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            exec_rhai_cancelable(
                r#"let pid = start_detached(["sh", "-c", "sleep 30"]); print(pid); run(["sh", "-c", "sleep 30"]);"#,
                None,
                worker_cancelled,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        cancelled.store(true, Ordering::Relaxed);
        let (output, code) = worker.join().unwrap().unwrap();
        assert_eq!(code, 1);
        let output = String::from_utf8(output).unwrap();
        let pid: libc::pid_t = output.lines().next().unwrap().parse().unwrap();
        let _cleanup = DetachedCleanup(pid);
        // The regular run was killed with killpg; the detached session remains.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::new(());

        fn text(script: &str) -> (String, i32) {
            let (bytes, code) = exec_rhai(script, None).unwrap();
            (
                String::from_utf8(bytes).unwrap().replace("\r\n", "\n"),
                code,
            )
        }

        #[test]
        fn cmd_output_exit_code_and_print_order() {
            let (output, code) = text(
                r#"run(["cmd", "/d", "/c", "echo stdout& (echo stderr) 1>&2"]); print("done");"#,
            );
            assert_eq!(code, 0);
            assert_eq!(output, "stdout\nstderr\ndone\n");

            let (output, code) = text(r#"run(["cmd", "/d", "/c", "exit /b 7"]);"#);
            assert_eq!(code, 1);
            assert!(output.starts_with("script error:"), "{output:?}");
            assert!(output.contains("exit code 7"), "{output:?}");
        }

        #[test]
        fn run_ok_and_missing_command_keep_the_contract() {
            let (output, code) = text(
                r#"if run_ok(["cmd", "/d", "/c", "exit /b 0"]) { print("ok"); }
                   if !run_ok(["cmd", "/d", "/c", "exit /b 9"]) { print("failed"); }
                   if !run_ok(["clipwire-command-that-does-not-exist"]) { print("missing"); }"#,
            );
            assert_eq!(code, 0);
            assert!(output.contains("ok\n"), "{output:?}");
            assert!(output.contains("failed\n"), "{output:?}");
            assert!(
                output.contains("run_ok: clipwire-command-that-does-not-exist"),
                "{output:?}"
            );
            assert!(output.ends_with("missing\n"), "{output:?}");
        }

        #[test]
        fn child_environment_excludes_clipd_token() {
            let _guard = ENV_LOCK.lock().unwrap();
            std::env::set_var("CLIPD_TOKEN", "windows-ci-secret");
            let result = text(r#"run(["cmd", "/d", "/c", "set"]);"#);
            std::env::remove_var("CLIPD_TOKEN");
            let (output, code) = result;
            assert_eq!(code, 0);
            assert!(
                !output.to_ascii_uppercase().contains("CLIPD_TOKEN="),
                "{output:?}"
            );
        }

        #[test]
        fn resource_limits_return_script_errors() {
            for script in ["loop {}", r#"let s = "a"; loop { s += s; }"#] {
                let (output, code) = text(script);
                assert_eq!(code, 1);
                assert!(output.starts_with("script error:"), "{output:?}");
            }
        }
    }
}
