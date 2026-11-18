//! Types shared by every Foundry crate. No I/O.

pub mod ids;
pub mod messages;
pub mod record;
pub mod settings;
pub mod state;

/// Serializes `value` to JSON, reads it back and checks nothing changed.
#[cfg(test)]
pub(crate) fn assert_round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serializes");
    let back: T = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(&back, value, "round trip through {json}");
}

/// Reads `json` as `T` and returns the error text, failing if it parses.
#[cfg(test)]
pub(crate) fn parse_error<T>(json: &str) -> String
where
    T: serde::de::DeserializeOwned + std::fmt::Debug,
{
    serde_json::from_str::<T>(json)
        .expect_err("should not parse")
        .to_string()
}
