// j -- JSON for records, files and the command line's --json output.  The mapping itself is
// JFLib's (agent_core::jmap: Field, Json, record!, wire_enum!): missing or mistyped members read
// as their default and unknown members are ignored, so old and new programs read each other.

use alloc::string::String;

pub use agent_core::jmap::{from_list, list, obj, str_of, u64_of, Field, Json, Value};
pub use agent_core::{record, wire_enum};

/// the limits of everything read here: 8 MiB, 200 000 values
pub fn big() -> agent_core::json::Limits {
    agent_core::json::Limits { bytes: 8 << 20, depth: 32, values: 200_000, string_bytes: 2 << 20, members: 100_000 }
}

/// the encoded text of a value on one line (empty on a limit violation)
pub fn text(v: &Value) -> String {
    agent_core::json::stringify_with_limits(v, big()).unwrap_or_default()
}

/// the encoded text of a value for people: indented, one member per line
pub fn pretty(v: &Value) -> String {
    agent_core::json::stringify_pretty(v, big(), 2).unwrap_or_default()
}

/// parse (None: not JSON or over the limits)
pub fn parse(bytes: &[u8]) -> Option<Value> {
    agent_core::json::parse_with_limits(bytes, big()).ok()
}

/// a record read from a file (None: missing, unreadable or not this record)
pub fn load<T: Json>(path: &str) -> Option<T> {
    T::from_json(&parse(&plat::read_file(path)?)?)
}

/// a record written to a file only its owner can read, replaced in one step
pub fn save<T: Json>(path: &str, v: &T) -> bool {
    // a record beyond the limits has no text: the file as it is stays rather than an empty one
    let text = pretty(&v.to_json());
    let ok = !text.is_empty() && plat::fs::write_file_private(path, text.as_bytes());
    if ok {
        crate::util::share_with_user(path);
    }
    ok
}

/// a string value
pub fn s(v: &str) -> Value {
    Value::string(v)
}

/// an unsigned value
pub fn n(v: u64) -> Value {
    Value::u64(v)
}

/// an array of strings
pub fn strs<S: AsRef<str>>(v: &[S]) -> Value {
    Value::Array(v.iter().map(|x| Value::string(x.as_ref())).collect())
}

/// a boolean member of an object (false when missing)
pub fn bool_of(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}
