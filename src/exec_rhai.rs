use super::*;

pub(crate) fn exec_rhai(script: &str, dir: Option<&str>) -> Result<(Vec<u8>, i32)> {
    use std::sync::{Arc, Mutex};
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let dir = dir.map(str::to_string);

    let mut engine = rhai::Engine::new();

    // run(["cmd", "arg", ...]) — 失敗したらスクリプトを停止
    {
        let out = out.clone();
        let dir = dir.clone();
        engine.register_fn(
            "run",
            move |args: rhai::Array| -> Result<(), Box<rhai::EvalAltResult>> {
                let args: Vec<String> = args
                    .iter()
                    .map(|a| {
                        a.clone()
                            .try_cast::<String>()
                            .unwrap_or_else(|| a.to_string())
                    })
                    .collect();
                if args.is_empty() {
                    return Ok(());
                }
                let mut cmd = std::process::Command::new(&args[0]);
                cmd.args(&args[1..]);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                if let Some(ref d) = dir {
                    cmd.current_dir(d);
                }
                let o = cmd.output().map_err(|e| e.to_string())?;
                {
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(&o.stderr);
                    g.extend_from_slice(&o.stdout);
                }
                if !o.status.success() {
                    return Err(format!("exit code {}", o.status.code().unwrap_or(-1)).into());
                }
                Ok(())
            },
        );
    }

    // run_ok(["cmd", ...]) — 失敗しても続行、成功なら true
    {
        let out = out.clone();
        let dir = dir.clone();
        engine.register_fn("run_ok", move |args: rhai::Array| -> bool {
            let args: Vec<String> = args
                .iter()
                .map(|a| {
                    a.clone()
                        .try_cast::<String>()
                        .unwrap_or_else(|| a.to_string())
                })
                .collect();
            if args.is_empty() {
                return true;
            }
            let mut cmd = std::process::Command::new(&args[0]);
            cmd.args(&args[1..]);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            if let Some(ref d) = dir {
                cmd.current_dir(d);
            }
            match cmd.output() {
                Ok(o) => {
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(&o.stderr);
                    g.extend_from_slice(&o.stdout);
                    o.status.success()
                }
                Err(e) => {
                    // run_ok は失敗を無視して続行する設計だが、起動すらできな
                    // かった理由（プログラムが見つからない等）まで無音にする
                    // と原因調査が不可能になるため、出力に残す。
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(
                        format!("run_ok: {} の起動に失敗しました: {e}\n", args[0]).as_bytes(),
                    );
                    false
                }
            }
        });
    }

    // file_exists(path)
    {
        let dir = dir.clone();
        engine.register_fn("file_exists", move |path: &str| -> bool {
            let p = match &dir {
                Some(d) => std::path::Path::new(d).join(path),
                None => path.into(),
            };
            p.exists()
        });
    }

    // rm(path) — ファイル削除、失敗しても続行
    {
        let dir = dir.clone();
        engine.register_fn("rm", move |path: &str| -> bool {
            let p = match &dir {
                Some(d) => std::path::Path::new(d).join(path),
                None => path.into(),
            };
            std::fs::remove_file(p).is_ok()
        });
    }

    let code = match engine.eval::<()>(script) {
        Ok(_) => 0i32,
        Err(e) => {
            out.lock()
                .unwrap()
                .extend_from_slice(format!("script error: {e}\n").as_bytes());
            1i32
        }
    };
    let bytes = out.lock().unwrap().clone();
    Ok((bytes, code))
}
