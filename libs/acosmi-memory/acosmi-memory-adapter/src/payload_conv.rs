use std::collections::HashMap;

use acosmi_segment::types::Payload;

/// Convert indexer fields into an acosmi-segment payload wrapper.
pub fn fields_to_payload(fields: HashMap<String, serde_json::Value>) -> Payload {
    let mut map = serde_json::Map::with_capacity(fields.len());
    for (key, value) in fields {
        map.insert(key, value);
    }
    Payload::from(map)
}
