use crate::config::{Config, RepoConfig};
use crate::facts::{self, divergence, Divergence, Facts};
use crate::git::Git;
use crate::locks;
use crate::mux::{in_worktree, inspect_session, is_shell, Mux, MuxError};
use crate::paths::Paths;
use crate::reconcile::Op;
use crate::runner::Runner;
use crate::sessions;
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State, Suppression};
use crate::worktrees::Worktrees;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::Duration;

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub runner: &'a dyn Runner,
    pub worktrees: &'a dyn Worktrees,
    pub mux: &'a dyn Mux,
    pub paths: &'a Paths,
    pub now: DateTime<Utc>,
}

impl<'a> Ctx<'a> {
    fn git(&self) -> Git<'a> {
        Git::new(self.runner)
    }
    fn repo(&self, key: &PrKey) -> Result<&'a RepoConfig> {
        self.cfg.repo(&key.repo).with_context(|| format!("repo {} is not in the config", key.repo))
    }
}

fn record<'s>(state: &'s mut State, key: &PrKey) -> Result<&'s mut PrRecord> {
    state.get_mut(key).with_context(|| format!("{key} is not in the state"))
}

pub fn create(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<()> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = record(state, key)?;
    if rec.phase != Phase::Creating {
        return Ok(());
    }
    let sha = git.fetch_branch(&repo.path, &rec.head_ref)?;
    if sha != rec.observed_head_sha {
        return Ok(()); // GitHub e remoto ainda não concordam; o próximo ciclo resolve
    }
    if rec.path.exists() {
        // Sem prova de que fomos nós que criamos: nunca assume.
        rec.phase = Phase::Blocked {
            reason: format!("{} already exists and the daemon has no record of creating it; check it and remove it by hand", rec.path.display()),
        };
        return Ok(());
    }
    if git.ref_sha(&repo.path, &format!("refs/heads/{}", rec.head_ref))?.is_some() {
        rec.phase = Phase::Blocked { reason: format!("branch {} already exists in the clone", rec.head_ref) };
        return Ok(());
    }
    if let Some(parent) = rec.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ctx.worktrees.create(&repo.path, &rec.head_ref, &format!("origin/{}", rec.head_ref), &rec.path)?;
    rec.created_branch = true;
    rec.managed_sha = Some(sha);
    rec.phase = Phase::Preparing;
    rec.warning = None;
    if let Err(e) = git.set_upstream(&rec.path, &rec.head_ref) {
        rec.warning = Some(format!("upstream not configured: {e:#}"));
    }
    // Falha aqui não desfaz o worktree: o SyncBase do próximo ciclo tenta de novo.
    if let Err(e) = sync_base_of(&git, &repo.path, rec) {
        rec.warning = Some(format!("PR base not synced: {e:#}"));
    }
    Ok(())
}

