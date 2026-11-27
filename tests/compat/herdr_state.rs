// Cópia congelada e mínima do `state.json` como o herdr-reviewq lê (commit 96c42eb): só os
// campos obrigatórios, sem `deny_unknown_fields`, como lá. Serve ao rollback: um estado gravado
// pelo rrq precisa continuar legível pelo daemon antigo. Não mude junto com `src/state.rs`.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct PrKey {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Remote {
    Pending,
    NotPending,
    Unknown,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ownership {
    Managed,
    Adopted { reason: String, at: DateTime<Utc> },
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    Creating,
    Preparing,
    Ready,
    Failed { step: String, reason: String },
    Removing,
    Blocked { reason: String },
}

#[derive(Debug, Deserialize)]
pub struct PrRecord {
    pub key: PrKey,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_ref: String,
    pub remote: Remote,
    pub ownership: Ownership,
    pub phase: Phase,
    pub path: PathBuf,
    pub observed_head_sha: String,
    pub first_seen_at: DateTime<Utc>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub notify_pending: bool,
}

#[derive(Debug, Deserialize)]
pub struct State {
    #[serde(default)]
    pub prs: BTreeMap<String, PrRecord>,
    #[serde(default)]
    pub auth_error_notified: bool,
}
