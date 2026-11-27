//! Enter, combo (`c`) e "próxima pronta" (`f`) do painel: levam o app do Rex até a sessão do PR,
//! criando a sessão sob demanda. Tudo passa pelo `Mux`, então os testes usam o `FakeMux`.

use crate::config::Config;
use crate::git::Git;
use crate::locks;
use crate::mux::{
    in_worktree, inspect_session, is_shell, BlockInfo, CreateReq, Mux, MuxError, MuxResult,
    Process, SplitReq,
};
use crate::paths::Paths;
use crate::sessions::{self, SessionEntry};
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State};
use crate::worktrees::{GitWorktrees, Worktrees};
use chrono::Utc;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Quanto o Enter espera pelo lock do PR (o fechamento da sessão pelo daemon segura o mesmo lock).
pub const LOCK_WAIT: Duration = Duration::from_millis(100);
pub const SHELL_LABEL: &str = "shell";
pub const CLAUDE_LABEL: &str = "claude";
pub const REVIEWR_LABEL: &str = "reviewr";
/// Fração da largura que fica com o shell quando o `claude` abre ao lado dele.
const CLAUDE_RATIO: f64 = 0.5;
/// Fração que fica com o `claude` quando o `reviewr` abre ao lado dele (o `reviewr` fica com 55%).
const REVIEWR_RATIO: f64 = 0.45;

/// O que o Enter precisa: o Rex, a config, os caminhos e o `client_id` do app em cache. O
/// `client_id` muda a cada reconexão do app, então é refeito quando o Rex responde `ClientNotFound`.
pub struct Ctx<'a> {
    pub mux: &'a dyn Mux,
    pub cfg: &'a Config,
    pub paths: &'a Paths,
    pub client: &'a RefCell<Option<String>>,
}

#[derive(Debug)]
pub enum FocusError {
    /// Outro Enter, combo ou fechamento do mesmo PR está em andamento.
    Busy(PrKey),
    Gone(PrKey),
    NotEligible(String),
    /// Nenhum app conectado com o label `rex.app_label`.
    NoApp(String),
    Rex(MuxError),
    Other(anyhow::Error),
}

impl std::fmt::Display for FocusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FocusError::Busy(k) => write!(
                f,
                "{k} is busy (another Enter or the daemon is on it); try again"
            ),
            FocusError::Gone(k) => write!(f, "{k} is no longer in the panel"),
            FocusError::NotEligible(m) => f.write_str(m),
            FocusError::NoApp(label) => write!(
                f,
                "{label} not connected: no Rex app with label \"{label}\""
            ),
            FocusError::Rex(e) => write!(f, "{e}"),
            FocusError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for FocusError {}

impl From<MuxError> for FocusError {
    fn from(e: MuxError) -> Self {
        FocusError::Rex(e)
    }
}

impl From<anyhow::Error> for FocusError {
    fn from(e: anyhow::Error) -> Self {
        FocusError::Other(e)
    }
}

pub type FocusResult<T> = std::result::Result<T, FocusError>;

#[derive(Debug, Clone, PartialEq)]
pub struct Entered {
    pub session: String,
    /// A sessão foi criada agora (não existia ou tinha sido fechada à mão).
    pub created: bool,
}

/// Fases em que existe uma worktree estável para abrir.
pub fn phase_eligible(phase: &Phase) -> bool {
    !matches!(phase, Phase::Creating | Phase::Removing)
}

/// Enter e `c` aceitam qualquer fase menos `Creating` e `Removing`, desde que a worktree exista.
pub fn eligible(rec: &PrRecord) -> bool {
    phase_eligible(&rec.phase) && rec.path.exists()
}

pub fn is_first_ready_candidate(r: &PrRecord) -> bool {
    r.remote == Remote::Pending && r.ownership == Ownership::Managed && r.phase == Phase::Ready
}

/// A review pronta mais antiga (por `first_seen_at`), entre as gerenciadas e ainda pedidas.
pub fn pick_first_ready(state: &State) -> Option<PrKey> {
    state
        .prs
        .values()
        .filter(|r| is_first_ready_candidate(r))
        .min_by(|a, b| (a.first_seen_at, &a.key).cmp(&(b.first_seen_at, &b.key)))
        .map(|r| r.key.clone())
}

/// `#123 repo`, ou `owner/repo#123` quando a config tem repos de mais de um dono.
pub fn session_label(cfg: &Config, key: &PrKey) -> String {
    let owner = |r: &str| r.split('/').next().unwrap_or(r).to_string();
    let owners: std::collections::BTreeSet<_> = cfg.repos.iter().map(|r| owner(&r.name)).collect();
    if owners.len() > 1 {
        return key.to_string();
    }
    let short = key.repo.rsplit('/').next().unwrap_or(&key.repo);
    format!("#{} {short}", key.number)
}

/// Quoting POSIX de um argumento: sem aspas quando só tem caracteres seguros.
pub fn quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// O argv do combo como linha de shell, com `{worktree}` substituído.
pub fn command_line(argv: &[String], worktree: &Path) -> String {
    let wt = worktree.to_string_lossy();
    argv.iter()
        .map(|a| quote(&a.replace("{worktree}", &wt)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Conta as chamadas ao Rex de um Enter, para o `tui.log`.
struct Counting<'a> {
    inner: &'a dyn Mux,
    n: Cell<usize>,
}

impl Counting<'_> {
    fn tick(&self) {
        self.n.set(self.n.get() + 1);
    }
}

impl Mux for Counting<'_> {
    fn server_api_revision(&self) -> MuxResult<u32> {
        self.tick();
        self.inner.server_api_revision()
    }
    fn list_sessions(&self) -> MuxResult<Vec<crate::mux::SessionInfo>> {
        self.tick();
        self.inner.list_sessions()
    }
    fn create_session(&self, req: &CreateReq) -> MuxResult<crate::mux::Created> {
        self.tick();
        self.inner.create_session(req)
    }
    fn view(&self, session: &str) -> MuxResult<Vec<BlockInfo>> {
        self.tick();
        self.inner.view(session)
    }
    fn session_of(&self, block: &str) -> MuxResult<String> {
        self.tick();
        self.inner.session_of(block)
    }
    fn destroy_session(&self, session: &str) -> MuxResult<()> {
        self.tick();
        self.inner.destroy_session(session)
    }
    fn inspect_block(&self, session: &str, block: &str) -> MuxResult<Option<Process>> {
        self.tick();
        self.inner.inspect_block(session, block)
    }
    fn new_split(&self, req: &SplitReq) -> MuxResult<String> {
        self.tick();
        self.inner.new_split(req)
    }
    fn write(&self, session: &str, block: &str, text: &str) -> MuxResult<()> {
        self.tick();
        self.inner.write(session, block, text)
    }
    fn focus_block(&self, session: &str, block: &str) -> MuxResult<()> {
        self.tick();
        self.inner.focus_block(session, block)
    }
    fn apps(&self) -> MuxResult<Vec<crate::mux::ClientInfo>> {
        self.tick();
        self.inner.apps()
    }
    fn select_session(&self, client: &str, session: &str) -> MuxResult<()> {
        self.tick();
        self.inner.select_session(client, session)
    }
}

