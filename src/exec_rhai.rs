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

pub(crate) fn exec_rhai_with_deadline(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<(Vec<u8>, i32)> {
    exec_rhai_inner(script, dir, cancelled, false, true)
}

fn exec_rhai_inner(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
    operation_limit: bool,
    timeout_exit: bool,
) -> Result<(Vec<u8>, i32)> {
    let (output, relay) = OrderedOutput::new()?;
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
                job.wait_cancelable(&cancelled).map_err(|e| e.to_string())?;
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
                    job.wait_cancelable(&cancelled).is_ok() && job.exit_code() == Some(0)
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
                if !(0..=60_000).contains(&milliseconds) {
                    return Err("sleep は 0..=60000 ms で指定してください".into());
                }
                let mut remaining = std::time::Duration::from_millis(milliseconds as u64);
                while !remaining.is_zero() && !cancelled.load(Ordering::Relaxed) {
                    let slice = remaining.min(std::time::Duration::from_millis(100));
                    std::thread::sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
                Ok(())
            },
        );
    }
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
