//! Pure lexical validation and comparison for Windows write paths.
//!
//! This module deliberately does not use [`std::path::Path`]: on non-Windows
//! hosts it would interpret Windows paths using the host platform's rules.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowsPath {
    pub(crate) drive: Option<char>,
    pub(crate) components: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathError {
    InvalidEncoding,
    NotAbsolute,
    DotComponent,
    AlternateDataStream,
    TrailingDotOrSpace,
    ReservedDeviceName,
    UncOrDeviceNamespace,
}

/// Decodes and lexically validates an `X-Clipwire-Path` header value.
///
/// Percent escapes are decoded as bytes before UTF-8 and path validation.
/// Unlike `application/x-www-form-urlencoded`, `+` is a literal plus sign,
/// not an encoded space.
pub(crate) fn decode_and_validate_header_path(raw: &str) -> Result<WindowsPath, PathError> {
    let decoded = percent_decode(raw)?;
    parse_canonical(&decoded)
}

/// Applies the same lexical validation as [`decode_and_validate_header_path`]
/// to an already-decoded path (for example one returned by the OS).
pub(crate) fn parse_canonical(decoded: &str) -> Result<WindowsPath, PathError> {
    if decoded.contains('\0') {
        return Err(PathError::InvalidEncoding);
    }

    let (path, had_extended_prefix) = match decoded.strip_prefix(r"\\?\") {
        Some(path) => (path, true),
        None => (decoded, false),
    };

    if path.starts_with(r"\\") {
        return Err(PathError::UncOrDeviceNamespace);
    }
    if had_extended_prefix && starts_with_ascii_case_insensitive(path, r"UNC\") {
        return Err(PathError::UncOrDeviceNamespace);
    }

    let bytes = path.as_bytes();
    let is_drive_absolute =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !is_drive_absolute {
        return Err(if had_extended_prefix {
            PathError::UncOrDeviceNamespace
        } else {
            PathError::NotAbsolute
        });
    }

    let mut components = Vec::new();
    for component in path[3..].split('\\').filter(|part| !part.is_empty()) {
        if component == "." || component == ".." {
            return Err(PathError::DotComponent);
        }
        components.push(component.to_owned());
    }

    if let Some(file_name) = components.last() {
        validate_file_name(file_name)?;
    }

    Ok(WindowsPath {
        drive: path.chars().next(),
        components,
    })
}

/// Renders a validated drive path as a Windows absolute path string.
pub(crate) fn to_absolute_string(path: &WindowsPath) -> String {
    let drive = path
        .drive
        .expect("WindowsPath without a drive is not supported here");
    let mut absolute = format!("{drive}:\\");
    absolute.push_str(&path.components.join("\\"));
    absolute
}

/// Applies the path comparison folding used by this module.
///
/// Only ASCII `a` through `z` are changed. `str::to_uppercase` is unsuitable:
/// Unicode uppercase mappings can change length (for example, `ß` to `SS`) and
/// could therefore make distinct path components compare equal.
pub(crate) fn ordinal_uppercase(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase()
            } else {
                ch
            }
        })
        .collect()
}

/// Returns whether `path` is `root` itself or a descendant of it.
pub(crate) fn is_under(path: &WindowsPath, root: &WindowsPath) -> bool {
    if !drives_equal(path.drive, root.drive) || root.components.len() > path.components.len() {
        return false;
    }

    root.components
        .iter()
        .zip(&path.components)
        .all(|(root_component, path_component)| {
            ordinal_uppercase(root_component) == ordinal_uppercase(path_component)
        })
}

/// Returns whether either path is an ancestor of, equal to, or below the other.
pub(crate) fn overlaps(a: &WindowsPath, b: &WindowsPath) -> bool {
    is_under(a, b) || is_under(b, a)
}

