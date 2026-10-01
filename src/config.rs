use super::*;

// ── Exec target config ────────────────────────────────────────────────────────

/// `steps` フィールドの値: 構造化配列 or 1行1コマンドの文字列
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum StepsDef {
    Text(String),
    Argv(Vec<Vec<String>>),
}

impl StepsDef {
    pub(crate) fn into_argv(self) -> Vec<Vec<String>> {
        match self {
            StepsDef::Argv(v) => v,
            StepsDef::Text(s) => s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(shlex::split)
                .collect(),
        }
    }
}

/// Linux 側 targets.toml のエントリ兼 HTTP 登録ペイロード (dir なし)
#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(untagged)]
pub(crate) enum ExecPayload {
    Script {
        script: String,
    },
    Steps {
        steps: StepsDef,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
}

/// Windows 側 pending.toml / registered.toml のエントリ (dir あり)
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoredTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) steps: Option<StepsDef>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) env: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
#[cfg_attr(not(test), allow(dead_code))]
struct CanonicalTarget<'a> {
    v: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    script: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<&'a StepsDef>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    env: &'a std::collections::BTreeMap<String, String>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn canonical_json(target: &StoredTarget) -> Vec<u8> {
    serde_json::to_vec(&CanonicalTarget {
        v: 1,
        dir: target.dir.as_deref(),
        script: target.script.as_deref(),
        steps: target.steps.as_ref(),
        env: &target.env,
    })
    .expect("canonical target serialization cannot fail")
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn definition_hash(canonical: &[u8]) -> String {
    use sha2::Digest;
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(canonical)))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DefinitionError {
    pub(crate) field: String,
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) codepoint: u32,
}

impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: 行 {} 桁 {}: U+{:04X} は使用できません",
            self.field, self.line, self.column, self.codepoint
        )
    }
}

impl std::error::Error for DefinitionError {}

fn validate_definition_string(field: String, value: &str) -> Result<(), DefinitionError> {
    use unicode_general_category::{get_general_category, GeneralCategory};

    let chars: Vec<char> = value.chars().collect();
    let (mut line, mut column) = (1, 1);
    for (index, &ch) in chars.iter().enumerate() {
        let category = get_general_category(ch);
        let allowed_whitespace =
            matches!(ch, '\t' | '\n') || (ch == '\r' && chars.get(index + 1) == Some(&'\n'));
        let forbidden = (!allowed_whitespace
            && matches!(
                category,
                GeneralCategory::Control
                    | GeneralCategory::Format
                    | GeneralCategory::LineSeparator
                    | GeneralCategory::ParagraphSeparator
            ))
            || matches!(ch, '\u{115f}' | '\u{1160}' | '\u{3164}');
        if forbidden {
            return Err(DefinitionError {
                field,
                line,
                column,
                codepoint: ch as u32,
            });
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Ok(())
}

pub(crate) fn validate_definition(target: &StoredTarget) -> Result<(), DefinitionError> {
    if let Some(dir) = &target.dir {
        validate_definition_string("dir".into(), dir)?;
    }
    if let Some(script) = &target.script {
        validate_definition_string("script".into(), script)?;
    }
    if let Some(steps) = &target.steps {
        match steps {
            StepsDef::Text(text) => validate_definition_string("steps".into(), text)?,
            StepsDef::Argv(commands) => {
                for (command_index, command) in commands.iter().enumerate() {
                    for (arg_index, arg) in command.iter().enumerate() {
                        validate_definition_string(
                            format!("steps[{command_index}][{arg_index}]"),
                            arg,
                        )?;
                    }
                }
            }
        }
    }
    for (key, value) in &target.env {
        validate_definition_string(format!("env key {key:?}"), key)?;
        validate_definition_string(format!("env[{key:?}]"), value)?;
    }
    Ok(())
}

impl StoredTarget {
    pub(crate) fn into_exec(self) -> Result<(Option<String>, ExecPayload)> {
        if let Some(s) = self.script {
            Ok((self.dir, ExecPayload::Script { script: s }))
        } else if let Some(steps) = self.steps {
            Ok((
                self.dir,
                ExecPayload::Steps {
                    steps,
                    env: self.env,
                },
            ))
        } else {
            bail!("ターゲットに script も steps もありません")
        }
    }
}

pub(crate) type TargetMap = std::collections::HashMap<String, StoredTarget>;

pub(crate) const CONFIG_DIR_ENV: &str = "CLIPWIRE_CONFIG_DIR";

pub(crate) fn clipwire_config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(CONFIG_DIR_ENV) {
        return PathBuf::from(path);
    }
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("clipwire")
}

/// Keep test/dev servers using different stores from contending with the
/// production singleton. FNV-1a is used only to make a compact, stable name;
/// this is not a security boundary.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn singleton_mutex_name(config_dir: &Path) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in config_dir.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("Global\\clipwire_singleton_{hash:016x}")
}

