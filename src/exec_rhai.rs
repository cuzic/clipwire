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

pub(crate) fn exec_rhai(script: &str, dir: Option<&str>) -> Result<(Vec<u8>, i32)> {
    exec_rhai_cancelable(script, dir, Arc::new(AtomicBool::new(false)))
}

pub(crate) fn exec_rhai_cancelable(
    script: &str,
    dir: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<(Vec<u8>, i32)> {
    let (output, relay) = OrderedOutput::new()?;
    let runner = Arc::new(Runner::with_output(
        std::env::temp_dir().join("clipwire-runner"),
        output.clone(),
    ));
    let dir = dir.map(std::path::PathBuf::from);
    let mut engine = rhai::Engine::new();
    engine
        .set_max_operations(MAX_OPERATIONS)
        .set_max_string_size(MAX_STRING_SIZE)
        .set_max_array_size(MAX_ARRAY_SIZE)
        .set_max_map_size(MAX_MAP_SIZE)
        .set_max_call_levels(MAX_CALL_LEVELS);

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