/// Traz `origin/<base>` e grava a base como escolha do reviewr, para o diff do worktree
/// bater com o do GitHub. Só mexe em `refs/remotes/origin/*` e no ref privado do reviewr:
/// vale também para worktree adotado, e nunca avança o `main` local, que é do usuário.
pub fn sync_base(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<Option<String>> {
    let repo = ctx.repo(key)?;
    sync_base_of(&ctx.git(), &repo.path, record(state, key)?)
}

fn sync_base_of(git: &Git, repo: &Path, rec: &mut PrRecord) -> Result<Option<String>> {
    let base = rec.base_ref.clone();
    if base.is_empty() {
        return Ok(None);
    }
    if !rec.path.exists() {
        // Recriado pelo retry, o worktree passa pelo create, que sincroniza de novo.
        rec.synced_base = Some(base);
        return Ok(None);
    }
    git.fetch_branch(repo, &base).with_context(|| format!("base {base} not fetched"))?;
    let mut note = None;
    if rec.synced_base.as_deref() != Some(base.as_str()) {
        let current = git.reviewr_base_pick(&rec.path)?;
        // Só sobrescreve o que o daemon gravou (ou nada); uma escolha feita no reviewr fica.
        if current.is_none() || current == rec.synced_base {
            git.set_reviewr_base_pick(&rec.path, &base)?;
        } else if current.as_deref() != Some(base.as_str()) {
            note = Some(format!("PR base is now {base}; reviewer's manual choice ({}) kept", current.unwrap_or_default()));
        }
    }
    rec.synced_base = Some(base);
    Ok(note)
}

pub fn update(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<()> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let (head_ref, path, observed, managed) = {
        let r = state.get(key).with_context(|| format!("{key} is not in the state"))?;
        if r.ownership != Ownership::Managed {
            return Ok(());
        }
        (r.head_ref.clone(), r.path.clone(), r.observed_head_sha.clone(), r.managed_sha.clone())
    };
    let sha = git.fetch_branch(&repo.path, &head_ref)?;
    if sha != observed || managed.as_deref() == Some(sha.as_str()) {
        return Ok(());
    }
    // Checagem logo antes do efeito, depois do fetch.
    let f = facts::collect(ctx.runner, &path, &repo.disposable_ignored)?;
    let rec = record(state, key)?;
    match divergence(rec, &f) {
        Some(Divergence::Missing) => {
            rec.phase = Phase::Blocked { reason: "worktree is gone from disk; use retry to recreate it".into() };
            return Ok(());
        }
        Some(Divergence::Diverged(reason)) => {
            rec.ownership = Ownership::Adopted { reason, at: ctx.now };
            rec.warning = Some("author updated the PR".into());
            return Ok(());
        }
        None => {}
    }
    if let Some(head) = &f.head {
        git.backup(&repo.path, key.number, head, ctx.now)?;
    }
    if let Err(e) = git.reset_keep(&path, &sha) {
        if !would_overwrite(&format!("{e:#}")) {
            // ex.: index.lock de um git do usuário; segue gerenciado e o próximo ciclo tenta de novo
            return Err(e.context("reset --keep failed"));
        }
        rec.ownership = Ownership::Adopted { reason: format!("update aborted to keep local changes: {e:#}"), at: ctx.now };
        return Ok(());
    }
    rec.managed_sha = Some(sha);
    rec.phase = Phase::Preparing;
    rec.warning = None;
    Ok(())
}

/// O `reset --keep` recusou porque sobrescreveria alteração local (e não por outro motivo).
pub(crate) fn would_overwrite(err: &str) -> bool {
    ["would be overwritten", "not uptodate", "Entry '"].iter().any(|n| err.contains(n))
}

/// Remove a worktree com git (sem `--force`) e faz o prune.
fn discard_worktree(ctx: &Ctx, repo: &Path, path: &Path) -> Result<()> {
    ctx.worktrees.remove(repo, path)
}

/// Apaga a branch do PR só se o daemon a criou, se ela ainda está no sha que ele
/// colocou e se não está aberta em outro worktree. Devolve uma nota quando preserva.
pub fn delete_owned_branch(git: &Git, repo: &Path, rec: &PrRecord) -> Result<Option<String>> {
    if !rec.created_branch {
        return Ok(None);
    }
    let Some(expected) = &rec.managed_sha else { return Ok(None) };
    let Some(current) = git.ref_sha(repo, &format!("refs/heads/{}", rec.head_ref))? else { return Ok(None) };
    if &current != expected {
        return Ok(Some(format!("branch {} kept: it changed since the daemon created it", rec.head_ref)));
    }
    if git.branch_checked_out(repo, &rec.head_ref)? {
        return Ok(Some(format!("branch {} kept: it is checked out in another worktree", rec.head_ref)));
    }
    if !git.delete_branch_if_at(repo, &rec.head_ref, expected)? {
        return Ok(Some(format!("branch {} kept: it changed during removal", rec.head_ref)));
    }
    Ok(None)
}

/// Resultado do fechamento da sessão do PR no Rex.
#[derive(Debug, Clone, PartialEq)]
pub enum Closing {
    /// A sessão estava ociosa e foi fechada; a entrada saiu do `sessions.json`.
    Closed,
    /// Não há sessão: sem entrada no `sessions.json`, ou a sessão já não existe no Rex (a
    /// entrada velha é apagada).
    NoSession,
    /// A sessão está (ou pode estar) em uso. Nada foi fechado nem apagado.
    InUse(String),
}

/// Fecha a sessão do PR se, e só se, ela for ociosa: em todos os blocos, o foreground é um shell
/// com cwd dentro da worktree. Na dúvida (lock ocupado, erro do Rex, bloco sem foreground,
/// sessão sem blocos), devolve `InUse` sem tocar em nada. Só mexe em sessões do `sessions.json`.
pub fn close_session(ctx: &Ctx, key: &PrKey, worktree: &Path) -> Result<Closing> {
    // arquivo corrompido devolve erro: o daemon isola o arquivo antes do próximo ciclo
    if sessions::load(&ctx.paths.sessions_file())?.get(key).is_none() {
        return Ok(Closing::NoSession);
    }
    let _lock = match locks::pr_lock(ctx.paths, key, Duration::ZERO) {
        Ok(l) => l,
        Err(e) if e.to_string().contains("is busy") => {
            return Ok(Closing::InUse("the panel is entering this PR".into()))
        }
        Err(e) => return Err(e),
    };
    // relê sob o lock: o painel pode ter recriado a sessão entre a leitura e o lock
    let Some(entry) = sessions::load(&ctx.paths.sessions_file())?.get(key).cloned() else {
        return Ok(Closing::NoSession);
    };
    let blocks = match inspect_session(ctx.mux, &entry.session_id) {
        Ok(b) => b,
        Err(MuxError::SessionNotFound) => {
            // fechada à mão ou numa execução que morreu antes de apagar a entrada
            sessions::remove(ctx.paths, key)?;
            return Ok(Closing::NoSession);
        }
        Err(e) => return Ok(Closing::InUse(format!("could not inspect the session: {e}"))),
    };
    if blocks.is_empty() {
        return Ok(Closing::InUse("the session has no blocks to inspect".into()));
    }
    for (b, p) in &blocks {
        let label = b.label.as_deref().unwrap_or(&b.id);
        match p {
            Some(p) if is_shell(p) && in_worktree(p, worktree) => {}
            Some(p) if is_shell(p) => {
                return Ok(Closing::InUse(format!("shell in block {label} is outside the worktree")))
            }
            Some(p) => return Ok(Closing::InUse(format!("{} running in block {label}", p.name))),
            None => return Ok(Closing::InUse(format!("block {label} has no foreground process"))),
        }
    }
    match ctx.mux.destroy_session(&entry.session_id) {
        Ok(()) | Err(MuxError::SessionNotFound) => {}
        Err(e) => return Err(anyhow!(e).context(format!("could not close the session of {key}"))),
    }
    sessions::remove(ctx.paths, key)?;
    Ok(Closing::Closed)
}

/// Adota o worktree que divergiu durante a remoção. Devolve a nota para o log.
fn adopt_on_removal(ctx: &Ctx, state: &mut State, key: &PrKey, reason: &str) -> Result<Option<String>> {
    let r = record(state, key)?;
    r.ownership = Ownership::Adopted { reason: reason.into(), at: ctx.now };
    r.phase = Phase::Blocked { reason: format!("removal canceled: {reason}") };
    r.clear_session_block();
    Ok(Some(format!("removal canceled: {reason}")))
}

fn diverged(rec: &PrRecord, f: &Facts) -> Option<String> {
    match divergence(rec, f) {
        Some(Divergence::Diverged(reason)) => Some(reason),
        _ => None,
    }
}

/// Remove um worktree gerenciado e impecável. Qualquer alteração local → adota. A sessão do PR
/// no Rex é fechada antes, só se estiver ociosa; em uso, o PR espera em `Removing`.
pub fn remove(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<Option<String>> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = state.get(key).cloned().with_context(|| format!("{key} is not in the state"))?;
    if rec.ownership != Ownership::Managed {
        return Ok(None);
    }
    let f = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    if let Some(reason) = diverged(&rec, &f) {
        return adopt_on_removal(ctx, state, key, &reason);
    }
    match close_session(ctx, key, &rec.path)? {
        Closing::InUse(why) => {
            let r = record(state, key)?;
            r.leaving_blocked_by_session = true;
            r.blocked_since.get_or_insert(ctx.now);
            return Ok(Some(format!("session in use: {why}")));
        }
        Closing::Closed | Closing::NoSession => record(state, key)?.clear_session_block(),
    }
    // de novo: algo pode ter escrito na worktree enquanto a sessão fechava
    let f = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    if let Some(reason) = diverged(&rec, &f) {
        return adopt_on_removal(ctx, state, key, &reason);
    }
    if f.exists {
        if let Some(head) = &f.head {
            git.backup(&repo.path, key.number, head, ctx.now)?;
        }
    }
    discard_worktree(ctx, &repo.path, &rec.path)?;
    let note = delete_owned_branch(&git, &repo.path, &rec)?;
    state.remove(key);
    if let Some(n) = &note {
        state.push_alert(format!("{key}: {n}"));
    }
    Ok(note)
}

/// Libera um worktree adotado, só se nada local puder se perder.
pub fn release(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<String> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = state.get(key).cloned().with_context(|| format!("{key} is not being tracked"))?;
    if rec.ownership == Ownership::Managed {
        bail!("{key} is not adopted; it is removed automatically when it leaves your queue");
    }
    git.fetch_origin(&repo.path).context("could not update origin to confirm that nothing is lost")?;
    // Depois de squash merge a branch some do origin; o head do PR continua em refs/pull/<n>/head.
    if let Err(e) = git.fetch_pull_head(&repo.path, key.number) {
        crate::applog::info(&format!("{key}: {e:#}; checking against origin branches only"));
    }
    let f = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    // Commits fora do origin num PR já fechado: ficam só no backup ref, avisado na mensagem.
    let mut unpushed_note = None;
    if f.exists {
        if let Some(dirt) = f.dirt() {
            let only_ignored = !f.op_in_progress && f.tracked.is_empty() && f.flagged.is_empty() && f.untracked.is_empty();
            let hint = if only_ignored { " (if the setup generates them, add them to disposable_ignored in the config)" } else { "" };
            bail!("{key} cannot be released: {dirt}. Commit and push, or discard, and try again{hint}");
        }
        if let Some(head) = &f.head {
            if !git.is_reachable_from_origin(&repo.path, head)? {
                let short = &head[..head.len().min(8)];
                // Depois de merge ou fechamento o trabalho local quase sempre já entrou por outro
                // caminho (squash, rebase, outro PR); com o PR aberto ele ainda pode ser pushado.
                match crate::github_extras::fetch_pr_state(ctx.runner, &key.repo, key.number) {
                    Ok(state) if state != "OPEN" => {
                        unpushed_note = Some(format!(
                            "PR {}: commits of HEAD {short} that are not on origin are kept in refs/reviewq/backup/pr-{} for 14 days",
                            state.to_lowercase(),
                            key.number
                        ));
                    }
                    Ok(_) => bail!("{key} cannot be released: HEAD {short} has commits that are not on origin and the PR is still open"),
                    Err(e) => bail!(
                        "{key} cannot be released: HEAD {short} has commits that are not on origin (could not check whether the PR is closed: {e:#})"
                    ),
                }
            }
        }
    }
    if f.exists && f.head.is_none() {
        bail!("{key} cannot be released: could not read HEAD");
    }
    let stashes = git.stash_count(&repo.path)?;
    // Mesma regra do remove: fecha a sessão só se estiver ociosa. Em uso, a sessão fica aberta
    // e sai do sessions.json (o daemon nunca mais a fecha); a worktree é liberada mesmo assim.
    let session_note = match close_session(ctx, key, &rec.path)? {
        Closing::InUse(why) => {
            sessions::remove(ctx.paths, key)?;
            Some(format!("its Rex session was left open ({why})"))
        }
        Closing::Closed | Closing::NoSession => None,
    };
    // de novo, depois de mexer na sessão: nada local pode ter aparecido nesse meio-tempo
    let again = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    if again.exists != f.exists || again.head != f.head {
        bail!("{key} cannot be released: the worktree changed while it was being released");
    }
    if let Some(dirt) = again.exists.then(|| again.dirt()).flatten() {
        bail!("{key} cannot be released: {dirt} (it appeared while the release ran)");
    }
    if f.exists {
        if let Some(head) = &f.head {
            git.backup(&repo.path, key.number, head, ctx.now)?;
        }
    }
    discard_worktree(ctx, &repo.path, &rec.path)?;
    let note = delete_owned_branch(&git, &repo.path, &rec)?;
    if matches!(rec.remote, Remote::Pending | Remote::Unknown) {
        state.suppressions.insert(key.to_string(), Suppression { after: rec.last_request_event_at });
    }
    state.remove(key);
    let mut msg = format!("{key} released");
    if let Some(n) = &unpushed_note {
        state.push_alert(format!("{key}: {n}"));
        msg.push_str(&format!("; {n}"));
    }
    if let Some(n) = note {
        state.push_alert(format!("{key}: {n}"));
        msg.push_str(&format!("; {n}"));
    }
    if let Some(n) = session_note {
        msg.push_str(&format!("; {n}"));
    }
    if stashes > 0 {
        msg.push_str(&format!("; note: there are {stashes} stash(es) in the repository"));
    }
    Ok(msg)
}

pub fn run_op(ctx: &Ctx, state: &mut State, op: &Op) -> Result<Option<String>> {
    match op {
        Op::Create(k) => create(ctx, state, k).map(|_| None),
        Op::Update(k) => update(ctx, state, k).map(|_| None),
        Op::Remove(k) => remove(ctx, state, k),
        Op::SyncBase(k) => sync_base(ctx, state, k),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fake::{program_at, shell_at, FakeBlock, FakeMux};
    use crate::sessions::SessionEntry;
    use crate::worktrees::GitWorktrees;
    use crate::reconcile::worktree_path;
    use crate::mux::MuxError;
    use crate::runner::RealRunner;
    use crate::testutil::{config, fixture, git, Fixture};

    const BRANCH: &str = "feat/x";

    /// Rex falso e diretório de estado (sessions.json, locks) do teste.
    fn env(fx: &Fixture) -> (FakeMux, Paths) {
        (FakeMux::new(), Paths::from_home(fx.root.join("home")))
    }

    fn creating(fx: &Fixture, cfg: &Config) -> (State, PrKey) {
        let mut rec = PrRecord::fixture("o/r", 7, BRANCH, &fx.remote_sha(BRANCH));
        rec.phase = Phase::Creating;
        rec.created_branch = false;
        rec.managed_sha = None;
        rec.prepared_sha = None;
        rec.workspace_id = None;
        rec.path = worktree_path(&cfg.worktrees_dir, &rec.key);
        let key = rec.key.clone();
        let mut s = State::default();
        s.insert(rec);
        (s, key)
    }

    /// Cria e marca como pronto.
    fn ready(ctx: &Ctx, state: &mut State, key: &PrKey) -> std::path::PathBuf {
        create(ctx, state, key).unwrap();
        let rec = state.get_mut(key).unwrap();
        rec.phase = Phase::Ready;
        rec.path.clone()
    }

    fn adopt(s: &mut State, key: &PrKey) {
        s.get_mut(key).unwrap().ownership = Ownership::Adopted { reason: "teste".into(), at: Utc::now() };
    }

    fn branch_exists(fx: &Fixture, name: &str) -> bool {
        crate::git::Git::new(&RealRunner).ref_sha(&fx.clone, &format!("refs/heads/{name}")).unwrap().is_some()
    }

    #[test]
    fn create_builds_worktree_with_upstream() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        create(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Preparing);
        assert!(rec.created_branch);
        assert_eq!(rec.managed_sha.as_deref(), Some(fx.remote_sha(BRANCH).as_str()));
        assert_eq!(rec.workspace_id, None, "o workspace deixa de nascer junto com a worktree");
        assert_eq!(git(&rec.path, &["rev-parse", "--abbrev-ref", "@{u}"]), "origin/feat/x");
        assert_eq!(git(&rec.path, &["rev-parse", "--abbrev-ref", "HEAD"]), BRANCH);
    }

    /// PR `head` contra `base`, pronto para o create.
    fn creating_pr(fx: &Fixture, cfg: &Config, head: &str, base: &str) -> (State, PrKey) {
        let (mut s, key) = creating(fx, cfg);
        let rec = s.get_mut(&key).unwrap();
        rec.head_ref = head.into();
        rec.observed_head_sha = fx.remote_sha(head);
        rec.base_ref = base.into();
        rec.synced_base = None;
        (s, key)
    }

    fn changed(wt: &Path, base: &str) -> String {
        git(wt, &["diff", "--name-only", &format!("origin/{base}...HEAD")])
    }

    fn pick(path: &Path) -> Option<String> {
        crate::git::Git::new(&RealRunner).reviewr_base_pick(path).unwrap()
    }

    #[test]
    fn create_diffs_against_fresh_base_and_leaves_local_main_alone() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let stale = git(&fx.clone, &["rev-parse", "main"]);
        fx.author_push("main", "na-main.txt", "entrou depois\n");
        fx.author_branch("main", "feat/y", "y.txt");
        let (mut s, key) = creating_pr(&fx, &cfg, "feat/y", "main");
        create(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        // O bug do PR 15682: com a base parada, viria também na-main.txt.
        assert_eq!(changed(&rec.path, "main"), "y.txt");
        assert_eq!(git(&fx.clone, &["rev-parse", "refs/heads/main"]), stale);
        assert_eq!(pick(&rec.path).as_deref(), Some("main"));
        assert_eq!(rec.synced_base.as_deref(), Some("main"));
    }

    #[test]
    fn stacked_pr_diffs_against_the_pr_below_until_retargeted() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        fx.author_branch(BRANCH, "feat/y", "y.txt");
        fx.author_branch("feat/y", "feat/z", "z.txt");
        let (mut s, key) = creating_pr(&fx, &cfg, "feat/y", BRANCH);
        create(&ctx, &mut s, &key).unwrap();
        let path = s.get(&key).unwrap().path.clone();
        assert_eq!(changed(&path, BRANCH), "y.txt");
        assert_eq!(pick(&path).as_deref(), Some(BRANCH));
        // O de baixo foi mergeado: o GitHub retargeta para main e rebaseia.
        git(&fx.author, &["checkout", "-q", "main"]);
        git(&fx.author, &["merge", "-q", "--ff-only", BRANCH]);
        git(&fx.author, &["push", "-q", "origin", "main", &format!(":{BRANCH}")]);
        s.get_mut(&key).unwrap().base_ref = "main".into();
        assert_eq!(sync_base(&ctx, &mut s, &key).unwrap(), None);
        assert_eq!(pick(&path).as_deref(), Some("main"));
        assert_eq!(changed(&path, "main"), "y.txt");
        assert_eq!(s.get(&key).unwrap().synced_base.as_deref(), Some("main"));
    }

    #[test]
    fn sync_base_keeps_a_pick_made_in_reviewr() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        fx.author_branch(BRANCH, "feat/y", "y.txt");
        let (mut s, key) = creating_pr(&fx, &cfg, "feat/y", BRANCH);
        create(&ctx, &mut s, &key).unwrap();
        let path = s.get(&key).unwrap().path.clone();
        crate::git::Git::new(&RealRunner).set_reviewr_base_pick(&path, "HEAD~1").unwrap();
        adopt(&mut s, &key);
        s.get_mut(&key).unwrap().base_ref = "main".into();
        let note = sync_base(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("HEAD~1"), "{note}");
        assert_eq!(pick(&path).as_deref(), Some("HEAD~1"));
        assert_eq!(s.get(&key).unwrap().synced_base.as_deref(), Some("main"));
    }

    #[test]
    fn create_survives_a_base_gone_from_remote() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating_pr(&fx, &cfg, BRANCH, "sumiu");
        create(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Preparing);
        assert!(rec.warning.as_deref().unwrap().contains("sumiu"));
        assert_eq!(rec.synced_base, None);
        assert_eq!(pick(&rec.path), None);
    }

    #[test]
    fn create_waits_when_github_and_remote_disagree() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        s.get_mut(&key).unwrap().observed_head_sha = "outro-sha".into();
        create(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().phase, Phase::Creating);
        assert!(!s.get(&key).unwrap().path.exists());
    }

    #[test]
    fn create_blocks_when_branch_already_exists_locally() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        git(&fx.clone, &["fetch", "-q", "origin"]);
        git(&fx.clone, &["branch", BRANCH, "origin/feat/x"]);
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        create(&ctx, &mut s, &key).unwrap();
        assert!(matches!(&s.get(&key).unwrap().phase, Phase::Blocked { reason } if reason.contains("already exists")));
        assert!(!s.get(&key).unwrap().created_branch);
    }

    #[test]
    fn create_never_claims_an_existing_path() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let before = s.clone();
        create(&ctx, &mut s, &key).unwrap(); // simula: criou, mas o estado não foi salvo
        let path = s.get(&key).unwrap().path.clone();
        std::fs::write(path.join("notas.txt"), "trabalho").unwrap();
        let mut restarted = before;
        create(&ctx, &mut restarted, &key).unwrap();
        let rec = restarted.get(&key).unwrap();
        assert!(matches!(&rec.phase, Phase::Blocked { reason } if reason.contains("already exists")));
        assert!(!rec.created_branch);
        assert!(path.join("notas.txt").exists());
    }

    #[test]
    fn update_moves_to_new_sha_and_keeps_backup() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let old = s.get(&key).unwrap().managed_sha.clone().unwrap();
        let new = fx.author_push(BRANCH, "f2.txt", "x");
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        update(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Preparing);
        assert_eq!(rec.managed_sha.as_deref(), Some(new.as_str()));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
        let backups = git(&fx.clone, &["for-each-ref", "--format=%(objectname)", "refs/reviewq/backup/pr-7"]);
        assert!(backups.contains(&old));
    }

    #[test]
    fn update_handles_force_push() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let new = fx.author_force_push(BRANCH);
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
    }

    #[test]
    fn update_adopts_instead_of_touching_local_commit() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "meu"]);
        let mine = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = fx.author_push(BRANCH, "f2.txt", "x");
        update(&ctx, &mut s, &key).unwrap();
        assert!(matches!(s.get(&key).unwrap().ownership, Ownership::Adopted { .. }));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), mine);
    }

    #[test]
    fn update_adopts_when_user_created_a_file() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join("notas.txt"), "x").unwrap();
        let old = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = fx.author_push(BRANCH, "f2.txt", "x");
        update(&ctx, &mut s, &key).unwrap();
        assert!(matches!(&s.get(&key).unwrap().ownership, Ownership::Adopted { reason, .. } if reason.contains("notas.txt")));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), old);
    }

    #[test]
    fn update_with_git_lock_fails_and_retries_without_adopting() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let new = fx.author_push(BRANCH, "f2.txt", "x");
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        let lock = std::path::PathBuf::from(git(&path, &["rev-parse", "--absolute-git-dir"])).join("index.lock");
        std::fs::write(&lock, "").unwrap();
        assert!(update(&ctx, &mut s, &key).is_err());
        assert_eq!(s.get(&key).unwrap().ownership, Ownership::Managed);
        assert_ne!(s.get(&key).unwrap().managed_sha.as_deref(), Some(new.as_str()));
        std::fs::remove_file(&lock).unwrap();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().ownership, Ownership::Managed);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
    }

    #[test]
    fn would_overwrite_matches_only_overwrite_errors() {
        assert!(would_overwrite("error: Entry 'feature.txt' not uptodate. Cannot merge."));
        assert!(would_overwrite("Your local changes to the following files would be overwritten by merge"));
        assert!(!would_overwrite("fatal: Unable to create '/x/index.lock': File exists."));
    }

    #[test]
    fn update_waits_when_fetch_disagrees_with_github() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let old = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = "sha-que-o-remoto-ainda-nao-tem".into();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().phase, Phase::Ready);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), old);
    }

    #[test]
    fn remove_cleans_worktree_branch_and_record() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::create_dir_all(path.join("node_modules/pkg")).unwrap(); // descartável
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(s.get(&key).is_none());
        assert!(!path.exists());
        assert!(!branch_exists(&fx, BRANCH));
        assert!(!git(&fx.clone, &["for-each-ref", "refs/reviewq/backup/pr-7"]).is_empty());
    }

    #[test]
    fn remove_backs_off_on_any_local_change() {
        for (file, content) in [(".env", "SEGREDO=1"), ("feature.txt", "lockfile reescrito\n"), ("notas.txt", "x")] {
            let fx = fixture(BRANCH);
            let cfg = config(&fx, "true");
            let (mux, paths) = env(&fx);
            let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
            let (mut s, key) = creating(&fx, &cfg);
            let path = ready(&ctx, &mut s, &key);
            std::fs::write(path.join(file), content).unwrap();
            s.get_mut(&key).unwrap().phase = Phase::Removing;
            let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
            assert!(note.contains("canceled"), "{file}: {note}");
            assert!(matches!(s.get(&key).unwrap().ownership, Ownership::Adopted { .. }));
            assert_eq!(std::fs::read_to_string(path.join(file)).unwrap(), content);
            assert!(branch_exists(&fx, BRANCH));
        }
    }

    #[test]
    fn remove_preserves_branch_that_moved_after_worktree_is_gone() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
        let main = git(&fx.clone, &["rev-parse", "main"]);
        git(&fx.clone, &["update-ref", "refs/heads/feat/x", &main]);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("kept"));
        assert!(branch_exists(&fx, BRANCH));
        assert!(s.get(&key).is_none());
        assert!(s.alerts.iter().any(|a| a.message.contains("kept")));
    }

    #[test]
    fn remove_preserves_branch_open_in_another_worktree() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
        let other = fx.root.join("outro-checkout");
        git(&fx.clone, &["worktree", "add", "-q", other.to_str().unwrap(), BRANCH]);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("another worktree"));
        assert!(branch_exists(&fx, BRANCH));
    }

    #[test]
    fn remove_of_record_without_worktree_just_forgets() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        remove(&ctx, &mut s, &key).unwrap();
        assert!(s.get(&key).is_none());
    }

    /// `gh` no FakeRunner (sem regra = falha) e git de verdade.
    struct GhOnFake {
        gh: crate::runner::FakeRunner,
    }

    impl GhOnFake {
        fn new() -> Self {
            Self { gh: crate::runner::FakeRunner::new() }
        }
        fn pr_state(&self, state: &str) {
            self.gh.on("gh pr view 7 --repo o/r", crate::runner::Output::ok(&format!("{state}\n")));
        }
    }

    impl crate::runner::Runner for GhOnFake {
        fn run(&self, c: &crate::runner::Cmd) -> Result<crate::runner::Output> {
            if c.program == "gh" { self.gh.run(c) } else { RealRunner.run(c) }
        }
    }

    #[test]
    fn release_refuses_unpushed_commit_then_accepts_after_push() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let runner = GhOnFake::new();
        let ctx = Ctx { cfg: &cfg, runner: &runner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["checkout", "-q", "-b", "stack/minha"]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "empilhado"]);
        adopt(&mut s, &key);
        runner.pr_state("OPEN");
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("origin") && err.contains("still open"), "{err}");
        assert!(path.exists());

        git(&path, &["push", "-q", "origin", "stack/minha"]);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(msg.contains("released"));
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
        assert!(branch_exists(&fx, "stack/minha"), "branch empilhada é do usuário");
        assert!(!branch_exists(&fx, BRANCH), "branch do PR foi criada pelo daemon");
    }

    #[test]
    fn release_after_squash_merge_uses_pull_head() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let head = fx.remote_sha(BRANCH);
        git(&fx.remote, &["update-ref", "refs/pull/7/head", &head]);
        git(&fx.remote, &["update-ref", "-d", "refs/heads/feat/x"]); // squash merge + delete branch
        adopt(&mut s, &key);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(msg.contains("released"), "{msg}");
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
    }

    #[test]
    fn release_without_pull_head_stays_conservative() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let runner = GhOnFake::new();
        let ctx = Ctx { cfg: &cfg, runner: &runner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.remote, &["update-ref", "-d", "refs/heads/feat/x"]);
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("origin") && err.contains("could not check"), "{err}");
        assert!(path.exists());
    }

    #[test]
    fn release_ignores_stale_refs_of_other_remotes() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let runner = GhOnFake::new();
        let ctx = Ctx { cfg: &cfg, runner: &runner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["checkout", "-q", "-b", "stack/minha"]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "empilhado"]);
        let local = git(&path, &["rev-parse", "HEAD"]);
        git(&fx.clone, &["update-ref", "refs/remotes/backup/velha", &local]);
        adopt(&mut s, &key);
        runner.pr_state("OPEN");
        assert!(release(&ctx, &mut s, &key).is_err());
        assert!(path.exists());
    }

    #[test]
    fn release_refuses_local_changes() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join("feature.txt"), "mexi\n").unwrap();
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("feature.txt"), "{err}");
        assert!(path.exists());
    }

    #[test]
    fn release_of_closed_pr_keeps_unpushed_commits_only_in_backup() {
        for pr_state in ["MERGED", "CLOSED"] {
            let fx = fixture(BRANCH);
            let cfg = config(&fx, "true");
            let (mux, paths) = env(&fx);
            let runner = GhOnFake::new();
            let ctx = Ctx { cfg: &cfg, runner: &runner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
            let (mut s, key) = creating(&fx, &cfg);
            let path = ready(&ctx, &mut s, &key);
            git(&path, &["commit", "-q", "--allow-empty", "-m", "rebase local"]);
            let local = git(&path, &["rev-parse", "HEAD"]);
            adopt(&mut s, &key);
            runner.pr_state(pr_state);
            let msg = release(&ctx, &mut s, &key).unwrap();
            assert!(msg.contains("released") && msg.contains("refs/reviewq/backup/pr-7"), "{msg}");
            assert!(!path.exists());
            assert!(s.get(&key).is_none());
            let backups = git(&fx.clone, &["for-each-ref", "--points-at", &local, "--format=%(refname)", "refs/reviewq/backup/pr-7/"]);
            assert!(!backups.trim().is_empty(), "local commit must stay in a backup ref");
            assert!(s.alerts.iter().any(|a| a.message.contains("refs/reviewq/backup/pr-7")), "{:?}", s.alerts);
        }
    }

    #[test]
    fn release_refuses_ignored_env_file() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join(".env"), "SEGREDO=1").unwrap();
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains(".env") && err.contains("disposable_ignored"), "{err}");
        assert!(path.join(".env").exists());
    }

    #[test]
    fn release_suppresses_when_pending_or_unknown() {
        for remote in [Remote::Pending, Remote::Unknown] {
            let fx = fixture(BRANCH);
            let cfg = config(&fx, "true");
            let (mux, paths) = env(&fx);
            let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
            let (mut s, key) = creating(&fx, &cfg);
            ready(&ctx, &mut s, &key);
            let when: DateTime<Utc> = "2026-10-07T10:00:00Z".parse().unwrap();
            let rec = s.get_mut(&key).unwrap();
            rec.last_request_event_at = Some(when);
            rec.remote = remote.clone();
            adopt(&mut s, &key);
            release(&ctx, &mut s, &key).unwrap();
            assert_eq!(s.suppressions[&key.to_string()].after, Some(when), "{remote:?}");
        }
    }

    #[test]
    fn release_rejects_managed() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &mux, paths: &paths, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        ready(&ctx, &mut s, &key);
        assert!(release(&ctx, &mut s, &key).is_err());
    }

    // ---- fechamento da sessão do PR (item 18) ----

    macro_rules! ctx {
        ($cfg:expr, $mux:expr, $paths:expr) => {
            Ctx { cfg: &$cfg, runner: &RealRunner, worktrees: &GitWorktrees::new(&RealRunner), mux: &$mux, paths: &$paths, now: Utc::now() }
        };
    }

    const SID: &str = "session:pr7";

    fn block(id: &str, label: &str, p: Option<crate::mux::Process>) -> FakeBlock {
        FakeBlock { id: id.into(), label: Some(label.into()), process: p, ..Default::default() }
    }

    /// Sessão do PR no Rex falso e a entrada dela no sessions.json.
    fn with_session(mux: &FakeMux, paths: &Paths, key: &PrKey, blocks: Vec<FakeBlock>) {
        mux.add_session(SID, "#7 r", blocks);
        let entry = SessionEntry { session_id: SID.into(), shell_block: "b1".into(), created_at: Utc::now(), entered_at: None };
        sessions::upsert(paths, key, entry).unwrap();
    }

    fn has_entry(paths: &Paths, key: &PrKey) -> bool {
        sessions::load(&paths.sessions_file()).unwrap().get(key).is_some()
    }

    fn has_session(mux: &FakeMux) -> bool {
        mux.sessions.borrow().contains_key(SID)
    }

    fn calls(mux: &FakeMux, method: &str) -> usize {
        mux.calls.borrow().iter().filter(|c| *c == method).count()
    }

    /// Pronto, depois a carência passou: `Removing`.
    fn removing(ctx: &Ctx, s: &mut State, key: &PrKey) -> std::path::PathBuf {
        let path = ready(ctx, s, key);
        s.get_mut(key).unwrap().phase = Phase::Removing;
        path
    }

    fn assert_waiting(s: &State, key: &PrKey) {
        let r = s.get(key).expect("o PR continua no estado");
        assert_eq!(r.phase, Phase::Removing);
        assert_eq!(r.ownership, Ownership::Managed);
        assert!(r.leaving_blocked_by_session);
        assert!(r.blocked_since.is_some());
    }

    #[test]
    fn idle_session_is_closed_before_the_worktree_goes() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path))), block("b2", "log", Some(shell_at(&path.join("sub"))))]);
        assert_eq!(close_session(&ctx, &key, &path).unwrap(), Closing::Closed);
        assert!(!has_session(&mux) && !has_entry(&paths, &key));

        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
        assert!(!has_session(&mux) && !has_entry(&paths, &key));
        assert!(!branch_exists(&fx, BRANCH));
    }

    #[test]
    fn dirty_worktree_is_adopted_and_its_session_kept() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        std::fs::write(path.join("notas.txt"), "x").unwrap();
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("removal canceled"), "{note}");
        assert!(matches!(s.get(&key).unwrap().ownership, Ownership::Adopted { .. }));
        assert!(path.join("notas.txt").exists());
        assert!(has_session(&mux) && has_entry(&paths, &key), "a sessão não é fechada");
        assert_eq!(calls(&mux, "destroy_session"), 0);
    }

    #[test]
    fn changed_branch_is_preserved_with_its_session() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        git(&path, &["checkout", "-q", "-b", "minha"]);
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("switched to minha"), "{note}");
        assert!(path.exists());
        assert!(branch_exists(&fx, BRANCH) && branch_exists(&fx, "minha"));
        assert!(has_session(&mux) && has_entry(&paths, &key));
        assert_eq!(calls(&mux, "destroy_session"), 0);
    }

    #[test]
    fn claude_in_foreground_keeps_everything_until_it_exits() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path))), block("b2", "claude", Some(program_at("claude", &path)))]);
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("session in use") && note.contains("claude"), "{note}");
        assert_waiting(&s, &key);
        let since = s.get(&key).unwrap().blocked_since;
        assert!(path.exists() && branch_exists(&fx, BRANCH));
        assert!(has_session(&mux) && has_entry(&paths, &key));
        assert_eq!(calls(&mux, "destroy_session"), 0);

        // ciclo seguinte, ainda em uso: o tempo retido não reinicia
        let mut later = ctx!(cfg, mux, paths);
        later.now = Utc::now() + chrono::Duration::minutes(5);
        remove(&later, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().blocked_since, since);

        // o Claude saiu: a remoção segue
        mux.sessions.borrow_mut().get_mut(SID).unwrap().blocks[1].process = Some(shell_at(&path));
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(s.get(&key).is_none() && !path.exists());
        assert!(!has_session(&mux) && !has_entry(&paths, &key));
    }

    #[test]
    fn inspect_errors_and_odd_blocks_count_as_in_use() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let elsewhere = fx.root.join("outro");
        std::fs::create_dir_all(&elsewhere).unwrap();
        type Setup = Box<dyn Fn(&FakeMux, &Path)>;
        let cases: Vec<(&str, Setup)> = vec![
            ("Other", Box::new(|m: &FakeMux, _: &Path| m.fail_on("inspect_block", MuxError::Other("payload estranho".into())))),
            ("Unreachable no view", Box::new(|m: &FakeMux, _: &Path| m.fail_on("view", MuxError::Unreachable("exit 4".into())))),
            ("bloco sem foreground", Box::new(|m: &FakeMux, _: &Path| m.sessions.borrow_mut().get_mut(SID).unwrap().blocks[0].process = None)),
        ];
        for (name, setup) in cases {
            let (mux, paths) = env(&fx);
            let ctx = ctx!(cfg, mux, paths);
            let (mut s, key) = creating(&fx, &cfg);
            let path = removing(&ctx, &mut s, &key);
            with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
            setup(&mux, &path);
            let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
            assert!(note.contains("session in use"), "{name}: {note}");
            assert_waiting(&s, &key);
            assert!(path.exists(), "{name}");
            assert!(has_entry(&paths, &key), "{name}");
            assert_eq!(calls(&mux, "destroy_session"), 0, "{name}");
            // limpa para o próximo caso (mesmo fixture, mesmo caminho)
            git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
            git(&fx.clone, &["branch", "-D", BRANCH]);
        }
        // shell parado fora da worktree também é uso
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&elsewhere)))]);
        assert!(matches!(close_session(&ctx, &key, &path).unwrap(), Closing::InUse(w) if w.contains("outside")));
    }

    #[test]
    fn session_gone_from_rex_lets_the_removal_go_on() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![]);
        mux.sessions.borrow_mut().clear(); // fechada à mão
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(!path.exists() && s.get(&key).is_none());
        assert!(!has_entry(&paths, &key), "a entrada velha sai do sessions.json");
        assert_eq!(calls(&mux, "destroy_session"), 0);
    }

    #[test]
    fn block_that_vanished_during_inspection_is_ignored() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path))), block("b2", "claude", Some(program_at("claude", &path)))]);
        mux.fail_on_call("inspect_block", 2, MuxError::BlockNotFound);
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(!path.exists() && !has_session(&mux));
    }

    #[test]
    fn busy_pr_lock_counts_as_in_use() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        let held = locks::pr_lock(&paths, &key, Duration::ZERO).unwrap();
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("panel"), "{note}");
        assert_waiting(&s, &key);
        assert!(path.exists() && has_session(&mux));
        assert!(mux.calls.borrow().is_empty(), "sem o lock, nem inspeciona");
        drop(held);
    }

    #[test]
    fn session_with_zero_blocks_is_never_idle() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![]);
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("no blocks"), "{note}");
        assert_waiting(&s, &key);
        assert!(path.exists() && has_session(&mux) && has_entry(&paths, &key));
    }

    #[test]
    fn ignored_file_created_while_closing_the_session_adopts() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        let env_file = path.join(".env");
        let hook_file = env_file.clone();
        *mux.on_destroy.borrow_mut() = Some(Box::new(move || std::fs::write(&hook_file, "SEGREDO=1").unwrap()));
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("removal canceled") && note.contains(".env"), "{note}");
        let r = s.get(&key).unwrap();
        assert!(matches!(r.ownership, Ownership::Adopted { .. }));
        assert!(!r.leaving_blocked_by_session);
        assert_eq!(std::fs::read_to_string(&env_file).unwrap(), "SEGREDO=1");
        assert!(branch_exists(&fx, BRANCH));
    }

    #[test]
    fn removal_resumes_after_dying_between_destroy_and_remove() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        // morreu depois do destroy: sessão fechada, entrada e worktree ainda lá
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        let before = s.clone();
        mux.destroy_session(SID).unwrap();
        let mut resumed = before.clone();
        assert_eq!(remove(&ctx, &mut resumed, &key).unwrap(), None);
        assert!(resumed.get(&key).is_none() && !path.exists() && !has_entry(&paths, &key));
        assert!(!branch_exists(&fx, BRANCH));

        // morreu depois do descarte: a worktree também já sumiu, o estado ainda diz Removing
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        mux.destroy_session(SID).unwrap();
        git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(s.get(&key).is_none() && !has_entry(&paths, &key));
        assert!(!branch_exists(&fx, BRANCH));
    }

    #[test]
    fn release_with_session_in_use_frees_the_worktree_and_keeps_the_session() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        adopt(&mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "claude", Some(program_at("claude", &path)))]);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(msg.contains("released") && msg.contains("left open"), "{msg}");
        assert!(!path.exists() && s.get(&key).is_none());
        assert!(has_session(&mux), "a sessão em uso continua aberta");
        assert!(!has_entry(&paths, &key), "e sai do sessions.json");
        assert_eq!(calls(&mux, "destroy_session"), 0);

        // ociosa: o release fecha a sessão
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        adopt(&mut s, &key);
        with_session(&mux, &paths, &key, vec![block("b1", "shell", Some(shell_at(&path)))]);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(!msg.contains("left open"), "{msg}");
        assert!(!has_session(&mux) && !has_entry(&paths, &key));
    }

    #[test]
    fn session_outside_sessions_json_is_never_closed() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let (mux, paths) = env(&fx);
        let ctx = ctx!(cfg, mux, paths);
        let (mut s, key) = creating(&fx, &cfg);
        let path = removing(&ctx, &mut s, &key);
        // mesma cara de sessão do PR, mas não foi o rrq que a registrou
        mux.add_session("session:manual", "#7 r", vec![block("b1", "shell", Some(shell_at(&path)))]);
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(!path.exists());
        assert!(mux.sessions.borrow().contains_key("session:manual"));
        assert!(mux.calls.borrow().is_empty(), "nem lista, nem inspeciona: {:?}", mux.calls.borrow());
    }
}
