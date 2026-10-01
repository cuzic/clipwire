//! Side-effect-free runner policy. OS handles, clocks, and I/O stay in the shell.

use std::{ffi::OsString, time::Duration};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct LogChunk {
    pub(super) keep: usize,
    pub(super) add_marker: bool,
}

pub(super) fn log_chunk(written: usize, marked: bool, incoming: usize, limit: usize) -> LogChunk {
    let keep = limit.saturating_sub(written).min(incoming);
    LogChunk {
        keep,
        add_marker: keep != incoming && !marked,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelayAction {
    Continue,
    Finish,
}

pub(super) fn relay_action(eof: bool, main_finished: bool, idle: Duration) -> RelayAction {
    if eof || (main_finished && idle >= super::RELAY_IDLE_TIMEOUT) {
        RelayAction::Finish
    } else {
        RelayAction::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Completion {
    pub(super) state: super::JobState,
    pub(super) exit_code: Option<i32>,
}

pub(super) fn completion(spawned: bool, success: bool, exit_code: Option<i32>) -> Completion {
    let state = if !spawned {
        super::JobState::SpawnFailed
    } else if success {
        super::JobState::Succeeded
    } else {
        super::JobState::Failed
    };
    Completion { state, exit_code }
}

pub(super) fn child_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    overrides: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut result: Vec<_> = inherited
        .into_iter()
        .filter(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case("CLIPD_TOKEN"))
        .collect();
    for (key, value) in overrides {
        if let Some(entry) = result.iter_mut().find(|(existing, _)| {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&key.to_string_lossy())
        }) {
            entry.1 = value.clone();
        } else {
            result.push((key.clone(), value.clone()));
        }
    }
    result.retain(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case("CLIPD_TOKEN"));
    result
}

pub(super) fn append_output(output: &mut Vec<u8>, chunk: &[u8]) {
    output.extend_from_slice(chunk);
}

pub(super) fn lingering_process_warning(pids: &[u32]) -> Option<String> {
    (!pids.is_empty()).then(|| {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[warn] {} processes still running (pids: {list})",
            pids.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_limit_policy_keeps_prefix_and_marks_once() {
        assert_eq!(
            log_chunk(3, false, 4, 10),
            LogChunk {
                keep: 4,
                add_marker: false
            }
        );
        assert_eq!(
            log_chunk(8, false, 4, 10),
            LogChunk {
                keep: 2,
                add_marker: true
            }
        );
        assert_eq!(
            log_chunk(10, true, 4, 10),
            LogChunk {
                keep: 0,
                add_marker: false
            }
        );
        assert_eq!(
            log_chunk(10, false, 0, 10),
            LogChunk {
                keep: 0,
                add_marker: false
            }
        );
    }

    #[test]
    fn relay_finishes_on_eof_or_finished_main_idle_timeout() {
        assert_eq!(
            relay_action(true, false, Duration::ZERO),
            RelayAction::Finish
        );
        assert_eq!(
            relay_action(false, true, super::super::RELAY_IDLE_TIMEOUT),
            RelayAction::Finish
        );
        assert_eq!(
            relay_action(false, true, Duration::from_millis(1999)),
            RelayAction::Continue
        );
        assert_eq!(
            relay_action(false, false, Duration::from_secs(99)),
            RelayAction::Continue
        );
    }

    #[test]
    fn completion_covers_success_failure_spawn_failure_and_signal() {
        assert_eq!(
            completion(true, true, Some(0)),
            Completion {
                state: super::super::JobState::Succeeded,
                exit_code: Some(0)
            }
        );
        assert_eq!(
            completion(true, false, Some(7)),
            Completion {
                state: super::super::JobState::Failed,
                exit_code: Some(7)
            }
        );
        assert_eq!(
            completion(true, false, None),
            Completion {
                state: super::super::JobState::Failed,
                exit_code: None
            }
        );
        assert_eq!(
            completion(false, false, None),
            Completion {
                state: super::super::JobState::SpawnFailed,
                exit_code: None
            }
        );
    }

    #[test]
    fn environment_merges_overrides_and_always_removes_token() {
        let inherited = [
            ("Path".into(), "old".into()),
            ("clipd_token".into(), "secret".into()),
        ];
        let overrides = [
            ("PATH".into(), "new".into()),
            ("CLIPD_TOKEN".into(), "other".into()),
            ("X".into(), "1".into()),
        ];
        let env = child_environment(inherited, &overrides);
        assert_eq!(
            env,
            [
                (OsString::from("Path"), OsString::from("new")),
                (OsString::from("X"), OsString::from("1"))
            ]
        );
    }

    #[test]
    fn output_and_warning_format_are_deterministic() {
        let mut output = b"one".to_vec();
        append_output(&mut output, b"two");
        assert_eq!(output, b"onetwo");
        assert_eq!(lingering_process_warning(&[]), None);
        assert_eq!(
            lingering_process_warning(&[42, 7]).as_deref(),
            Some("[warn] 2 processes still running (pids: 42, 7)")
        );
    }
}
