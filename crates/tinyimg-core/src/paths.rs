//! Native paths stay native for filesystem operations. JSON strings cover normal
//! Unicode paths; an explicit object preserves the raw bytes/code units otherwise.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct RawPath {
    encoding: &'static str,
    hex: String,
    display: String,
}
fn raw_path(path: &Path) -> RawPath {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        RawPath {
            encoding: "unix-bytes-hex",
            hex: path
                .as_os_str()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            display: path.to_string_lossy().into_owned(),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        RawPath {
            encoding: "windows-utf16-hex",
            hex: path
                .as_os_str()
                .encode_wide()
                .map(|v| format!("{v:04x}"))
                .collect(),
            display: path.to_string_lossy().into_owned(),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        RawPath {
            encoding: "platform-display",
            hex: String::new(),
            display: path.to_string_lossy().into_owned(),
        }
    }
}
pub fn serialize_path<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    if let Some(text) = path.to_str() {
        serializer.serialize_str(text)
    } else {
        raw_path(path).serialize(serializer)
    }
}
pub fn serialize_optional_path<S: Serializer>(
    path: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match path {
        Some(p) => serialize_path(p, serializer),
        None => serializer.serialize_none(),
    }
}
/// Read the report path representation on its native operating system.
pub fn deserialize_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    if let Some(text) = value.as_str() {
        return Ok(PathBuf::from(text));
    }
    let encoding = value
        .get("encoding")
        .and_then(|v| v.as_str())
        .ok_or_else(|| serde::de::Error::custom("path must be a string or raw-path object"))?;
    let hex = value
        .get("hex")
        .and_then(|v| v.as_str())
        .ok_or_else(|| serde::de::Error::custom("raw path requires hex"))?;
    if !hex.is_ascii() {
        return Err(serde::de::Error::custom("path hex must be ASCII"));
    }
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        if encoding != "unix-bytes-hex" || !hex.len().is_multiple_of(2) {
            return Err(serde::de::Error::custom(
                "invalid or non-native raw-path encoding",
            ));
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        if encoding != "windows-utf16-hex" || !hex.len().is_multiple_of(4) {
            return Err(serde::de::Error::custom(
                "invalid or non-native raw-path encoding",
            ));
        }
        let units = (0..hex.len())
            .step_by(4)
            .map(|i| u16::from_str_radix(&hex[i..i + 4], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(PathBuf::from(OsString::from_wide(&units)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (encoding, hex);
        Err(serde::de::Error::custom(
            "raw paths unsupported on this platform",
        ))
    }
}

/// Stable grouping label for an absolute native path; this is never used to open
/// a file. The raw marker cannot be a valid absolute Unicode path on Unix/Windows.
pub fn path_key(path: &Path) -> String {
    if let Some(text) = path.to_str() {
        #[cfg(windows)]
        {
            text.replace('\\', "/")
        }
        #[cfg(not(windows))]
        {
            text.to_owned()
        }
    } else {
        let raw = raw_path(path);
        format!("{}:{}", raw.encoding, raw.hex)
    }
}
/// Resolve a user-selected output/backup root without creating it. Existing
/// system aliases (notably macOS /var and /tmp) are resolved once. The writer
/// subsequently rejects symlinks inside the resulting output tree.
pub fn resolve_selected_root(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut result = std::fs::canonicalize(ancestor)?;
                for name in suffix.iter().rev() {
                    result.push(name);
                }
                return Ok(result);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| {
                            anyhow::anyhow!("cannot resolve output root {}", path.display())
                        })?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("output path has no existing ancestor"))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Serialize, Deserialize)]
    struct Example {
        #[serde(
            serialize_with = "serialize_path",
            deserialize_with = "deserialize_path"
        )]
        path: PathBuf,
    }
    #[test]
    fn unicode_paths_remain_strings() {
        let value = serde_json::to_value(Example {
            path: PathBuf::from("图片 空格.png"),
        })
        .unwrap();
        assert_eq!(value["path"], "图片 空格.png");
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_is_losslessly_serialized() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let p = PathBuf::from(OsString::from_vec(vec![b'/', b'x', 0xff]));
        let value = serde_json::to_value(Example { path: p.clone() }).unwrap();
        assert_eq!(value["path"]["encoding"], "unix-bytes-hex");
        assert_eq!(value["path"]["hex"], "2f78ff");
        assert_eq!(path_key(&p), "unix-bytes-hex:2f78ff");
        let roundtrip: Example = serde_json::from_value(value).unwrap();
        assert_eq!(roundtrip.path, p);
    }
    #[cfg(windows)]
    #[test]
    fn unpaired_utf16_is_losslessly_serialized() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let p = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]));
        let value = serde_json::to_value(Example { path: p }).unwrap();
        assert_eq!(value["path"]["encoding"], "windows-utf16-hex");
        assert_eq!(value["path"]["hex"], "0043003a005cd800");
    }
}
