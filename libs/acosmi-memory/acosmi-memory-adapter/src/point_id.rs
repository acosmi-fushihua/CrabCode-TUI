use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

/// Stable namespace for TS memory point IDs.
pub const NAMESPACE_MEMORY: Uuid = Uuid::from_u128(0x6f76_6b00_0000_0000_0000_0000_0000_0001);

/// Map scope + root-relative path without `.md` to a stable UUID5 point id.
pub fn scoped_path_to_point_id(scope: &str, relative_path_no_ext: &str) -> String {
    let normalized_path = relative_path_no_ext
        .replace('\\', "/")
        .trim_start_matches("./")
        .nfc()
        .collect::<String>();
    let key = format!("{scope}:{normalized_path}");
    Uuid::new_v5(&NAMESPACE_MEMORY, key.as_bytes()).to_string()
}