/// Roda `f` com o Rex contado e grava `<what> <key> <ms>ms <n> calls` no `tui.log`.
fn timed<T>(
    ctx: &Ctx,
    what: &str,
    key: &PrKey,
    f: impl FnOnce(&Ctx) -> FocusResult<T>,
) -> FocusResult<T> {
    let start = Instant::now();
    let counting = Counting {
        inner: ctx.mux,
        n: Cell::new(0),
    };
    let inner = Ctx {
        mux: &counting,
        cfg: ctx.cfg,
        paths: ctx.paths,
        client: ctx.client,
    };
    let r = f(&inner);
    let ms = start.elapsed().as_millis();
    let n = counting.n.get();
    let line = match &r {
        Ok(_) => format!("{what} {key} {ms}ms {n} calls"),
        Err(e) => format!("{what} {key} failed after {ms}ms {n} calls: {e}"),
    };
    let _ = crate::applog::append(&ctx.paths.tui_log_file(), &line);
    r
}

fn lock(ctx: &Ctx, key: &PrKey) -> FocusResult<std::fs::File> {
    locks::pr_lock(ctx.paths, key, LOCK_WAIT).map_err(|e| {
        if e.to_string().contains("is busy") {
            FocusError::Busy(key.clone())
        } else {
            FocusError::Other(e)
        }
    })
}

/// Relê o estado (o snapshot do painel pode estar velho) e confere que o PR ainda pode ser aberto.
fn eligible_record(ctx: &Ctx, key: &PrKey) -> FocusResult<PrRecord> {
    let state = State::read(&ctx.paths.state_file())?;
    let rec = state
        .get(key)
        .cloned()
        .ok_or_else(|| FocusError::Gone(key.clone()))?;
    if !phase_eligible(&rec.phase) {
        return Err(FocusError::NotEligible(format!(
            "{key} is being created or removed"
        )));
    }
    if !rec.path.exists() {
        return Err(FocusError::NotEligible(format!(
            "the worktree of {key} no longer exists"
        )));
    }
    Ok(rec)
}

/// O `client_id` do app com o label configurado: o do cache, ou um `apps()` novo.
fn client_id(ctx: &Ctx, refresh: bool) -> FocusResult<String> {
    if !refresh {
        if let Some(id) = ctx.client.borrow().clone() {
            return Ok(id);
        }
    }
    let label = &ctx.cfg.rex.app_label;
    let found = ctx
        .mux
        .apps()?
        .into_iter()
        .find(|a| a.label.as_deref() == Some(label.as_str()));
    *ctx.client.borrow_mut() = found.as_ref().map(|a| a.id.clone());
    found
        .map(|a| a.id)
        .ok_or_else(|| FocusError::NoApp(label.clone()))
}

/// `Ok(true)`: o app trocou para a sessão. `Ok(false)`: a sessão não existe mais. Um `select`
/// numa sessão inexistente volta como erro interno, então quem decide é o `view`.
fn select(ctx: &Ctx, session: &str) -> FocusResult<bool> {
    let mut r = ctx.mux.select_session(&client_id(ctx, false)?, session);
    if r == Err(MuxError::ClientNotFound) {
        r = ctx.mux.select_session(&client_id(ctx, true)?, session);
    }
    match r {
        Ok(()) => Ok(true),
        Err(e @ (MuxError::ClientNotFound | MuxError::Unreachable(_))) => Err(e.into()),
        Err(e) => match ctx.mux.view(session) {
            Err(MuxError::SessionNotFound) => Ok(false),
            _ => Err(e.into()),
        },
    }
}

/// Cria a sessão do PR com um shell na worktree e grava a entrada no `sessions.json`.
fn create(ctx: &Ctx, key: &PrKey, worktree: &Path) -> FocusResult<String> {
    let c = ctx.mux.create_session(&CreateReq {
        label: session_label(ctx.cfg, key),
        cwd: worktree.to_path_buf(),
        block_label: SHELL_LABEL.into(),
        initial_input: None,
    })?;
    let entry = SessionEntry {
        session_id: c.session_id.clone(),
        shell_block: c.block_id,
        created_at: Utc::now(),
        entered_at: None,
    };
    sessions::upsert(ctx.paths, key, entry)?;
    Ok(c.session_id)
}

fn load_sessions(ctx: &Ctx) -> FocusResult<sessions::Sessions> {
    // corrompido: criar uma sessão que não dá para gravar deixaria uma órfã a cada Enter
    sessions::load(&ctx.paths.sessions_file())
        .map_err(|c| FocusError::Other(anyhow::anyhow!("{c}")))
}

