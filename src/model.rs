use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub group: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Inventory {
    pub hostname: String,
    pub system: Vec<Spec>,
    pub metrics: Vec<Metric>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub synthetic: bool,
    /// Current capability/read status; never part of persistent catalog identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub availability: BTreeMap<String, Availability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Availability {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub sequence: i64,
    pub timestamp_ms: i64,
    pub values: BTreeMap<String, Option<f64>>,
    pub persisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub revision: u64,
    pub sampling_seconds: u64,
    pub retention_days: u64,
    pub p100_fan_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            sampling_seconds: 10,
            retention_days: 30,
            p100_fan_id: None,
        }
    }
}