pub(crate) fn load_target_map(path: &Path) -> Result<TargetMap> {
    if !path.exists() {
        return Ok(TargetMap::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

/// `load_target_map` を呼び、失敗（ファイルは存在するが読めない/壊れている）
/// 場合は空マップにフォールバックしつつ**必ず警告ログを残す**。
/// 黙って空マップ扱いにすると、破損に気づかないまま
/// 「登録した全ターゲットが消えた」ように見えてしまう
/// （サーバー側呼び出し元専用。`unwrap_or_default()` を直接使わないこと）。
pub(crate) fn load_target_map_or_warn(path: &Path) -> TargetMap {
    match load_target_map(path) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "{} の読み込みに失敗しました（空として扱います）: {e:#}",
                path.display()
            );
            TargetMap::default()
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn save_target_map(path: &Path, map: &TargetMap) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, toml::to_string(map)?)?;
    Ok(())
}

#[derive(serde::Deserialize)]
pub(crate) struct TargetsFile {
    pub(crate) targets: std::collections::HashMap<String, StoredTarget>,
}

pub(crate) fn load_exec_target(name: &str) -> Result<StoredTarget> {
    let path = clipwire_config_dir().join("targets.toml");
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("設定ファイルが見つかりません: {}", path.display()))?;
    let file: TargetsFile = toml::from_str(&src)?;
    file.targets
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .with_context(|| format!("ターゲット '{}' が定義されていません", name))
}

pub(crate) fn validate_target_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid_first = bytes.first().is_some_and(|b| b.is_ascii_alphanumeric());
    let valid_rest = bytes
        .iter()
        .skip(1)
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if bytes.len() <= 64 && valid_first && valid_rest && !name.contains("..") {
        Ok(())
    } else {
        bail!("無効なターゲット名です: ASCII英数字で始まる1〜64文字の英数字・'.'・'_'・'-'を指定してください ('..' は使用不可)")
    }
}

pub(crate) fn invalid_stored_target_names(config_dir: &Path) -> Vec<(PathBuf, String)> {
    ["registered.toml", "pending.toml"]
        .into_iter()
        .filter_map(|file| {
            let path = config_dir.join(file);
            load_target_map(&path).ok().map(|map| (path, map))
        })
        .flat_map(|(path, map)| {
            map.into_keys()
                .filter(|name| validate_target_name(name).is_err())
                .map(move |name| (path.clone(), name))
        })
        .collect()
}

pub(crate) fn warn_invalid_stored_target_names(config_dir: &Path) {
    for (path, name) in invalid_stored_target_names(config_dir) {
        warn!(
            "{} に無効なターゲット名が存在します: {:?}",
            path.display(),
            name
        );
    }
}

// Production callers are Windows-only; Linux keeps this available for L tests.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\u{9}' | '\u{a}' | '\u{d}')
            || ('\u{20}'..='\u{d7ff}').contains(&ch)
            || ('\u{e000}'..='\u{fffd}').contains(&ch)
            || ('\u{10000}'..='\u{10ffff}').contains(&ch)
        {
            match ch {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '"' => escaped.push_str("&quot;"),
                '\'' => escaped.push_str("&apos;"),
                _ => escaped.push(ch),
            }
        }
    }
    escaped
}
