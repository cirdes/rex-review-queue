//! Início do trabalho: agentes (Claude/Codex) rodando dentro de uma worktree de review.

use crate::mux::{inspect_session, in_worktree, Mux, Process};
use crate::sessions::Sessions;
use crate::state::{PrKey, State};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::Instant;

const KNOWN: &[&str] = &["claude", "codex"];

/// Agente visto numa worktree pelo `scan`. `foreground_cwd` é um segundo caminho opcional que
/// também conta para achar a worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub agent: String,
    pub cwd: Option<PathBuf>,
    pub foreground_cwd: Option<PathBuf>,
}

/// O nome do agente se o processo for um: o `name`, o basename do `path` ou o `argv0` igual a
/// `claude` ou `codex`. O Claude instalado pelo instalador nativo roda como `versions/2.1.x`, com
/// `argv0 = claude`, por isso os três.
pub fn agent_name(p: &Process) -> Option<&'static str> {
    let base = p.path.as_deref().and_then(|x| x.file_name()).map(|n| n.to_string_lossy().into_owned());
    let names = [Some(p.name.as_str()), base.as_deref(), p.argv0.as_deref()];
    KNOWN.iter().copied().find(|k| names.contains(&Some(*k)))
}

/// Agentes em foreground nas sessões dos PRs ainda sem início. Só os PRs com sessão em
/// `sessions.json` são inspecionados; um erro numa sessão (fechada, Rex fora) só pula aquela.
/// Passado o `deadline` (o que sobrou do orçamento dos extras), não inspeciona mais nenhuma.
pub fn scan(mux: &dyn Mux, sessions: &Sessions, state: &State, deadline: Instant) -> Vec<AgentInfo> {
    let mut out = vec![];
    for rec in state.prs.values().filter(|r| r.work_started_at.is_none()) {
        let Some(entry) = sessions.get(&rec.key) else { continue };
        if Instant::now() >= deadline {
            break;
        }
        let Ok(blocks) = inspect_session(mux, &entry.session_id) else { continue };
        for p in blocks.into_iter().filter_map(|(_, p)| p) {
            if let Some(agent) = agent_name(&p).filter(|_| in_worktree(&p, &rec.path)) {
                out.push(AgentInfo { agent: agent.into(), cwd: p.cwd.clone(), foreground_cwd: None });
            }
        }
    }
    out
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Marca `work_started_at`/`work_agent` nos registros ainda sem início que têm um agente conhecido
/// na raiz do worktree ou abaixo dela. Uma vez marcado, não muda. Devolve os marcados agora.
pub fn apply_agents(state: &mut State, agents: &[AgentInfo], now: DateTime<Utc>) -> Vec<PrKey> {
    let mut known: Vec<(&str, Vec<PathBuf>)> = agents
        .iter()
        .filter(|a| KNOWN.contains(&a.agent.as_str()))
        .map(|a| (a.agent.as_str(), [&a.cwd, &a.foreground_cwd].into_iter().flatten().map(|p| canon(p)).collect()))
        .collect();
    // desempate determinístico quando há mais de um agente no mesmo worktree
    known.sort_by(|a, b| a.0.cmp(b.0));
    let mut marked = vec![];
    for rec in state.prs.values_mut().filter(|r| r.work_started_at.is_none()) {
        let root = canon(&rec.path);
        if let Some((agent, _)) = known.iter().find(|(_, ps)| ps.iter().any(|p| p.starts_with(&root))) {
            rec.work_started_at = Some(now);
            rec.work_agent = Some(agent.to_string());
            marked.push(rec.key.clone());
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fake::*;
    use crate::mux::MuxError;
    use crate::sessions::SessionEntry;
    use crate::state::{PrKey, PrRecord, State};
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        "2026-10-13T14:32:00Z".parse().unwrap()
    }

    fn agent(name: &str, cwd: &str) -> AgentInfo {
        AgentInfo { agent: name.into(), cwd: Some(PathBuf::from(cwd)), foreground_cwd: None }
    }

    fn state(recs: &[(u64, &str)]) -> State {
        let mut s = State::default();
        for (n, path) in recs {
            let mut r = PrRecord::fixture("o/r", *n, "h", "s");
            r.path = PathBuf::from(path);
            s.insert(r);
        }
        s
    }

    fn started(s: &State, n: u64) -> (Option<DateTime<Utc>>, Option<String>) {
        let r = s.get(&PrKey::new("o/r", n)).unwrap();
        (r.work_started_at, r.work_agent.clone())
    }

    #[test]
    fn marks_worktree_with_agent_in_root_or_subdir() {
        let mut s = state(&[(1, "/w/pr-1"), (2, "/w/pr-2"), (3, "/w/pr-3")]);
        let marked = apply_agents(&mut s, &[agent("claude", "/w/pr-1"), agent("codex", "/w/pr-2/app/models")], now());
        assert_eq!(marked, vec![PrKey::new("o/r", 1), PrKey::new("o/r", 2)]);
        assert_eq!(started(&s, 1), (Some(now()), Some("claude".into())));
        assert_eq!(started(&s, 2), (Some(now()), Some("codex".into())));
        assert_eq!(started(&s, 3), (None, None));
    }

    #[test]
    fn uses_foreground_cwd_too() {
        let mut s = state(&[(1, "/w/pr-1")]);
        let a = AgentInfo { agent: "claude".into(), cwd: Some("/home".into()), foreground_cwd: Some("/w/pr-1/lib".into()) };
        apply_agents(&mut s, &[a], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }

    #[test]
    fn similar_prefix_does_not_match() {
        let mut s = state(&[(157, "/w/pr-157")]);
        assert!(apply_agents(&mut s, &[agent("claude", "/w/pr-1579")], now()).is_empty());
        assert_eq!(started(&s, 157), (None, None));
    }

    #[test]
    fn matches_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("pr-1")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut s = state(&[(1, link.join("pr-1").to_str().unwrap())]);
        apply_agents(&mut s, &[agent("claude", real.join("pr-1").to_str().unwrap())], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }

    #[test]
    fn ignores_agents_outside_allowlist() {
        let mut s = state(&[(1, "/w/pr-1")]);
        assert!(apply_agents(&mut s, &[agent("pi", "/w/pr-1"), agent("gemini", "/w/pr-1")], now()).is_empty());
    }

    #[test]
    fn first_detection_is_stable() {
        let mut s = state(&[(1, "/w/pr-1")]);
        apply_agents(&mut s, &[agent("codex", "/w/pr-1")], now());
        let later = now() + chrono::Duration::hours(1);
        assert!(apply_agents(&mut s, &[agent("claude", "/w/pr-1")], later).is_empty());
        assert_eq!(started(&s, 1), (Some(now()), Some("codex".into())));
    }

    #[test]
    fn picks_alphabetical_agent_on_tie() {
        let mut s = state(&[(1, "/w/pr-1")]);
        apply_agents(&mut s, &[agent("codex", "/w/pr-1"), agent("claude", "/w/pr-1/x")], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }

    fn sessions_for(pairs: &[(u64, &str)]) -> Sessions {
        let mut s = Sessions::default();
        for (n, id) in pairs {
            let e = SessionEntry { session_id: (*id).into(), shell_block: "b".into(), created_at: now(), entered_at: None };
            s.prs.insert(PrKey::new("o/r", *n).to_string(), e);
        }
        s
    }

    fn fake_block(id: &str, p: Process) -> FakeBlock {
        FakeBlock { id: id.into(), label: None, process: Some(p), written: vec![] }
    }

    #[test]
    fn agent_name_matches_name_path_or_argv0_exactly() {
        let wt = std::path::Path::new("/w");
        assert_eq!(agent_name(&program_at("claude", wt)), Some("claude"));
        assert_eq!(agent_name(&program_at("codex", wt)), Some("codex"));
        // instalador nativo do Claude: o executável é a versão, o argv0 é `claude`
        let native = Process {
            pid: 3,
            name: "2.1.332".into(),
            path: Some("/h/.local/share/claude/versions/2.1.332".into()),
            argv0: Some("claude".into()),
            cwd: Some(wt.into()),
        };
        assert_eq!(agent_name(&native), Some("claude"));
        let by_path = Process { name: "node".into(), path: Some("/opt/bin/codex".into()), argv0: Some("node".into()), ..native.clone() };
        assert_eq!(agent_name(&by_path), Some("codex"));
        // `vim claude.md`: o argv completo não aparece, e nada é exatamente `claude`
        assert_eq!(agent_name(&program_at("vim", wt)), None);
        assert_eq!(agent_name(&Process { argv0: Some("claude-helper".into()), ..program_at("x", wt) }), None);
        assert_eq!(agent_name(&shell_at(wt)), None);
    }

    #[test]
    fn scan_finds_agents_inside_the_worktree_only_for_prs_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let wt = |n: u64| {
            let p = dir.path().join(format!("pr-{n}"));
            std::fs::create_dir_all(p.join("src")).unwrap();
            p
        };
        let (w1, w2, w3, w4) = (wt(1), wt(2), wt(3), wt(4));
        let mut s = state(&[(1, w1.to_str().unwrap()), (2, w2.to_str().unwrap()), (3, w3.to_str().unwrap()), (4, w4.to_str().unwrap())]);
        s.get_mut(&PrKey::new("o/r", 3)).unwrap().work_started_at = Some(now());
        let m = FakeMux::new();
        m.add_session("s1", "#1 r", vec![fake_block("b1", shell_at(&w1)), fake_block("b2", program_at("claude", &w1.join("src")))]);
        // claude aberto fora da worktree do PR 2 não conta
        m.add_session("s2", "#2 r", vec![fake_block("b3", program_at("claude", dir.path()))]);
        m.add_session("s3", "#3 r", vec![fake_block("b4", program_at("codex", &w3))]);
        // PR 4 sem sessão no mapa: nem é inspecionado
        let sessions = sessions_for(&[(1, "s1"), (2, "s2"), (3, "s3")]);
        let found = scan(&m, &sessions, &s, far());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].agent, "claude");
        assert_eq!(found[0].cwd.as_deref(), Some(w1.join("src").as_path()));
        let views = m.calls.borrow().iter().filter(|c| *c == "view").count();
        assert_eq!(views, 2, "o PR já marcado não é inspecionado");
        apply_agents(&mut s, &found, now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
        assert_eq!(started(&s, 2), (None, None));
    }

    #[test]
    fn scan_skips_sessions_that_fail() {
        let mut s = state(&[(1, "/w/pr-1")]);
        let m = FakeMux::new();
        let sessions = sessions_for(&[(1, "sumiu")]);
        assert!(scan(&m, &sessions, &s, far()).is_empty());
        m.add_session("sumiu", "#1 r", vec![fake_block("b", program_at("claude", std::path::Path::new("/w/pr-1")))]);
        m.fail_on("inspect_block", MuxError::Unreachable("x".into()));
        assert!(scan(&m, &sessions, &s, far()).is_empty());
        assert!(apply_agents(&mut s, &[], now()).is_empty());
    }

    fn far() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    #[test]
    fn scan_stops_when_the_budget_is_over() {
        let s = state(&[(1, "/w/pr-1")]);
        let m = FakeMux::new();
        m.add_session("s1", "#1 r", vec![fake_block("b", program_at("claude", std::path::Path::new("/w/pr-1")))]);
        let sessions = sessions_for(&[(1, "s1")]);
        assert!(scan(&m, &sessions, &s, std::time::Instant::now()).is_empty());
        assert!(m.calls.borrow().is_empty(), "sem orçamento, nem inspeciona");
    }
}
