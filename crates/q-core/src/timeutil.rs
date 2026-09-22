use serde::{Deserialize, Deserializer, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::QueueError;

pub fn format_timestamp(ts: OffsetDateTime) -> String {
    let ts = ts.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        ts.year(),
        ts.month() as u8,
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second()
    )
}

pub fn parse_timestamp(value: &str) -> Result<OffsetDateTime, QueueError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|ts| ts.to_offset(UtcOffset::UTC))
        .map_err(|err| QueueError::Database(format!("invalid timestamp {value}: {err}")))
}

pub mod ts {
    use super::*;

    pub fn serialize<S>(dt: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format_timestamp(*dt))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_timestamp(&value).map_err(serde::de::Error::custom)
    }
}
