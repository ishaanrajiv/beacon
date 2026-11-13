//! The item, as every route that returns notifications sends it (`spec/api.md`, "The item").
//! The store builds it from its rows; the server serializes it as it is.

use crate::lifecycle::{Button, Times};
use crate::model::{Class, Icon, ItemState, PushLevel, PushReason, PushStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub version: i64,
    pub kind: String,
    pub class: Class,
    pub source: String,
    pub source_name: String,
    pub icon: Icon,
    pub key: String,
    pub title: String,
    pub body: String,
    pub preview: Option<String>,
    pub url: Option<String>,
    pub context: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub created: i64,
    pub updated: i64,
    pub state: Option<ItemState>,
    pub times: Option<Times>,
    pub alert: Option<AlertInfo>,
    /// The buttons that apply now.
    pub actions: Vec<Button>,
    pub seen: bool,
    pub collapse_id: String,
    pub push: Push,
}

/// An alert's flap and repeat state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertInfo {
    /// Openings within the flap window.
    pub openings: u32,
    pub flapping: bool,
    pub one_off: bool,
    pub repeats_sent: u32,
    pub next_repeat: Option<i64>,
}

/// What happened to the item's push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Push {
    pub level: PushLevel,
    pub status: PushStatus,
    pub reason: Option<PushReason>,
    pub at: Option<i64>,
    pub release_at: Option<i64>,
    pub covered_by: Option<i64>,
}
