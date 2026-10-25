//! Following declared changes: the Task fields the Forge controller reads
//! and owns.
//!
//! - `spec.forge` ([`ForgeSpec`]): a Task's overrides of the `[forge]`
//!   relay and draft defaults; mutable like `dispatch`.
//! - `status.declared`: facts the worker declared through `fleet declare`,
//!   each a JSON object with a string `kind`; the controller reads the
//!   `ChangeRequest` ones ([`Declaration`]) and ignores the rest.
//! - `status.forge` ([`ForgeStatus`]): one entry per followed change, with
//!   the cursor the relay went through, and this controller's conditions.
//!
//! The worker mark, `<!-- fleet:worker <task uid> -->`, is how a worker's
//! own words on the forge are told from a person's on the same login and
//! from another worker's.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::condition::Condition;
use crate::meta::Millis;
use crate::plan::{Intent, Receipt};

/// The `kind` of a declaration this controller follows.
pub const DECLARED_CHANGE_REQUEST: &str = "ChangeRequest";

/// The mark a worker ends every forge post with, around its Task's `uid`.
pub const WORKER_MARK_PREFIX: &str = "<!-- fleet:worker ";
pub const WORKER_MARK_SUFFIX: &str = " -->";

/// `<!-- fleet:worker <uid> -->`.
pub fn worker_mark(uid: &str) -> String {
    format!("{WORKER_MARK_PREFIX}{uid}{WORKER_MARK_SUFFIX}")
}

/// The Task `uid` of the first worker mark in a body, if any.
pub fn marked_by(body: &str) -> Option<&str> {
    let start = body.find(WORKER_MARK_PREFIX)? + WORKER_MARK_PREFIX.len();
    let rest = &body[start..];
    let end = rest.find(WORKER_MARK_SUFFIX)?;
    let uid = rest[..end].trim();
    (!uid.is_empty() && !uid.contains(char::is_whitespace)).then_some(uid)
}

/// The `[forge]` relay defaults a Task's `spec.forge` may override, handed
/// to the controller as values: it reads no configuration itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeDefaults {
    pub relay_notes: bool,
    pub relay_checks: bool,
    pub relay_bots: bool,
}

impl Default for ForgeDefaults {
    fn default() -> Self {
        ForgeDefaults {
            relay_notes: true,
            relay_checks: true,
            relay_bots: false,
        }
    }
}

/// `spec.forge`: each field present overrides the matching `[forge]`
/// default; a field absent inherits it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelaySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelaySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<bool>,
}

/// `spec.forge.draft`. Stored by this step and acted on by the mark-ready
/// step: `policy` absent inherits `forge.draftPolicy`, `null` disables
/// mark-ready for this Task, a name selects a policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftSpec {
    /// `None`: absent. `Some(None)`: an explicit `null`. `Some(Some(name))`.
    #[serde(
        default,
        deserialize_with = "explicit_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub policy: Option<Option<String>>,
}

/// Keep an explicit `null` apart from an absent field.
fn explicit_null<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

impl ForgeSpec {
    pub fn relay_notes(&self, defaults: &ForgeDefaults) -> bool {
        self.relay
            .as_ref()
            .and_then(|r| r.notes)
            .unwrap_or(defaults.relay_notes)
    }

    pub fn relay_checks(&self, defaults: &ForgeDefaults) -> bool {
        self.relay
            .as_ref()
            .and_then(|r| r.checks)
            .unwrap_or(defaults.relay_checks)
    }
}

/// A `ChangeRequest` declaration, as `fleet declare --change-request`
/// writes it: `{"kind": "ChangeRequest", "url": …, "name": …}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub url: Option<String>,
    /// The ChangeRequest the verb ensured for it.
    pub name: String,
}

/// Every item is a JSON object with a non-empty string `kind`; nothing else
/// is validated, so other tools can declare their own facts.
pub fn validate_declared(items: &[Value]) -> Result<(), String> {
    for (i, item) in items.iter().enumerate() {
        validate_declaration(item).map_err(|why| format!("status.declared[{i}]: {why}"))?;
    }
    Ok(())
}

pub fn validate_declaration(item: &Value) -> Result<(), String> {
    let Some(map) = item.as_object() else {
        return Err("a declaration is a JSON object".into());
    };
    match map.get("kind").and_then(Value::as_str) {
        Some(k) if !k.trim().is_empty() => Ok(()),
        _ => Err("a declaration needs a string `kind`".into()),
    }
}

/// The `ChangeRequest` declarations that name their object, in order, each
/// name once.
pub fn declared_changes(items: &[Value]) -> Vec<Declaration> {
    let mut out: Vec<Declaration> = Vec::new();
    for item in items {
        if item.get("kind").and_then(Value::as_str) != Some(DECLARED_CHANGE_REQUEST) {
            continue;
        }
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        if out.iter().any(|d| d.name == name) {
            continue;
        }
        out.push(Declaration {
            url: item.get("url").and_then(Value::as_str).map(str::to_string),
            name: name.to_string(),
        });
    }
    out
}