/// O corpo do Enter, com o lock do PR já pego: troca para a sessão, criando se preciso.
fn enter_locked(ctx: &Ctx, key: &PrKey, worktree: &Path) -> FocusResult<Entered> {
    // sem o app de destino não adianta criar sessão
    client_id(ctx, false)?;
    let existing = load_sessions(ctx)?.get(key).map(|e| e.session_id.clone());
    let entered = match existing {
        Some(id) if select(ctx, &id)? => Entered {
            session: id,
            created: false,
        },
        _ => {
            let id = create(ctx, key, worktree)?;
            if !select(ctx, &id)? {
                return Err(MuxError::SessionNotFound.into());
            }
            Entered {
                session: id,
                created: true,
            }
        }
    };
    sessions::mark_entered(ctx.paths, key, Utc::now())?;
    Ok(entered)
}

/// Enter numa review: o app do MacBook passa a mostrar a sessão do PR.
pub fn enter(ctx: &Ctx, key: &PrKey) -> FocusResult<Entered> {
    timed(ctx, "enter", key, |ctx| {
        let _lock = lock(ctx, key)?;
        let rec = eligible_record(ctx, key)?;
        enter_locked(ctx, key, &rec.path)
    })
}

/// O que o combo fez com um dos blocos.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlockAction {
    Opened,
    Relaunched,
    Running,
    /// O bloco existe, mas sem programa nem shell: um `write` não chega a ninguém. O autor fecha o
    /// bloco e o próximo `c` abre outro.
    NoProgram,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Combo {
    pub entered: Entered,
    pub claude: BlockAction,
    pub reviewr: BlockAction,
}

/// Garante o bloco `label` rodando o programa: cria ao lado de `anchor`, relança num shell parado
/// ou deixa como está.
#[allow(clippy::too_many_arguments)]
fn ensure_block(
    mux: &dyn Mux,
    session: &str,
    blocks: &[(BlockInfo, Option<Process>)],
    label: &str,
    anchor: &str,
    ratio: f64,
    argv: &[String],
    worktree: &Path,
) -> FocusResult<(String, BlockAction)> {
    let cmd = command_line(argv, worktree);
    match blocks
        .iter()
        .find(|(b, _)| b.label.as_deref() == Some(label))
    {
        None => {
            let id = mux.new_split(&SplitReq {
                session: session.into(),
                anchor: anchor.into(),
                ratio,
                label: label.into(),
                cwd: worktree.to_path_buf(),
                initial_input: Some(format!("{cmd}\r")),
                focus: false,
            })?;
            Ok((id, BlockAction::Opened))
        }
        Some((b, p)) => Ok((
            b.id.clone(),
            run_in(mux, session, &b.id, p.as_ref(), &cmd, worktree)?,
        )),
    }
}

/// Roda `cmd` num bloco que já existe: escreve no shell parado, ou deixa o programa que roda.
fn run_in(
    mux: &dyn Mux,
    session: &str,
    block: &str,
    p: Option<&Process>,
    cmd: &str,
    worktree: &Path,
) -> FocusResult<BlockAction> {
    match p {
        Some(p) if is_shell(p) => {
            // o programa saiu e o shell pode ter mudado de pasta
            let line = if in_worktree(p, worktree) {
                cmd.to_string()
            } else {
                format!("cd {} && {cmd}", quote(&worktree.to_string_lossy()))
            };
            mux.write(session, block, &format!("{line}\r"))?;
            Ok(BlockAction::Relaunched)
        }
        Some(_) => Ok(BlockAction::Running),
        None => Ok(BlockAction::NoProgram),
    }
}

/// `c`: o Enter, depois o Claude no shell da sessão e o reviewr à direita dele, sem duplicar.
/// Se falhar no meio, o próximo `c` completa o que faltou. O foco termina no Claude.
pub fn combo(ctx: &Ctx, key: &PrKey) -> FocusResult<Combo> {
    timed(ctx, "combo", key, |ctx| {
        let _lock = lock(ctx, key)?;
        let rec = eligible_record(ctx, key)?;
        let entered = enter_locked(ctx, key, &rec.path)?;
        let shell = load_sessions(ctx)?.get(key).map(|e| e.shell_block.clone());
        let blocks = inspect_session(ctx.mux, &entered.session)?;
        // o shell original pode ter sido fechado à mão: ancora no primeiro bloco que sobrou
        let anchor = shell
            .filter(|s| blocks.iter().any(|(b, _)| &b.id == s))
            .or_else(|| blocks.first().map(|(b, _)| b.id.clone()))
            .ok_or(FocusError::Rex(MuxError::Other(
                "the session has no blocks".into(),
            )))?;
        let combo = &ctx.cfg.combo;
        // sessões antigas podem ter um bloco `claude` à parte: segue usando ele
        let has_claude = blocks
            .iter()
            .any(|(b, _)| b.label.as_deref() == Some(CLAUDE_LABEL));
        let (claude, claude_did) = if has_claude {
            ensure_block(
                ctx.mux,
                &entered.session,
                &blocks,
                CLAUDE_LABEL,
                &anchor,
                CLAUDE_RATIO,
                &combo.claude,
                &rec.path,
            )?
        } else {
            // o Claude roda no próprio shell da sessão: o combo tem dois blocos, sem terminal vazio
            let p = blocks
                .iter()
                .find(|(b, _)| b.id == anchor)
                .and_then(|(_, p)| p.as_ref());
            let did = run_in(
                ctx.mux,
                &entered.session,
                &anchor,
                p,
                &command_line(&combo.claude, &rec.path),
                &rec.path,
            )?;
            let did = if did == BlockAction::Relaunched {
                BlockAction::Opened
            } else {
                did
            };
            (anchor.clone(), did)
        };
        let (_, reviewr_did) = ensure_block(
            ctx.mux,
            &entered.session,
            &blocks,
            REVIEWR_LABEL,
            &claude,
            REVIEWR_RATIO,
            &combo.reviewr,
            &rec.path,
        )?;
        ctx.mux.focus_block(&entered.session, &claude)?;
        Ok(Combo {
            entered,
            claude: claude_did,
            reviewr: reviewr_did,
        })
    })
}