fn percent_decode(raw: &str) -> Result<String, PathError> {
    let input = raw.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err(PathError::InvalidEncoding);
            }
            let high = hex_value(input[index + 1]).ok_or(PathError::InvalidEncoding)?;
            let low = hex_value(input[index + 2]).ok_or(PathError::InvalidEncoding)?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| PathError::InvalidEncoding)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_file_name(file_name: &str) -> Result<(), PathError> {
    if file_name.contains(':') {
        return Err(PathError::AlternateDataStream);
    }
    if file_name.ends_with(['.', ' ']) {
        return Err(PathError::TrailingDotOrSpace);
    }

    let stem = file_name.split('.').next().unwrap_or(file_name);
    let folded = ordinal_uppercase(stem);
    let numbered_device = matches!(
        folded.as_bytes(),
        [b'C', b'O', b'M', b'1'..=b'9'] | [b'L', b'P', b'T', b'1'..=b'9']
    );
    if matches!(
        folded.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered_device
    {
        return Err(PathError::ReservedDeviceName);
    }
    Ok(())
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn drives_equal(a: Option<char>, b: Option<char>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            ordinal_uppercase(&a.to_string()) == ordinal_uppercase(&b.to_string())
        }
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path: &str) -> WindowsPath {
        decode_and_validate_header_path(path).unwrap()
    }

    #[test]
    fn accepts_absolute_drive_path() {
        assert_eq!(
            parse(r"C:\work\pad2py\run.py"),
            WindowsPath {
                drive: Some('C'),
                components: vec!["work".into(), "pad2py".into(), "run.py".into()],
            }
        );
    }

    #[test]
    fn rejects_non_absolute_paths() {
        for path in [r"C:foo", r"\foo", "foo"] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::NotAbsolute)
            );
        }
    }

    #[test]
    fn rejects_dot_components_after_decoding() {
        for path in [r"C:\work\..\x", r"C:\work\.\x", r"C:\work\%2E%2E\x"] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::DotComponent)
            );
        }
    }

    #[test]
    fn rejects_alternate_data_stream_in_file_name() {
        assert_eq!(
            decode_and_validate_header_path(r"C:\work\run.py:hidden"),
            Err(PathError::AlternateDataStream)
        );
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        for path in [r"C:\work\run.py.", r"C:\work\run.py "] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::TrailingDotOrSpace)
            );
        }
    }

    #[test]
    fn rejects_reserved_device_names_without_overmatching() {
        for path in [r"C:\work\CON", r"C:\work\CON.txt", r"C:\work\con.TXT"] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::ReservedDeviceName)
            );
        }
        assert!(decode_and_validate_header_path(r"C:\work\CONSOLE.txt").is_ok());
    }

    #[test]
    fn rejects_unc_and_device_namespaces() {
        for path in [
            r"\\server\share\x",
            r"\\?\GLOBALROOT\x",
            r"\\.\pipe\x",
            r"\\?\UNC\server\share\x",
        ] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::UncOrDeviceNamespace)
            );
        }
    }

    #[test]
    fn accepts_extended_prefix_for_normal_drive_path() {
        assert_eq!(parse(r"\\?\C:\work\run.py"), parse(r"C:\work\run.py"));
    }

    #[test]
    fn under_comparison_is_component_based_and_includes_equality() {
        let root = parse(r"C:\work\pad2py");
        for (path, expected) in [
            (r"C:\work\pad2py\sub\file.py", true),
            (r"C:\work\pad2py-evil\file.py", false),
            (r"C:\work\pad2py", true),
            (r"C:\other", false),
        ] {
            assert_eq!(is_under(&parse(path), &root), expected, "{path}");
        }
    }

    #[test]
    fn comparisons_are_ascii_case_insensitive() {
        let ancestor = parse(r"C:\Work");
        let descendant = parse(r"c:\WORK\sub");
        assert!(is_under(&descendant, &ancestor));
        assert!(overlaps(&ancestor, &descendant));
        assert!(overlaps(&descendant, &ancestor));
    }

    #[test]
    fn ordinal_uppercase_only_changes_ascii_lowercase() {
        assert_eq!(ordinal_uppercase("abcXYZ123"), "ABCXYZ123");
        assert_eq!(ordinal_uppercase("ß"), "ß");
    }

    #[test]
    fn rejects_invalid_utf8_nul_and_bad_percent_escape() {
        for path in [r"C:\work\%FF", r"C:\work\a%00b", r"C:\work\%GG"] {
            assert_eq!(
                decode_and_validate_header_path(path),
                Err(PathError::InvalidEncoding),
                "{path}"
            );
        }
    }

    #[test]
    fn plus_is_not_decoded_as_space() {
        assert_eq!(
            parse(r"C:\work\a+b.txt").components.last().unwrap(),
            "a+b.txt"
        );
    }

    #[test]
    fn parse_canonical_validates_without_percent_decoding() {
        assert_eq!(
            parse_canonical(r"C:\work\run.py"),
            Ok(parse(r"C:\work\run.py"))
        );
        assert_eq!(
            parse_canonical(r"C:\work\..\run.py"),
            Err(PathError::DotComponent)
        );
        assert_eq!(
            parse_canonical(r"C:\work\%2E%2E\run.py")
                .unwrap()
                .components[1],
            "%2E%2E"
        );
    }

    #[test]
    fn renders_an_absolute_windows_path() {
        assert_eq!(
            to_absolute_string(&parse(r"C:\work\run.py")),
            r"C:\work\run.py"
        );
        assert_eq!(to_absolute_string(&parse(r"D:\")), r"D:\");
    }
}
