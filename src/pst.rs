//! Program Status Protocol (OSC 7501): o painel diz ao Rex o que está pronto, falhou ou pede
//! atenção. O Rex mostra um indicador na aba do painel; os registros `done`/`error` sobrevivem
//! ao fim do processo. https://www.superlogical.com/rex/docs/build/program-status

use crate::locks::fnv32;
use crate::sessions::Sessions;
use crate::state::{Phase, PrKey, PrRecord, Remote, State};
use std::collections::BTreeMap;

pub const APP: &str = "reviewq";
const TITLE_MAX: usize = 192;
const MSG_MAX: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl PState {
    fn as_str(self) -> &'static str {
        match self {
            PState::Idle => "idle",
            PState::Working => "working",
            PState::Done => "done",
            PState::Blocked => "blocked",
            PState::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub state: PState,
    pub title: String,
    pub msg: String,
}

pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            o.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    o
}

fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Sem controles (C0, DEL, C1) nem marcas bidi, cortado em `max` bytes numa fronteira de
/// caractere. Um report com controle é descartado inteiro pelo terminal.
pub fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s
        .chars()
        .map(|c| if c.is_control() || is_bidi(c) { ' ' } else { c })
    {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

pub fn report(id: Option<&str>, r: &Record) -> String {
    let mut pairs = vec![];
    if let Some(id) = id {
        pairs.push(format!("id={id}"));
    }
    pairs.push(format!("state={}", r.state.as_str()));
    if r.state == PState::Blocked {
        pairs.push("kind=question".into());
    }
    pairs.push(format!("app={APP}"));
    let title = sanitize(&r.title, TITLE_MAX);
    if !title.is_empty() {
        pairs.push(format!("title={}", base64(title.as_bytes())));
    }
    let msg = sanitize(&r.msg, MSG_MAX);
    if !msg.is_empty() {
        pairs.push(format!("msg={}", base64(msg.as_bytes())));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

/// Apaga o registro `id` e os filhos dele; sem `id`, apaga tudo do terminal.
pub fn clear(id: Option<&str>) -> String {
    match id {
        Some(id) => format!("\x1b]7501;id={id}:state=clear\x1b\\"),
        None => "\x1b]7501;state=clear\x1b\\".into(),
    }
}

/// Raiz em `error` com o motivo, para o TUI escrever antes de sair (erro ou panic).
pub fn exit_error(msg: &str) -> String {
    report(
        None,
        &Record {
            state: PState::Error,
            title: APP.into(),
            msg: msg.into(),
        },
    )
}

/// `pr-<n>-<hash do repo>`: segmento válido do protocolo (`[A-Za-z0-9_.+-]{1,32}`).
pub fn pr_id(key: &PrKey) -> String {
    format!("pr-{}-{:08x}", key.number, fnv32(&key.repo))
}

/// Condições que deixam a raiz em `error`, calculadas do estado atual (não do histórico de
/// alertas, que só cresce). A ordem decide qual aparece no `msg`.
pub fn active_conditions(state: &State, sessions_corrupt: bool) -> Vec<String> {
    let mut out = vec![];
    if state.auth_error_notified {
        out.push("gh is not authenticated: run `gh auth login` on this machine".into());
    }
    for (repo, st) in &state.repos {
        if !st.ok && st.error.is_some() {
            out.push(format!("GitHub out of sync for {repo}"));
        }
    }
    if sessions_corrupt {
        out.push("sessions.json is corrupt and was moved aside".into());
    }
    out
}

pub fn root_for(conditions: &[String]) -> Record {
    match conditions.first() {
        Some(m) => Record {
            state: PState::Error,
            title: APP.into(),
            msg: m.clone(),
        },
        None => Record {
            state: PState::Idle,
            title: APP.into(),
            msg: String::new(),
        },
    }
}

fn pr_record(rec: &PrRecord, sessions: &Sessions) -> Option<Record> {
    if rec.remote != Remote::Pending || sessions.entered(&rec.key) || !rec.path.exists() {
        return None;
    }
    let title = format!("#{} {}", rec.key.number, rec.key.repo);
    let (state, msg) = match &rec.phase {
        Phase::Failed { step, .. } => (PState::Error, format!("Setup failed: {step}")),
        Phase::Blocked { reason } => (PState::Blocked, reason.clone()),
        Phase::Creating | Phase::Preparing => {
            (PState::Working, format!("Preparing: {}", rec.title))
        }
        Phase::Ready => (PState::Done, format!("Review ready: {}", rec.title)),
        Phase::Removing => return None,
    };
    Some(Record { state, title, msg })
}

/// Um registro por PR da fila que ainda pede algo do autor. PR fora do mapa = `clear`.
pub fn records_for(state: &State, sessions: &Sessions) -> BTreeMap<String, Record> {
    state
        .prs
        .values()
        .filter_map(|r| Some((pr_id(&r.key), pr_record(r, sessions)?)))
        .collect()
}

/// O que escrever no terminal para passar de `old` para `new`: só o que mudou.
pub fn diff(old: &BTreeMap<String, Record>, new: &BTreeMap<String, Record>) -> Vec<String> {
    let mut out: Vec<String> = old
        .keys()
        .filter(|k| !new.contains_key(*k))
        .map(|k| clear(Some(k)))
        .collect();
    out.extend(
        new.iter()
            .filter(|(k, r)| old.get(*k) != Some(*r))
            .map(|(k, r)| report(Some(k), r)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionEntry;
    use crate::state::{Ownership, RepoStatus};
    use chrono::Utc;

    fn rec(n: u64, phase: Phase, path: &std::path::Path) -> PrRecord {
        PrRecord {
            key: PrKey::new("o/r", n),
            title: format!("PR {n}"),
            author: "a".into(),
            url: String::new(),
            head_ref: "b".into(),
            base_ref: String::new(),
            synced_base: None,
            remote: Remote::Pending,
            ownership: Ownership::Managed,
            phase,
            path: path.into(),
            created_branch: true,
            workspace_id: None,
            observed_head_sha: "x".into(),
            managed_sha: None,
            prepared_sha: None,
            generation: 0,
            first_seen_at: Utc::now(),
            last_request_event_at: None,
            notify_pending: false,
            warning: None,
            not_pending_since: None,
            ci: None,
            work_started_at: None,
            work_agent: None,
        }
    }

    fn decode(s: &str) -> String {
        // inverso mínimo do base64 só para conferir os testes
        const T: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bits: Vec<u32> = s
            .trim_end_matches('=')
            .chars()
            .map(|c| T.find(c).unwrap() as u32)
            .collect();
        let mut out = vec![];
        for c in bits.chunks(4) {
            let n = c
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, v)| n | v << (18 - 6 * i));
            for i in 0..c.len().saturating_sub(1) {
                out.push((n >> (16 - 8 * i) & 0xff) as u8);
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64(b"url"), "dXJs");
        assert_eq!(base64(b"ur"), "dXI=");
        assert_eq!(base64(b"u"), "dQ==");
        assert_eq!(base64(b""), "");
        assert_eq!(
            decode(&base64("painel caiu: teste".as_bytes())),
            "painel caiu: teste"
        );
    }

    #[test]
    fn report_bytes_follow_the_protocol() {
        let r = Record {
            state: PState::Done,
            title: "#7 o/r".into(),
            msg: "Review ready: x".into(),
        };
        let s = report(Some("pr-7-00000000"), &r);
        assert!(
            s.starts_with("\x1b]7501;id=pr-7-00000000:state=done:app=reviewq:title="),
            "{s:?}"
        );
        assert!(s.ends_with("\x1b\\"));
        assert!(!s.contains("kind="));
        let b = report(
            None,
            &Record {
                state: PState::Blocked,
                title: String::new(),
                msg: "why".into(),
            },
        );
        assert!(
            b.contains("state=blocked:kind=question:app=reviewq:msg="),
            "{b:?}"
        );
        assert!(!b.contains("title="), "title vazio não vai");
        assert_eq!(clear(None), "\x1b]7501;state=clear\x1b\\");
        assert_eq!(
            clear(Some("pr-1-a")),
            "\x1b]7501;id=pr-1-a:state=clear\x1b\\"
        );
        let e = exit_error("boom");
        assert!(e.contains("state=error") && !e.contains("id="));
    }

    #[test]
    fn sanitize_strips_controls_and_bidi_and_cuts_on_char_boundaries() {
        assert_eq!(sanitize("a\nb\tc\u{7f}d\u{202E}e", 100), "a b c d e");
        assert_eq!(sanitize("ação", 3), "aç");
        assert_eq!(sanitize("ação", 2), "a", "não corta no meio do ç");
        assert_eq!(sanitize(&"x".repeat(300), TITLE_MAX).len(), TITLE_MAX);
        let long = report(
            None,
            &Record {
                state: PState::Error,
                title: "t".repeat(500),
                msg: "m".repeat(5000),
            },
        );
        let field = |k: &str| {
            long.split(':')
                .find_map(|p| p.strip_prefix(k))
                .unwrap()
                .trim_end_matches("\x1b\\")
                .to_string()
        };
        assert_eq!(decode(&field("title=")).len(), TITLE_MAX);
        assert_eq!(decode(&field("msg=")).len(), MSG_MAX);
    }

    #[test]
    fn ids_are_valid_segments_and_differ_by_repo() {
        let a = pr_id(&PrKey::new("org/one", 12));
        let b = pr_id(&PrKey::new("org/two", 12));
        assert_ne!(a, b);
        assert!(
            a.len() <= 32
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c)),
            "{a}"
        );
    }

    #[test]
    fn phases_map_to_records_only_while_the_pr_needs_the_author() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        let mut s = State::default();
        s.insert(rec(1, Phase::Ready, wt));
        s.insert(rec(2, Phase::Preparing, wt));
        s.insert(rec(
            3,
            Phase::Failed {
                step: "bundle install".into(),
                reason: "x".into(),
            },
            wt,
        ));
        s.insert(rec(
            4,
            Phase::Blocked {
                reason: "removal canceled: dirty".into(),
            },
            wt,
        ));
        s.insert(rec(5, Phase::Removing, wt));
        let mut leaving = rec(6, Phase::Ready, wt);
        leaving.remote = Remote::NotPending;
        s.insert(leaving);
        s.insert(rec(7, Phase::Ready, &wt.join("sumiu")));
        let mut entered = Sessions::default();
        let r = records_for(&s, &entered);
        let st = |n| r.get(&pr_id(&PrKey::new("o/r", n))).map(|x| x.state);
        assert_eq!(
            (st(1), st(2), st(3), st(4)),
            (
                Some(PState::Done),
                Some(PState::Working),
                Some(PState::Error),
                Some(PState::Blocked)
            )
        );
        assert_eq!(
            (st(5), st(6), st(7)),
            (None, None, None),
            "removendo, fora da fila ou sem worktree: nada"
        );
        assert_eq!(
            r[&pr_id(&PrKey::new("o/r", 3))].msg,
            "Setup failed: bundle install"
        );
        entered.prs.insert(
            "o/r#1".into(),
            SessionEntry {
                session_id: "s".into(),
                shell_block: "b".into(),
                created_at: Utc::now(),
                entered_at: Some(Utc::now()),
            },
        );
        assert_eq!(
            records_for(&s, &entered).get(&pr_id(&PrKey::new("o/r", 1))),
            None,
            "entrou: o aviso some"
        );
    }

    #[test]
    fn diff_emits_only_changes_and_clears_what_left() {
        let r = |st| Record {
            state: st,
            title: "t".into(),
            msg: "m".into(),
        };
        let old = BTreeMap::from([
            ("a".to_string(), r(PState::Working)),
            ("b".to_string(), r(PState::Done)),
        ]);
        let new = BTreeMap::from([
            ("a".to_string(), r(PState::Done)),
            ("c".to_string(), r(PState::Error)),
        ]);
        let out = diff(&old, &new);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&clear(Some("b"))));
        assert!(out.iter().any(|s| s.contains("id=a:state=done")));
        assert!(out.iter().any(|s| s.contains("id=c:state=error")));
        assert!(diff(&new, &new).is_empty(), "nada mudou, nada a escrever");
    }

    #[test]
    fn root_is_error_only_while_a_condition_is_active() {
        let mut s = State::default();
        assert_eq!(root_for(&active_conditions(&s, false)).state, PState::Idle);
        s.push_alert("branch preserved".into());
        assert_eq!(
            root_for(&active_conditions(&s, false)).state,
            PState::Idle,
            "alerta histórico não acende a raiz"
        );
        s.repos.insert(
            "o/r".into(),
            RepoStatus {
                ok: false,
                error: Some("boom".into()),
                ..Default::default()
            },
        );
        s.repos.insert("o/new".into(), RepoStatus::default());
        assert_eq!(
            active_conditions(&s, false),
            vec!["GitHub out of sync for o/r".to_string()],
            "repo nunca sincronizado não é falha"
        );
        s.auth_error_notified = true;
        let c = active_conditions(&s, true);
        assert!(c[0].starts_with("gh is not authenticated"));
        assert_eq!(c.len(), 3);
        let root = root_for(&c);
        assert_eq!(
            (root.state, root.msg.as_str()),
            (PState::Error, c[0].as_str())
        );
        s.auth_error_notified = false;
        s.repos.clear();
        assert_eq!(root_for(&active_conditions(&s, false)).state, PState::Idle);
    }
}