/// `f`: entra na review pronta mais antiga. `Ok(None)` quando nenhuma está pronta.
pub fn first_ready(ctx: &Ctx) -> FocusResult<Option<(PrKey, Entered)>> {
    let mut state = State::read(&ctx.paths.state_file())?;
    // worktree apagada desde o último sync: passa para a próxima pronta em vez de falhar nela
    state.prs.retain(|_, r| r.path.exists());
    let Some(key) = pick_first_ready(&state) else {
        return Ok(None);
    };
    let entered = enter(ctx, &key)?;
    Ok(Some((key, entered)))
}

/// Onde o painel cria a worktree de um PR seu: ao lado das de review, com outro nome
/// (`pr-N` é do daemon, que remove o que é dele).
pub fn mine_worktree_path(dir: &Path, key: &PrKey) -> PathBuf {
    dir.join(&key.repo).join(format!("mine-pr-{}", key.number))
}

#[derive(Debug, Clone, PartialEq)]
pub enum MineWorktree {
    /// A branch já tinha worktree (criada aqui ou à mão).
    Existing(PathBuf),
    Created(PathBuf),
}

impl MineWorktree {
    pub fn path(&self) -> &Path {
        match self {
            MineWorktree::Existing(p) | MineWorktree::Created(p) => p,
        }
    }
}

/// A worktree de um PR seu: a que já tem a branch, ou uma nova em `mine_worktree_path`. A
/// worktree é sua: o daemon não acompanha nem remove, e o setup do repo não roda.
pub fn mine_worktree(
    git: &Git,
    runner: &dyn crate::runner::Runner,
    cfg: &Config,
    state: &State,
    key: &PrKey,
) -> anyhow::Result<MineWorktree> {
    use anyhow::{bail, Context};
    state
        .my_prs
        .repos
        .get(&key.repo)
        .and_then(|m| m.prs.iter().find(|p| p.number == key.number))
        .with_context(|| format!("{key} is no longer in the panel"))?;
    // meus PRs não guardam a branch; ela vem dos PRs abertos coletados para as pilhas
    let branch = state
        .open_prs
        .repos
        .get(&key.repo)
        .and_then(|o| o.prs.iter().find(|p| p.number == key.number))
        .map(|p| p.head_ref.clone())
        .with_context(|| {
            format!("the branch of {key} is not known yet; try again after the next sync")
        })?;
    let repo = cfg
        .repo(&key.repo)
        .with_context(|| format!("repo {} is not in the config", key.repo))?;
    if let Some(path) = git.worktree_of_branch(&repo.path, &branch)? {
        if !path.exists() {
            bail!("branch {branch} is checked out in {}, which no longer exists; run `git worktree prune` in the clone", path.display());
        }
        return Ok(MineWorktree::Existing(path));
    }
    let path = mine_worktree_path(&cfg.worktrees_dir, key);
    if path.exists() {
        bail!(
            "{} already exists and is not a worktree of {branch}; check it and remove it by hand",
            path.display()
        );
    }
    git.fetch_branch(&repo.path, &branch)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if git
        .ref_sha(&repo.path, &format!("refs/heads/{branch}"))?
        .is_some()
    {
        // branch local que já existe (talvez com commits seus): usa como está, sem resetar
        git.ok(
            &repo.path,
            &["worktree", "add", "-q", &path.to_string_lossy(), &branch],
        )?;
    } else {
        GitWorktrees::new(runner).create(
            &repo.path,
            &branch,
            &format!("origin/{branch}"),
            &path,
        )?;
        // sem upstream o push pede `-u`; falhar aqui não desfaz a worktree
        let _ = git.set_upstream(&path, &branch);
    }
    Ok(MineWorktree::Created(path))
}