/// `status.forge`: ForgeController's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgeStatus {
    /// One entry per followed change, by ChangeRequest name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changes: BTreeMap<String, ChangeEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

impl ForgeStatus {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.conditions.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    #[default]
    Following,
    Settled,
}

/// One followed change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeEntry {
    /// The ChangeRequest's `uid` when the entry was made or last rebound.
    pub uid: String,
    #[serde(default)]
    pub state: EntryState,
    /// The ChangeRequest `revision` relayed through.
    #[serde(default)]
    pub cursor: u64,
    /// How many times a relay found `droppedBefore` above the cursor.
    #[serde(default)]
    pub gaps: u64,
    /// The mark-ready record, owned by the mark-ready step; stored here
    /// untouched.
    #[serde(default)]
    pub mark_ready: MarkReady,
    /// Earliest next pass for this entry, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<Millis>,
}

impl ChangeEntry {
    /// A fresh entry at the present: following, cursor at `revision`.
    pub fn joined(uid: &str, revision: u64) -> ChangeEntry {
        ChangeEntry {
            uid: uid.to_string(),
            state: EntryState::Following,
            cursor: revision,
            gaps: 0,
            mark_ready: MarkReady::default(),
            not_before: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkReadyState {
    #[default]
    None,
    Pending,
    Done,
    Failed,
}

/// `{state, head, attempts, intents, receipts}`: the mark-ready record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkReady {
    #[serde(default)]
    pub state: MarkReadyState,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub intents: BTreeMap<String, Intent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub receipts: BTreeMap<String, Receipt>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_mark_names_a_task_uid() {
        let mark = worker_mark("u7");
        assert_eq!(mark, "<!-- fleet:worker u7 -->");
        assert_eq!(marked_by(&format!("done\n{mark}\n")), Some("u7"));
        assert_eq!(marked_by("<!-- fleet:worker -->"), None);
        assert_eq!(marked_by("no mark"), None);
    }

    #[test]
    fn an_explicit_null_policy_is_kept_apart_from_an_absent_one() {
        let absent: ForgeSpec = serde_json::from_value(json!({"draft": {}})).unwrap();
        assert_eq!(absent.draft.unwrap().policy, None);
        let disabled: ForgeSpec =
            serde_json::from_value(json!({"draft": {"policy": null}})).unwrap();
        assert_eq!(disabled.draft.as_ref().unwrap().policy, Some(None));
        assert_eq!(
            serde_json::to_value(&disabled).unwrap(),
            json!({"draft": {"policy": null}})
        );
        let named: ForgeSpec =
            serde_json::from_value(json!({"draft": {"policy": "checks"}})).unwrap();
        assert_eq!(
            named.draft.unwrap().policy,
            Some(Some("checks".to_string()))
        );
        assert!(
            serde_json::from_value::<ForgeSpec>(json!({"draft": {"ready": true}})).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn relay_fields_inherit_the_defaults_field_by_field() {
        let spec: ForgeSpec = serde_json::from_value(json!({"relay": {"notes": false}})).unwrap();
        let d = ForgeDefaults::default();
        assert!(!spec.relay_notes(&d));
        assert!(spec.relay_checks(&d));
        assert!(!ForgeSpec::default().relay_notes(&ForgeDefaults {
            relay_notes: false,
            ..d
        }));
    }

    #[test]
    fn declarations_need_a_kind_and_only_named_change_requests_are_followed() {
        assert!(validate_declared(&[json!({"kind": "ChangeRequest", "url": "u"})]).is_ok());
        assert!(validate_declared(&[json!({"kind": "Anything", "x": 1})]).is_ok());
        assert!(validate_declared(&[json!({"url": "u"})]).is_err());
        assert!(validate_declared(&[json!("ChangeRequest")]).is_err());
        assert!(validate_declared(&[json!({"kind": ""})]).is_err());
        let followed = declared_changes(&[
            json!({"kind": "Other", "name": "x"}),
            json!({"kind": "ChangeRequest", "url": "u1", "name": "cr-1"}),
            json!({"kind": "ChangeRequest", "url": "u1", "name": "cr-1"}),
            json!({"kind": "ChangeRequest", "url": "u2"}),
            json!({"kind": "ChangeRequest", "name": "cr-2"}),
        ]);
        assert_eq!(
            followed,
            vec![
                Declaration {
                    url: Some("u1".into()),
                    name: "cr-1".into()
                },
                Declaration {
                    url: None,
                    name: "cr-2".into()
                },
            ]
        );
    }

    #[test]
    fn an_entry_serialises_in_the_interface_pages_shape() {
        let e = ChangeEntry::joined("c1", 7);
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"uid": "c1", "state": "following", "cursor": 7, "gaps": 0,
                   "markReady": {"state": "none", "head": null, "attempts": 0}})
        );
        let st: ForgeStatus = serde_json::from_value(json!({})).unwrap();
        assert!(st.is_empty());
    }
}