/// Enter num PR seu: garante a worktree da branch e troca o app para a sessão dela.
pub fn enter_mine(
    ctx: &Ctx,
    runner: &dyn crate::runner::Runner,
    key: &PrKey,
) -> FocusResult<(MineWorktree, Entered)> {
    let state = State::read(&ctx.paths.state_file())?;
    let wt = mine_worktree(&Git::new(runner), runner, ctx.cfg, &state, key)?;
    let entered = timed(ctx, "enter", key, |ctx| {
        let _lock = lock(ctx, key)?;
        enter_locked(ctx, key, wt.path())
    })?;
    Ok((wt, entered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fake::*;
    use crate::mux::ClientInfo;

    struct Fx {
        _dir: tempfile::TempDir,
        paths: Paths,
        cfg: Config,
        wt: PathBuf,
        mux: FakeMux,
        client: RefCell<Option<String>>,
    }

    impl Fx {
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                mux: &self.mux,
                cfg: &self.cfg,
                paths: &self.paths,
                client: &self.client,
            }
        }
        fn save(&self, phase: Phase) {
            let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
            rec.path = self.wt.clone();
            rec.phase = phase;
            let mut s = State::default();
            s.insert(rec);
            s.save(&self.paths.state_file()).unwrap();
        }
        fn entry(&self) -> Option<SessionEntry> {
            sessions::load(&self.paths.sessions_file())
                .unwrap()
                .get(&key())
                .cloned()
        }
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.mux.calls.borrow_mut())
        }
        /// Sessão já aberta do PR, com o shell na worktree e entrada no `sessions.json`.
        fn open_session(&self) {
            self.mux.add_session(
                "session:pr",
                "#7 r",
                vec![block("block:sh", "shell", shell_at(&self.wt))],
            );
            let e = SessionEntry {
                session_id: "session:pr".into(),
                shell_block: "block:sh".into(),
                created_at: Utc::now(),
                entered_at: None,
            };
            sessions::upsert(&self.paths, &key(), e).unwrap();
        }
        fn blocks(&self) -> Vec<FakeBlock> {
            let id = self.entry().unwrap().session_id;
            self.mux.sessions.borrow()[&id].blocks.clone()
        }
        fn set_process(&self, label: &str, p: Process) {
            let id = self.entry().unwrap().session_id;
            let mut all = self.mux.sessions.borrow_mut();
            let b = all
                .get_mut(&id)
                .unwrap()
                .blocks
                .iter_mut()
                .find(|b| b.label.as_deref() == Some(label))
                .unwrap();
            b.process = Some(p);
        }
    }

    fn key() -> PrKey {
        PrKey::new("o/r", 7)
    }

    fn block(id: &str, label: &str, p: Process) -> FakeBlock {
        FakeBlock {
            id: id.into(),
            label: Some(label.into()),
            process: Some(p),
            written: vec![],
        }
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let wt = dir.path().join("w t").join("pr-7");
        std::fs::create_dir_all(&wt).unwrap();
        let cfg = Config::parse(
            "worktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/repo\"\nsetup = []\n",
            dir.path(),
        )
        .unwrap();
        let mux = FakeMux::new();
        mux.apps.borrow_mut().push(ClientInfo {
            id: "client:mac".into(),
            label: Some("macbook".into()),
        });
        mux.apps.borrow_mut().push(ClientInfo {
            id: "client:phone".into(),
            label: Some("iphone".into()),
        });
        let f = Fx {
            _dir: dir,
            paths,
            cfg,
            wt,
            mux,
            client: RefCell::new(None),
        };
        f.save(Phase::Ready);
        f
    }

    #[test]
    fn enter_with_open_session_only_selects() {
        let f = fx();
        f.open_session();
        let got = enter(&f.ctx(), &key()).unwrap();
        assert_eq!(
            got,
            Entered {
                session: "session:pr".into(),
                created: false
            }
        );
        assert_eq!(
            f.calls(),
            ["apps", "select_session"],
            "primeiro Enter descobre o app"
        );
        enter(&f.ctx(), &key()).unwrap();
        assert_eq!(
            f.calls(),
            ["select_session"],
            "com o client_id em cache: 1 chamada"
        );
        assert_eq!(
            f.mux.selected.borrow().last().unwrap(),
            &("client:mac".to_string(), "session:pr".to_string())
        );
        assert!(f.entry().unwrap().entered_at.is_some());
        let log = std::fs::read_to_string(f.paths.tui_log_file()).unwrap();
        assert!(
            log.lines().last().unwrap().contains("enter o/r#7 ") && log.contains("ms 1 calls"),
            "{log}"
        );
    }

    #[test]
    fn enter_without_entry_creates_the_session_in_the_worktree() {
        let f = fx();
        let got = enter(&f.ctx(), &key()).unwrap();
        assert!(got.created);
        assert_eq!(f.calls(), ["apps", "create_session", "select_session"]);
        let e = f.entry().unwrap();
        assert_eq!(e.session_id, got.session);
        let s = f.mux.sessions.borrow();
        let session = &s[&got.session];
        assert_eq!(session.label, "#7 r");
        assert_eq!(session.blocks[0].label.as_deref(), Some("shell"));
        assert_eq!(e.shell_block, session.blocks[0].id);
        assert_eq!(
            session.blocks[0].process.as_ref().unwrap().cwd.as_deref(),
            Some(f.wt.as_path())
        );
        assert!(e.entered_at.is_some());
    }

    #[test]
    fn stale_client_is_refreshed_once() {
        let f = fx();
        f.open_session();
        *f.client.borrow_mut() = Some("client:velho".into());
        enter(&f.ctx(), &key()).unwrap();
        assert_eq!(f.calls(), ["select_session", "apps", "select_session"]);
        assert_eq!(f.client.borrow().as_deref(), Some("client:mac"));
        // o app saiu de vez entre as duas chamadas: não tenta uma terceira vez
        f.mux.fail_on("select_session", MuxError::ClientNotFound);
        assert!(matches!(
            enter(&f.ctx(), &key()),
            Err(FocusError::Rex(MuxError::ClientNotFound))
        ));
        assert_eq!(f.calls(), ["select_session", "apps", "select_session"]);
    }

    #[test]
    fn session_closed_by_hand_is_created_again() {
        let f = fx();
        f.open_session();
        f.mux.sessions.borrow_mut().remove("session:pr");
        let got = enter(&f.ctx(), &key()).unwrap();
        assert!(got.created);
        assert_eq!(
            f.calls(),
            [
                "apps",
                "select_session",
                "view",
                "create_session",
                "select_session"
            ]
        );
        assert_eq!(f.entry().unwrap().session_id, got.session);
    }

    #[test]
    fn other_select_error_with_live_session_is_reported() {
        let f = fx();
        f.open_session();
        f.mux
            .fail_on("select_session", MuxError::Other("boom".into()));
        let e = enter(&f.ctx(), &key()).unwrap_err();
        assert!(
            matches!(&e, FocusError::Rex(MuxError::Other(m)) if m == "boom"),
            "{e}"
        );
        assert_eq!(f.calls(), ["apps", "select_session", "view"]);
        assert_eq!(
            f.entry().unwrap().session_id,
            "session:pr",
            "não recria uma sessão que existe"
        );
        let log = std::fs::read_to_string(f.paths.tui_log_file()).unwrap();
        assert!(log.contains("enter o/r#7 failed after"), "{log}");
    }

    #[test]
    fn no_app_with_the_label_never_switches_another_app() {
        let f = fx();
        f.mux
            .apps
            .borrow_mut()
            .retain(|a| a.label.as_deref() != Some("macbook"));
        let e = enter(&f.ctx(), &key()).unwrap_err();
        assert!(matches!(&e, FocusError::NoApp(l) if l == "macbook"));
        assert!(e.to_string().contains("macbook not connected"), "{e}");
        assert!(f.mux.selected.borrow().is_empty());
        assert!(f.entry().is_none(), "sem app, nada é criado");
    }

    #[test]
    fn busy_pr_lock_returns_busy() {
        let f = fx();
        let held = locks::pr_lock(&f.paths, &key(), Duration::ZERO).unwrap();
        assert!(matches!(enter(&f.ctx(), &key()), Err(FocusError::Busy(_))));
        assert!(matches!(combo(&f.ctx(), &key()), Err(FocusError::Busy(_))));
        assert!(f.calls().is_empty());
        drop(held);
    }

    #[test]
    fn ineligible_or_vanished_prs_are_refused() {
        let f = fx();
        for phase in [Phase::Creating, Phase::Removing] {
            f.save(phase);
            let e = enter(&f.ctx(), &key()).unwrap_err();
            assert!(matches!(e, FocusError::NotEligible(_)), "{e}");
        }
        f.save(Phase::Failed {
            step: "bundle".into(),
            reason: "x".into(),
        });
        assert!(enter(&f.ctx(), &key()).is_ok(), "falha de setup ainda abre");
        std::fs::remove_dir_all(&f.wt).unwrap();
        assert!(enter(&f.ctx(), &key())
            .unwrap_err()
            .to_string()
            .contains("no longer exists"));
        assert!(matches!(
            enter(&f.ctx(), &PrKey::new("o/r", 8)),
            Err(FocusError::Gone(_))
        ));
    }

    #[test]
    fn corrupt_sessions_file_creates_nothing() {
        let f = fx();
        std::fs::write(f.paths.sessions_file(), "{ lixo").unwrap();
        assert!(enter(&f.ctx(), &key())
            .unwrap_err()
            .to_string()
            .contains("corrupt"));
        assert!(!f.calls().contains(&"create_session".to_string()));
    }

    #[test]
    fn combo_opens_claude_and_reviewr_and_focuses_claude() {
        let f = fx();
        f.open_session();
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            (got.claude, got.reviewr),
            (BlockAction::Opened, BlockAction::Opened)
        );
        let b = f.blocks();
        let labels: Vec<_> = b.iter().map(|b| b.label.clone().unwrap()).collect();
        assert_eq!(
            labels,
            ["shell", "reviewr"],
            "o Claude roda no shell: sem terminal vazio"
        );
        assert_eq!(b[0].written, ["claude\r"]);
        assert_eq!(b[1].written, [format!("reviewr '{}'\r", f.wt.display())]);
        assert_eq!(f.mux.focused.borrow().as_slice(), [b[0].id.clone()]);
        let calls = f.calls();
        assert_eq!(calls.iter().filter(|c| *c == "new_split").count(), 1);
        assert_eq!(calls.last().unwrap(), "focus_block");
    }

    #[test]
    fn combo_splits_use_the_right_anchors_and_ratios() {
        // o FakeMux não guarda a geometria: confere nos SplitReq por um Mux que só grava
        struct Spy<'a>(&'a FakeMux, RefCell<Vec<SplitReq>>);
        impl Mux for Spy<'_> {
            fn server_api_revision(&self) -> MuxResult<u32> {
                self.0.server_api_revision()
            }
            fn list_sessions(&self) -> MuxResult<Vec<crate::mux::SessionInfo>> {
                self.0.list_sessions()
            }
            fn create_session(&self, r: &CreateReq) -> MuxResult<crate::mux::Created> {
                self.0.create_session(r)
            }
            fn view(&self, s: &str) -> MuxResult<Vec<BlockInfo>> {
                self.0.view(s)
            }
            fn session_of(&self, b: &str) -> MuxResult<String> {
                self.0.session_of(b)
            }
            fn destroy_session(&self, s: &str) -> MuxResult<()> {
                self.0.destroy_session(s)
            }
            fn inspect_block(&self, s: &str, b: &str) -> MuxResult<Option<Process>> {
                self.0.inspect_block(s, b)
            }
            fn new_split(&self, r: &SplitReq) -> MuxResult<String> {
                self.1.borrow_mut().push(r.clone());
                self.0.new_split(r)
            }
            fn write(&self, s: &str, b: &str, t: &str) -> MuxResult<()> {
                self.0.write(s, b, t)
            }
            fn focus_block(&self, s: &str, b: &str) -> MuxResult<()> {
                self.0.focus_block(s, b)
            }
            fn apps(&self) -> MuxResult<Vec<ClientInfo>> {
                self.0.apps()
            }
            fn select_session(&self, c: &str, s: &str) -> MuxResult<()> {
                self.0.select_session(c, s)
            }
        }
        let f = fx();
        f.open_session();
        let spy = Spy(&f.mux, RefCell::new(vec![]));
        let ctx = Ctx {
            mux: &spy,
            cfg: &f.cfg,
            paths: &f.paths,
            client: &f.client,
        };
        combo(&ctx, &key()).unwrap();
        let splits = spy.1.borrow();
        assert_eq!(splits.len(), 1);
        assert_eq!(
            (
                splits[0].anchor.as_str(),
                splits[0].ratio,
                splits[0].label.as_str()
            ),
            ("block:sh", 0.45, "reviewr")
        );
        assert!(splits.iter().all(|s| s.cwd == f.wt && !s.focus));
    }

    #[test]
    fn combo_relaunches_exited_programs_and_leaves_running_ones() {
        let f = fx();
        f.open_session();
        combo(&f.ctx(), &key()).unwrap();
        // Claude saiu (`/exit`): o foreground voltou a ser o shell. O reviewr segue rodando.
        f.set_process("shell", shell_at(&f.wt));
        f.set_process("reviewr", program_at("reviewr", &f.wt));
        f.calls();
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            (got.claude, got.reviewr),
            (BlockAction::Opened, BlockAction::Running)
        );
        let b = f.blocks();
        assert_eq!(b.len(), 2, "não duplica");
        assert_eq!(b[0].written, ["claude\r", "claude\r"]);
        assert_eq!(b[1].written.len(), 1);
        assert!(!f.calls().contains(&"new_split".to_string()));

        // shell parado fora da worktree: volta para ela antes de relançar
        f.set_process("shell", shell_at(Path::new("/tmp")));
        combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            f.blocks()[0].written.last().unwrap(),
            &format!("cd '{}' && claude\r", f.wt.display())
        );
    }

    #[test]
    fn repeated_combo_with_programs_running_does_nothing_but_focus() {
        let f = fx();
        f.open_session();
        combo(&f.ctx(), &key()).unwrap();
        f.set_process("shell", program_at("2.1.332", &f.wt));
        f.set_process("reviewr", program_at("reviewr", &f.wt));
        f.calls();
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            (got.claude, got.reviewr),
            (BlockAction::Running, BlockAction::Running)
        );
        let calls = f.calls();
        assert!(
            !calls.iter().any(|c| c == "new_split" || c == "write"),
            "{calls:?}"
        );
        assert_eq!(f.blocks().len(), 2);
    }

    #[test]
    fn combo_that_fails_midway_is_completed_by_the_next_c() {
        let f = fx();
        f.open_session();
        f.mux
            .fail_on_call("new_split", 1, MuxError::Other("timeout".into()));
        assert!(combo(&f.ctx(), &key()).is_err());
        assert_eq!(f.blocks().len(), 1, "só o claude abriu, no shell");
        f.set_process("shell", program_at("claude", &f.wt));
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            (got.claude, got.reviewr),
            (BlockAction::Running, BlockAction::Opened)
        );
        let labels: Vec<_> = f
            .blocks()
            .iter()
            .map(|b| b.label.clone().unwrap())
            .collect();
        assert_eq!(labels, ["shell", "reviewr"]);
    }

    #[test]
    fn combo_without_session_creates_it_first() {
        let f = fx();
        let got = combo(&f.ctx(), &key()).unwrap();
        assert!(got.entered.created);
        assert_eq!(f.blocks().len(), 2);
        assert_eq!(f.blocks()[0].id, f.entry().unwrap().shell_block);
    }

    #[test]
    fn combo_anchors_on_the_remaining_block_when_the_shell_was_closed() {
        let f = fx();
        f.mux.add_session(
            "session:pr",
            "#7 r",
            vec![block("block:outro", "notas", shell_at(&f.wt))],
        );
        let e = SessionEntry {
            session_id: "session:pr".into(),
            shell_block: "block:sumiu".into(),
            created_at: Utc::now(),
            entered_at: None,
        };
        sessions::upsert(&f.paths, &key(), e).unwrap();
        combo(&f.ctx(), &key()).unwrap();
        let b = f.blocks();
        assert_eq!(b.len(), 2);
        assert_eq!(
            b[0].written,
            ["claude\r"],
            "o Claude roda no bloco que sobrou"
        );
    }

    #[test]
    fn combo_keeps_using_a_separate_claude_block_from_older_sessions() {
        let f = fx();
        f.mux.add_session(
            "session:pr",
            "#7 r",
            vec![
                block("block:sh", "shell", shell_at(&f.wt)),
                block("block:cl", "claude", program_at("claude", &f.wt)),
            ],
        );
        let e = SessionEntry {
            session_id: "session:pr".into(),
            shell_block: "block:sh".into(),
            created_at: Utc::now(),
            entered_at: None,
        };
        sessions::upsert(&f.paths, &key(), e).unwrap();
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(got.claude, BlockAction::Running);
        let b = f.blocks();
        assert!(
            b[0].written.is_empty(),
            "não roda um segundo Claude no shell"
        );
        assert_eq!(b.len(), 3);
        assert_eq!(f.mux.focused.borrow().as_slice(), ["block:cl".to_string()]);
    }

    #[test]
    fn quoting_handles_spaces_quotes_and_the_worktree() {
        assert_eq!(quote("claude"), "claude");
        assert_eq!(quote("--model=opus"), "--model=opus");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        let argv: Vec<String> = ["my tool", "it's", "--dir={worktree}", "{worktree}"]
            .map(String::from)
            .into();
        assert_eq!(
            command_line(&argv, Path::new("/w/a b")),
            "'my tool' 'it'\\''s' '--dir=/w/a b' '/w/a b'"
        );
    }

    #[test]
    fn session_label_names_the_owner_only_with_several_owners() {
        let f = fx();
        assert_eq!(session_label(&f.cfg, &key()), "#7 r");
        let two = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/a\"\nsetup = []\n[[repos]]\nname = \"p/x\"\npath = \"/b\"\nsetup = []\n";
        let cfg = Config::parse(two, Path::new("/h")).unwrap();
        assert_eq!(session_label(&cfg, &key()), "o/r#7");
    }

    #[test]
    fn first_ready_enters_the_oldest_ready_review() {
        let f = fx();
        assert_eq!(first_ready(&f.ctx()).unwrap().map(|(k, _)| k), Some(key()));
        assert!(f.entry().is_some());
        f.save(Phase::Preparing);
        assert!(first_ready(&f.ctx()).unwrap().is_none());
    }

    #[test]
    fn first_ready_skips_a_review_whose_worktree_vanished() {
        let f = fx();
        let mut s = State::read(&f.paths.state_file()).unwrap();
        let mut gone = PrRecord::fixture("o/r", 3, "x", "s"); // epoch: o mais antigo
        gone.path = f.wt.with_file_name("sumiu");
        s.insert(gone);
        let mut newer = s.get(&key()).unwrap().clone();
        newer.first_seen_at = Utc::now();
        s.insert(newer);
        s.save(&f.paths.state_file()).unwrap();
        assert_eq!(first_ready(&f.ctx()).unwrap().map(|(k, _)| k), Some(key()));
    }

    #[test]
    fn combo_reports_a_block_left_without_a_program() {
        let f = fx();
        f.open_session();
        combo(&f.ctx(), &key()).unwrap();
        let id = f.entry().unwrap().session_id;
        f.mux.sessions.borrow_mut().get_mut(&id).unwrap().blocks[0].process = None;
        f.set_process("reviewr", program_at("reviewr", &f.wt));
        let got = combo(&f.ctx(), &key()).unwrap();
        assert_eq!(
            (got.claude, got.reviewr),
            (BlockAction::NoProgram, BlockAction::Running)
        );
        assert_eq!(
            f.blocks()[0].written.len(),
            1,
            "não escreve num bloco sem programa"
        );
    }

    #[test]
    fn first_ready_is_oldest_managed_pending_ready() {
        let mut s = State::default();
        let mut a = PrRecord::fixture("o/r", 2, "x", "s");
        a.first_seen_at = Utc::now();
        let b = PrRecord::fixture("o/r", 3, "x", "s"); // epoch: mais antigo
        let mut c = PrRecord::fixture("o/r", 1, "x", "s");
        c.phase = Phase::Preparing;
        for r in [a, b, c] {
            s.insert(r);
        }
        assert_eq!(pick_first_ready(&s), Some(PrKey::new("o/r", 3)));
        assert_eq!(pick_first_ready(&State::default()), None);
    }

    /// Estado com o PR seu #9 na branch `feat/x` (dado fictício).
    fn mine_state(branch: Option<&str>) -> State {
        use crate::state::{MyPr, MyPrsRepo, OpenPr, OpenPrsRepo};
        let mut s = State::default();
        let now = Utc::now();
        let pr = MyPr {
            number: 9,
            title: "Ajusta o cache".into(),
            url: "https://github.com/o/r/pull/9".into(),
            draft: false,
            created_at: now,
            head_sha: "m".into(),
            ci: None,
        };
        s.my_prs.repos.insert(
            "o/r".into(),
            MyPrsRepo {
                prs: vec![pr],
                opened_24h: 1,
                as_of: now,
            },
        );
        if let Some(b) = branch {
            let open = OpenPr {
                number: 9,
                head_ref: b.into(),
                base_ref: "main".into(),
            };
            s.open_prs.repos.insert(
                "o/r".into(),
                OpenPrsRepo {
                    prs: vec![open],
                    default_branch: Some("main".into()),
                    as_of: now,
                },
            );
        }
        s
    }

    fn mine_fx(state: &State) -> (crate::testutil::Fixture, Fx) {
        let git_fx = crate::testutil::fixture("feat/x");
        let mut f = fx();
        f.cfg = crate::testutil::config(&git_fx, "true");
        state.save(&f.paths.state_file()).unwrap();
        (git_fx, f)
    }

    #[test]
    fn enter_mine_creates_worktree_tracking_origin_and_opens_a_session() {
        use crate::runner::RealRunner;
        let (git_fx, f) = mine_fx(&mine_state(Some("feat/x")));
        let key = PrKey::new("o/r", 9);
        let (wt, entered) = enter_mine(&f.ctx(), &RealRunner, &key).unwrap();
        let path = mine_worktree_path(&f.cfg.worktrees_dir, &key);
        assert_eq!(wt, MineWorktree::Created(path.clone()));
        assert!(entered.created);
        assert_eq!(
            crate::testutil::git(&path, &["rev-parse", "HEAD"]),
            git_fx.remote_sha("feat/x")
        );
        assert_eq!(
            crate::testutil::git(&path, &["rev-parse", "--abbrev-ref", "@{u}"]),
            "origin/feat/x"
        );
        let shell = &f.mux.sessions.borrow()[&entered.session].blocks[0];
        assert_eq!(
            shell.process.as_ref().unwrap().cwd.as_deref(),
            Some(path.as_path())
        );

        // segundo Enter: mesma worktree, mesma sessão
        let (again, entered2) = enter_mine(&f.ctx(), &RealRunner, &key).unwrap();
        assert_eq!(again, MineWorktree::Existing(path));
        assert_eq!(
            entered2,
            Entered {
                session: entered.session,
                created: false
            }
        );
    }

    #[test]
    fn enter_mine_uses_a_worktree_made_by_hand_and_keeps_local_branch() {
        use crate::runner::RealRunner;
        let (git_fx, f) = mine_fx(&mine_state(Some("feat/x")));
        let g = Git::new(&RealRunner);
        g.fetch_branch(&git_fx.clone, "feat/x").unwrap();
        let mine = git_fx.root.join("meu-wt");
        crate::testutil::git(
            &git_fx.clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/x",
                mine.to_str().unwrap(),
                "origin/feat/x",
            ],
        );
        let (wt, _) = enter_mine(&f.ctx(), &RealRunner, &PrKey::new("o/r", 9)).unwrap();
        assert_eq!(wt.path(), mine.as_path());
        assert!(!mine_worktree_path(&f.cfg.worktrees_dir, &PrKey::new("o/r", 9)).exists());
    }

    #[test]
    fn enter_mine_reuses_existing_local_branch_without_resetting_it() {
        use crate::runner::RealRunner;
        let (git_fx, f) = mine_fx(&mine_state(Some("feat/x")));
        Git::new(&RealRunner)
            .fetch_branch(&git_fx.clone, "feat/x")
            .unwrap();
        let main = crate::testutil::git(&git_fx.clone, &["rev-parse", "main"]);
        crate::testutil::git(&git_fx.clone, &["branch", "feat/x", &main]);
        let key = PrKey::new("o/r", 9);
        let (wt, _) = enter_mine(&f.ctx(), &RealRunner, &key).unwrap();
        assert_eq!(
            wt,
            MineWorktree::Created(mine_worktree_path(&f.cfg.worktrees_dir, &key))
        );
        assert_eq!(
            crate::testutil::git(wt.path(), &["rev-parse", "HEAD"]),
            main,
            "branch local fica como estava"
        );
    }

    #[test]
    fn enter_mine_refuses_unknown_branch_vanished_pr_and_foreign_dir() {
        use crate::runner::RealRunner;
        let (_git_fx, f) = mine_fx(&mine_state(None));
        let key = PrKey::new("o/r", 9);
        let e = enter_mine(&f.ctx(), &RealRunner, &key).unwrap_err();
        assert!(e.to_string().contains("not known yet"), "{e}");
        mine_state(Some("feat/x"))
            .save(&f.paths.state_file())
            .unwrap();
        let e = enter_mine(&f.ctx(), &RealRunner, &PrKey::new("o/r", 8)).unwrap_err();
        assert!(e.to_string().contains("no longer in the panel"), "{e}");
        let path = mine_worktree_path(&f.cfg.worktrees_dir, &key);
        std::fs::create_dir_all(&path).unwrap();
        let e = enter_mine(&f.ctx(), &RealRunner, &key).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        assert!(f.mux.selected.borrow().is_empty());
    }
}
