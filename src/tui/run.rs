use crate::config::Config;
use crate::focus::{self, BlockAction, MineWorktree};
use crate::mux::{Mux, SplitReq};
use crate::paths::Paths;
use crate::pst::{self, Record};
use crate::requests;
use crate::rex::RexCli;
use crate::runner::{RealRunner, Runner};
use crate::sessions::{self, Sessions};
use crate::state::{PrKey, State};
use crate::tui::keys::{handle, Action, Key};
use crate::tui::model::{reselect, RowId, Sent, UiFacts, UiState, View, ViewConfig};
use anyhow::{bail, Result};
use std::cell::RefCell;
use std::collections::BTreeMap;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);
/// Último sinal recebido (para o motivo da saída no `tui.log`).
static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
    STOP.store(true, Ordering::SeqCst);
}

/// Por que o TUI saiu sem erro; vai para o `tui.log`.
#[derive(Debug, Clone, PartialEq)]
pub enum Exit {
    /// Tecla de saída (`q`, `Esc`, `Ctrl-C`).
    Key(String),
    /// SIGINT, SIGTERM ou SIGHUP.
    Signal(i32),
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Key(k) => write!(f, "key {k}"),
            Exit::Signal(libc::SIGINT) => f.write_str("signal SIGINT"),
            Exit::Signal(libc::SIGTERM) => f.write_str("signal SIGTERM"),
            Exit::Signal(libc::SIGHUP) => f.write_str("signal SIGHUP"),
            Exit::Signal(libc::SIGABRT) => f.write_str("signal SIGABRT"),
            Exit::Signal(n) => write!(f, "signal {n}"),
        }
    }
}

fn key_name(k: &Key) -> String {
    match k {
        Key::Up => "↑".into(),
        Key::Down => "↓".into(),
        Key::Enter => "Enter".into(),
        Key::Esc => "Esc".into(),
        Key::Backspace => "Backspace".into(),
        Key::CtrlC => "Ctrl-C".into(),
        Key::CtrlU => "Ctrl-U".into(),
        Key::Char(c) => c.to_string(),
    }
}

/// Registra no `tui.log` como o TUI terminou. O pane fecha junto com o processo e leva o stderr,
/// então este é o único rastro de um erro do TUI.
pub fn log_outcome(log: &Path, r: &Result<Exit>) {
    let msg = match r {
        Ok(exit) => format!("tui stopped: {exit}"),
        Err(e) => format!("tui stopped with an error: {e:#}"),
    };
    let _ = crate::applog::append(log, &msg);
}

/// Braço `tui` do `main`: registra início e fim (ou erro) no `tui.log` e devolve o erro ao `main`.
pub fn run_logged(paths: Paths, load_cfg: impl FnOnce() -> Result<Config>) -> Result<()> {
    let log = paths.tui_log_file();
    let _ = crate::applog::append(&log, &format!("tui started (pid {})", std::process::id()));
    let r = load_cfg().and_then(|cfg| run(paths, cfg));
    log_outcome(&log, &r);
    r.map(|_| ())
}

pub fn key_from_event(e: &Event) -> Option<Key> {
    // `Event::Paste` (bracketed paste) e qualquer outro evento que não seja tecla são ignorados
    let Event::Key(k) = e else { return None };
    // `Repeat` (com REPORT_EVENT_TYPES) só vale para rolar a lista segurando a tecla; ações nunca repetem
    // (backspace também repete: segurar apaga a busca; fora dela não faz nada)
    let scroll = matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') | KeyCode::Backspace)
        && !k.modifiers.contains(KeyModifiers::CONTROL);
    match k.kind {
        KeyEventKind::Press => {}
        KeyEventKind::Repeat if scroll => {}
        _ => return None,
    }
    match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlU),
        // outras combinações com ctrl/alt não viram letra (sujariam a busca)
        KeyCode::Char(_) if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => None,
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

pub fn log_command(path: &Path) -> String {
    format!("less +F '{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

// Etapas do terminal já ativadas; `restore_terminal` desfaz só estas, na ordem inversa.
const RAW: u8 = 1;
const ALT: u8 = 2;
const HIDDEN: u8 = 4;
const PASTE: u8 = 8;
const KBD: u8 = 16;
static ACTIVE: AtomicU8 = AtomicU8::new(0);

fn mark(step: u8) {
    ACTIVE.fetch_or(step, Ordering::SeqCst);
}

/// Idempotente (o hook de pânico e o `Drop` podem chamar os dois).
fn restore_terminal() {
    let active = ACTIVE.swap(0, Ordering::SeqCst);
    let mut out = std::io::stdout();
    if active & KBD != 0 {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if active & PASTE != 0 {
        let _ = execute!(out, DisableBracketedPaste);
    }
    if active & HIDDEN != 0 {
        let _ = execute!(out, cursor::Show);
    }
    if active & ALT != 0 {
        let _ = execute!(out, LeaveAlternateScreen);
    }
    if active & RAW != 0 {
        let _ = disable_raw_mode();
    }
}

struct TerminalGuard;

impl TerminalGuard {
    /// O guard existe logo após o raw mode: se uma etapa seguinte falhar, o `Drop` desfaz as anteriores.
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        mark(RAW);
        let guard = TerminalGuard;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen)?;
        mark(ALT);
        execute!(out, cursor::Hide)?;
        mark(HIDDEN);
        execute!(out, EnableBracketedPaste)?;
        mark(PASTE);
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES))?;
            mark(KBD);
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn osc52(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", crate::pst::base64(text.as_bytes()));
    let _ = out.flush();
}

/// O que o worker precisa para executar uma ação.
struct WorkerCtx<'a> {
    mux: &'a dyn Mux,
    runner: &'a dyn Runner,
    cfg: &'a Config,
    paths: &'a Paths,
    /// `client_id` do app do Rex, refeito quando o Rex diz que ele sumiu.
    client: RefCell<Option<String>>,
    /// `REX_BLOCK` do próprio TUI (âncora do split do log).
    block: Option<String>,
}

impl WorkerCtx<'_> {
    fn focus(&self) -> focus::Ctx<'_> {
        focus::Ctx { mux: self.mux, cfg: self.cfg, paths: self.paths, client: &self.client }
    }
}

/// Resultado de uma ação: a mensagem do rodapé e, depois de um Enter, o PR em que o autor entrou
/// (o loop relê o `sessions.json` na hora, para o aviso do PR sumir sem esperar o mtime).
#[derive(Debug, Default, PartialEq)]
struct Done {
    msg: String,
    entered: Option<PrKey>,
}

impl Done {
    fn msg(m: impl Into<String>) -> Self {
        Done { msg: m.into(), entered: None }
    }
    fn entered(key: &PrKey, m: impl Into<String>) -> Self {
        Done { msg: m.into(), entered: Some(key.clone()) }
    }
}

/// Executa `f` e transforma um pânico em mensagem, para o worker sobreviver.
fn guarded(f: impl FnOnce() -> Done) -> Done {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(done) => done,
        Err(p) => {
            let why = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            Done::msg(format!("the action failed: {why}"))
        }
    }
}

fn did(a: BlockAction) -> &'static str {
    match a {
        BlockAction::Opened => "opened",
        BlockAction::Relaunched => "relaunched",
        BlockAction::Running => "already running",
        BlockAction::NoProgram => "block has no program (close it and press c again)",
    }
}

/// Uma ação do worker. O Enter e o combo relêem o estado sob o lock do PR; URL e log relêem o
/// estado do disco (o snapshot do loop pode estar velho).
fn perform(action: &Action, ctx: &WorkerCtx) -> Done {
    let fctx = ctx.focus();
    match action {
        Action::Focus(key) => match focus::enter(&fctx, key) {
            Ok(e) if e.created => Done::entered(key, format!("opened a new session for {key}")),
            Ok(_) => Done::entered(key, format!("opening {key}")),
            Err(e) => Done::msg(e.to_string()),
        },
        Action::Combo(key) => match focus::combo(&fctx, key) {
            Ok(c) => Done::entered(key, format!("{key}: claude {}, reviewr {}", did(c.claude), did(c.reviewr))),
            Err(e) => Done::msg(e.to_string()),
        },
        Action::FirstReady => match focus::first_ready(&fctx) {
            Ok(Some((key, _))) => Done::entered(&key, format!("opening {key}")),
            Ok(None) => Done::msg("no review is ready"),
            Err(e) => Done::msg(e.to_string()),
        },
        Action::OpenMine(key) => match focus::enter_mine(&fctx, ctx.runner, key) {
            Ok((MineWorktree::Created(path), _)) => Done::entered(key, format!("created the worktree of {key} at {}", path.display())),
            Ok((MineWorktree::Existing(_), _)) => Done::entered(key, format!("opening {key}")),
            Err(e) => Done::msg(e.to_string()),
        },
        Action::OpenUrl(..) | Action::ShowLog(_) => Done::msg(url_or_log(action, ctx)),
        Action::Request(..) | Action::Quit => Done::default(),
    }
}

fn url_or_log(action: &Action, ctx: &WorkerCtx) -> String {
    let key = match action {
        Action::ShowLog(key) => key,
        Action::OpenUrl(id, _) => id.key(),
        _ => return String::new(),
    };
    let state = match State::read(&ctx.paths.state_file()) {
        Ok(s) => s,
        Err(e) => return format!("could not read the state: {e:#}"),
    };
    // cada linha procura na sua coleção: um meu PR nunca resolve para um registro de review
    let url = match action {
        Action::OpenUrl(RowId::Mine(k), _) => state
            .my_prs
            .repos
            .get(&k.repo)
            .and_then(|m| m.prs.iter().find(|p| p.number == k.number))
            .map(|p| p.url.clone()),
        _ => state.get(key).map(|r| r.url.clone()),
    };
    let Some(url) = url else { return format!("{key} is no longer in the panel") };
    if let Action::OpenUrl(..) = action {
        // o loop já mandou o OSC 52: o clipboard é o do app do Rex, não o desta máquina
        return format!("URL sent to the terminal clipboard: {url}");
    }
    let log = ctx.paths.setup_log(key);
    if !log.exists() {
        return "no log yet".into();
    }
    match &ctx.block {
        None => format!("log at {}", log.display()),
        Some(block) => show_log(ctx.mux, block, &log),
    }
}

/// Abre o log num bloco à direita do painel, com `less +F`.
fn show_log(mux: &dyn Mux, block: &str, log: &Path) -> String {
    let split = mux.session_of(block).and_then(|session| {
        mux.new_split(&SplitReq {
            session,
            anchor: block.into(),
            ratio: 0.5,
            label: "log".into(),
            cwd: log.parent().unwrap_or(log).to_path_buf(),
            initial_input: Some(format!("{}\r", log_command(log))),
            focus: true,
        })
    });
    match split {
        Ok(_) => "log opened on the right".into(),
        Err(e) => format!("could not open the log split ({e}); log at {}", log.display()),
    }
}

/// Ações enviadas ao worker e ainda não terminadas (na fila ou rodando).
#[derive(Clone, Default)]
struct Pending(Arc<Mutex<Vec<Action>>>);

impl Pending {
    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Action>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `false` se uma ação idêntica já está pendente.
    fn claim(&self, action: &Action) -> bool {
        let mut list = self.list();
        if list.contains(action) {
            return false;
        }
        list.push(action.clone());
        true
    }

    fn done(&self, action: &Action) {
        let mut list = self.list();
        if let Some(i) = list.iter().position(|a| a == action) {
            list.remove(i);
        }
    }
}

/// Fila de ações do TUI para o worker, sem duplicatas.
struct ActionQueue {
    tx: mpsc::Sender<Action>,
    pending: Pending,
}

impl ActionQueue {
    fn new() -> (Self, mpsc::Receiver<Action>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx, pending: Pending::default() }, rx)
    }

    /// `Ok(false)`: descartada, porque uma idêntica ainda está pendente ou rodando.
    fn submit(&self, action: Action) -> Result<bool> {
        if !self.pending.claim(&action) {
            return Ok(false);
        }
        if self.tx.send(action.clone()).is_err() {
            self.pending.done(&action);
            bail!("the TUI action worker stopped");
        }
        Ok(true)
    }
}

fn spawn_worker(cfg: Config, paths: Paths) -> (ActionQueue, mpsc::Receiver<Done>) {
    let (queue, rx) = ActionQueue::new();
    let pending = queue.pending.clone();
    let (back, msgs) = mpsc::channel::<Done>();
    std::thread::Builder::new()
        .name("tui-worker".into())
        .spawn(move || {
            let runner = RealRunner;
            let rex = RexCli::new(&runner, cfg.rex.bin.clone());
            let ctx = WorkerCtx {
                mux: &rex,
                runner: &runner,
                cfg: &cfg,
                paths: &paths,
                client: RefCell::new(None),
                block: std::env::var("REX_BLOCK").ok().filter(|b| !b.is_empty()),
            };
            for action in rx {
                let done = guarded(|| perform(&action, &ctx));
                pending.done(&action);
                if back.send(done).is_err() {
                    break;
                }
            }
        })
        .expect("could not create the TUI worker thread");
    (queue, msgs)
}

/// Estado lido do disco pelo loop, relido quando o mtime muda ou quando a última leitura falhou.
#[derive(Default)]
struct StateWatch {
    state: Option<State>,
    mtime: Option<SystemTime>,
    read_error: Option<String>,
}

impl StateWatch {
    fn check(&mut self, file: &Path) {
        let m = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        // com erro de leitura, tenta de novo mesmo sem mudança de mtime (o conserto pode ter caído no mesmo instante)
        if m == self.mtime && self.state.is_some() && self.read_error.is_none() {
            return;
        }
        self.mtime = m;
        match State::read(file) {
            Ok(s) => {
                self.state = if m.is_some() { Some(s) } else { None };
                self.read_error = None;
            }
            Err(e) => self.read_error = Some(format!("unreadable state: {e:#}")),
        }
    }
}

/// `sessions.json` lido pelo loop: relido quando o mtime muda e depois de cada Enter.
#[derive(Default)]
struct SessionsWatch {
    sessions: Sessions,
    mtime: Option<SystemTime>,
    loaded: bool,
    /// Arquivo corrompido: o painel trata como vazio e mostra o erro (só o daemon isola o arquivo).
    error: Option<String>,
}

impl SessionsWatch {
    fn check(&mut self, file: &Path, force: bool) {
        let m = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        if !force && self.loaded && m == self.mtime {
            return;
        }
        self.mtime = m;
        self.loaded = true;
        let (sessions, error) = sessions::load_for_panel(file);
        self.sessions = sessions;
        self.error = error;
    }
}

/// Os registros 7501 já escritos no terminal, para escrever só a diferença.
struct Emitter {
    root: Option<Record>,
    records: BTreeMap<String, Record>,
}

impl Emitter {
    /// Na partida, apaga tudo o que um painel anterior deixou neste bloco e começa do zero.
    fn start() -> (Self, String) {
        (Emitter { root: None, records: BTreeMap::new() }, pst::clear(None))
    }

    /// O que escrever para o terminal passar a refletir `state` e `sessions`.
    fn step(&mut self, state: &State, sessions: &Sessions) -> Vec<String> {
        let root = pst::root_for(&pst::active_conditions(state, false));
        let records = pst::records_for(state, sessions);
        let mut out = vec![];
        if self.root.as_ref() != Some(&root) {
            out.push(pst::report(None, &root));
            self.root = Some(root);
        }
        out.extend(pst::diff(&self.records, &records));
        self.records = records;
        out
    }
}

/// Escreve sequências OSC no terminal. Com o pty fechado a escrita falha: nada a fazer.
fn emit(out: &mut dyn Write, seqs: &[String]) {
    for s in seqs {
        let _ = out.write_all(s.as_bytes());
    }
    let _ = out.flush();
}

/// Se este painel emite o 7501 (segura o `panel.lock`). Lido no hook de pânico e no watchdog.
static EMITTING: AtomicBool = AtomicBool::new(false);

/// O que escrever na saída: `clear` geral quando o autor fechou o painel (`q`, `Esc`, `Ctrl-C` ou
/// SIGINT); um `error` na raiz, que sobrevive ao processo, quando ele caiu ou foi derrubado.
fn exit_sequence(r: &Result<Exit>) -> String {
    match r {
        Ok(Exit::Key(_)) | Ok(Exit::Signal(libc::SIGINT)) => pst::clear(None),
        Ok(exit) => pst::exit_error(&format!("panel stopped: {exit}; run `rrq tui` again")),
        Err(e) => pst::exit_error(&format!("panel stopped with an error: {e:#}; run `rrq tui` again")),
    }
}

/// PRs cujo worktree sumiu do disco.
fn missing_worktrees(state: &State) -> BTreeSet<PrKey> {
    state.prs.values().filter(|r| !r.path.exists()).map(|r| r.key.clone()).collect()
}

/// Batida do loop principal (ms desde `EPOCH`), para o watchdog.
static BEAT_MS: AtomicU64 = AtomicU64::new(0);
/// Sem batida há tanto tempo, o loop está preso (o `event::poll` volta em 250 ms).
const STALL: Duration = Duration::from_secs(5);
/// Depois de um sinal, quanto o loop tem para sair sozinho.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// Acordar do watchdog com atraso maior que isso = o processo ficou suspenso (SIGSTOP) e voltou.
const OVERSLEEP: Duration = Duration::from_secs(1);

/// Decisão do watchdog: `Some((código, motivo))` quando o processo deve sair à força.
/// O crossterm 0.28 entra em laço dentro do `event::poll` quando o tty some (read devolve EOF/EIO
/// para sempre), então nem o `STOP` de um SIGHUP é visto pelo loop. Um loop parado sozinho não
/// basta (pode ser só lentidão): só mata quando `tty_alive` confirma que o terminal sumiu.
fn watchdog_verdict(since_beat: Duration, since_stop: Option<Duration>, tty_alive: &dyn Fn() -> bool) -> Option<(i32, &'static str)> {
    if since_stop.is_some_and(|t| t >= STOP_GRACE) {
        return Some((0, "signal received and the loop did not exit within 1 s"));
    }
    if since_beat >= STALL && !tty_alive() {
        return Some((1, "loop stalled and the terminal is gone"));
    }
    None
}

/// Estado do watchdog entre um acordar e outro.
struct Watchdog {
    last_wake: Instant,
    stop_seen: Option<Instant>,
    /// Última retomada depois de suspensão: o tempo sem batida conta a partir daqui.
    resumed_at: Option<Instant>,
}

impl Watchdog {
    fn new(now: Instant) -> Self {
        Self { last_wake: now, stop_seen: None, resumed_at: None }
    }

    fn tick(&mut self, now: Instant, beat: Instant, stop: bool, tty_alive: &dyn Fn() -> bool) -> Option<(i32, &'static str)> {
        let overslept = now.saturating_duration_since(self.last_wake) > OVERSLEEP;
        self.last_wake = now;
        if overslept {
            // suspenso e retomado: dá ao loop a chance de bater antes de julgar
            self.resumed_at = Some(now);
            return None;
        }
        if stop {
            self.stop_seen.get_or_insert(now);
        }
        let since = self.resumed_at.map_or(beat, |r| r.max(beat));
        watchdog_verdict(now.saturating_duration_since(since), self.stop_seen.map(|t| now.saturating_duration_since(t)), tty_alive)
    }
}

/// O terminal em `fd` sumiu de verdade: `poll` com POLLHUP/POLLNVAL, ou `tcgetattr` falhando com
/// EIO/ENXIO/EBADF. Um fd que nunca foi tty (ENOTTY) não conta como sumido.
fn tty_gone(fd: libc::c_int) -> bool {
    // no macOS o POLLHUP de um pty sem mestre só vem quando se pede POLLIN; entrada pendente sozinha não conta
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    if n > 0 && p.revents & (libc::POLLHUP | libc::POLLNVAL) != 0 {
        return true;
    }
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        return matches!(errno, Some(libc::EIO) | Some(libc::ENXIO) | Some(libc::EBADF));
    }
    false
}

const WATCHDOG_THREAD: &str = "tui-watchdog";

/// Registra o motivo da saída forçada: primeiro no `tui.log`, depois em `err` (o stderr, que é o
/// pty do pane). Nunca entra em pânico: com o pane fechado, escrever no pty dá EIO, e `eprintln!`
/// entraria em pânico antes do `exit`, deixando o processo girando.
fn report_exit(log: &Path, err: &mut dyn Write, why: &str) {
    let msg = format!("tui stopped by the watchdog: {why}");
    let _ = crate::applog::append(log, &msg);
    let _ = writeln!(err, "rrq: {msg}");
    let _ = err.flush();
}

/// Saída forçada do watchdog. Nada aqui entra em pânico (`restore_terminal` só descarta erros).
fn force_exit(code: i32, log: &Path, why: &str) -> ! {
    restore_terminal();
    if EMITTING.load(Ordering::SeqCst) {
        emit(&mut std::io::stdout(), &[pst::exit_error(&format!("panel stopped: {why}"))]);
    }
    report_exit(log, &mut std::io::stderr(), why);
    std::process::exit(code)
}

fn spawn_watchdog(epoch: Instant, log: PathBuf) {
    let fallback_log = log.clone();
    let spawned = std::thread::Builder::new().name(WATCHDOG_THREAD.into()).spawn(move || {
        let mut dog = Watchdog::new(Instant::now());
        let tty_alive = || !tty_gone(libc::STDIN_FILENO);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let beat = epoch + Duration::from_millis(BEAT_MS.load(Ordering::SeqCst));
            if let Some((code, why)) = dog.tick(Instant::now(), beat, STOP.load(Ordering::SeqCst), &tty_alive) {
                force_exit(code, &log, why);
            }
        }
    });
    if let Err(e) = spawned {
        // sem watchdog o TUI ainda funciona; só perde a saída forçada com o tty morto
        let _ = crate::applog::append(&fallback_log, &format!("could not create the watchdog thread: {e}"));
    }
}

/// Pânico fora da thread da UI: o TUI segue de pé, então o texto vai para o `tui.log`.
fn record_panic(log: &Path, thread: Option<&str>, info: &dyn std::fmt::Display) {
    let _ = crate::applog::append(log, &format!("panic in thread {}: {info}", thread.unwrap_or("unnamed")));
}

fn install_signals() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGABRT] {
        let prev = unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
        if prev == libc::SIG_ERR {
            bail!("could not install the handler for signal {sig}: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Abre o painel. Só um painel por vez emite o OSC 7501 (`panel.lock`); um segundo funciona igual,
/// sem emitir. Na saída, o emissor deixa no bloco um `clear` (saída pedida) ou um `error`.
pub fn run(paths: Paths, cfg: Config) -> Result<Exit> {
    let emitter_lock = match crate::locks::try_panel_emitter(&paths) {
        Ok(lock) => lock,
        Err(e) => {
            let _ = crate::applog::append(&paths.tui_log_file(), &format!("could not take panel.lock: {e:#}"));
            None
        }
    };
    EMITTING.store(emitter_lock.is_some(), Ordering::SeqCst);
    let r = run_loop(&paths, cfg);
    if EMITTING.load(Ordering::SeqCst) {
        emit(&mut std::io::stdout(), &[exit_sequence(&r)]);
    }
    drop(emitter_lock);
    r
}

fn run_loop(paths: &Paths, cfg: Config) -> Result<Exit> {
    install_signals()?;
    let default_hook = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    let tui_log = paths.tui_log_file();
    std::panic::set_hook(Box::new(move |info| {
        // pânico do worker é pego por `guarded` e vira mensagem no TUI, que segue de pé:
        // nem restaura o terminal nem escreve no stderr por cima da tela; o texto vai para o tui.log
        let current = std::thread::current();
        if current.id() == ui_thread {
            restore_terminal();
            if EMITTING.load(Ordering::SeqCst) {
                // o registro `error` sobrevive ao processo: o MacBook vê que o painel caiu
                emit(&mut std::io::stdout(), &[pst::exit_error(&format!("panel crashed: {info}"))]);
            }
            // o pane fecha com o processo e leva o stderr: o pânico também fica no tui.log
            record_panic(&tui_log, Some("UI"), info);
            default_hook(info);
        } else {
            record_panic(&tui_log, current.name(), info);
            if current.name() == Some(WATCHDOG_THREAD) {
                // o watchdog é quem tira o processo de um tty morto: se ele cair, sai aqui mesmo
                restore_terminal();
                std::process::exit(1);
            }
        }
    }));
    let _guard = TerminalGuard::enter()?;
    // o que foi digitado antes (ex.: durante o roubo de foco ao abrir o pane) não vira ação
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let view_cfg = ViewConfig {
        repos: cfg.repos.iter().map(|r| r.name.clone()).collect(),
        poll_interval_secs: cfg.poll_interval_secs,
        remove_grace: cfg.remove_grace(),
    };
    let (work, results) = spawn_worker(cfg, paths.clone());
    let mut ui = UiState::default();
    let mut watch = StateWatch::default();
    let mut sessions_watch = SessionsWatch::default();
    let mut emitter = EMITTING.load(Ordering::SeqCst).then(|| {
        let (e, first) = Emitter::start();
        emit(&mut std::io::stdout(), &[first]);
        e
    });
    let mut facts = UiFacts::default();
    let mut last_check = Instant::now() - Duration::from_secs(1);
    let epoch = Instant::now();
    spawn_watchdog(epoch, paths.tui_log_file());
    loop {
        BEAT_MS.store(epoch.elapsed().as_millis() as u64, Ordering::SeqCst);
        if STOP.load(Ordering::SeqCst) {
            return Ok(Exit::Signal(SIGNAL.load(Ordering::SeqCst)));
        }
        if last_check.elapsed() >= Duration::from_millis(500) {
            last_check = Instant::now();
            watch.check(&paths.state_file());
            sessions_watch.check(&paths.sessions_file(), false);
            // a cada releitura (e também quando só o disco mudou: um worktree apagado não muda o estado)
            facts.missing_worktrees = watch.state.as_ref().map(missing_worktrees).unwrap_or_default();
        }
        loop {
            match results.try_recv() {
                Ok(done) => {
                    if !done.msg.is_empty() {
                        ui.message = Some(done.msg);
                    }
                    if done.entered.is_some() {
                        sessions_watch.check(&paths.sessions_file(), true);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => bail!("the TUI action worker stopped"),
            }
        }
        let mut view = View::build(watch.state.as_ref(), &ui, &view_cfg, &facts, chrono::Utc::now());
        if let Some(e) = &watch.read_error {
            view.header.errors.push(e.clone());
        }
        if let Some(e) = &sessions_watch.error {
            view.header.errors.push(e.clone());
        }
        if emitter.is_none() {
            view.notes.push("another `rrq tui` sends the Rex alerts; this panel only shows the queue".into());
        }
        let old_order = std::mem::replace(&mut ui.order, view.order());
        ui.selected = reselect(&old_order, ui.selected.as_ref(), &view);
        terminal.draw(|f| crate::tui::render::draw(f, &view, &ui))?;
        // o 7501 sai junto com o desenho, só com o que mudou
        if let (Some(em), Some(state)) = (emitter.as_mut(), watch.state.as_ref()) {
            let seqs = em.step(state, &sessions_watch.sessions);
            if !seqs.is_empty() {
                emit(&mut std::io::stdout(), &seqs);
            }
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let ev = event::read()?;
        let Some(key) = key_from_event(&ev) else { continue };
        let (next, action) = handle(key, &ui, &view);
        ui = next;
        match action {
            None => {}
            Some(Action::Quit) => return Ok(Exit::Key(key_name(&key))),
            Some(Action::Request(kind, pr)) => match requests::write(&paths.requests_dir(), kind, pr.as_ref().map(|k| k.to_string())) {
                Ok(id) => ui.sent.push(Sent { id, kind, pr, at: chrono::Utc::now() }),
                Err(e) => ui.message = Some(format!("could not send the request: {e:#}")),
            },
            Some(other) => {
                if let Action::OpenUrl(_, url) = &other {
                    osc52(url);
                }
                // `false`: idêntica ainda pendente ou rodando, descartada
                work.submit(other)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fake::{FakeBlock, FakeMux};
    use crate::mux::ClientInfo;
    use crate::runner::FakeRunner;
    use crate::state::{Phase, PrKey, PrRecord};
    use crate::tui::model::RowId;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent { code, modifiers: mods, kind, state: KeyEventState::NONE })
    }

    #[test]
    fn presses_become_keys_and_repeat_only_scrolls() {
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Press)), Some(Key::Char('r')));
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        // segurar ↑/↓/j/k rola a lista; nenhuma outra tecla repete (ações nunca)
        assert_eq!(key_from_event(&ev(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Down));
        assert_eq!(key_from_event(&ev(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Up));
        assert_eq!(key_from_event(&ev(KeyCode::Char('j'), KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Char('j')));
        assert_eq!(key_from_event(&ev(KeyCode::Char('k'), KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Char('k')));
        assert_eq!(key_from_event(&ev(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Release)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Press)), Some(Key::CtrlC));
        assert_eq!(key_from_event(&Event::Paste("y".into())), None);
        assert_eq!(key_from_event(&ev(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press)), Some(Key::Up));
        assert_eq!(key_from_event(&ev(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Press)), Some(Key::Backspace));
        assert_eq!(key_from_event(&ev(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Backspace));
        assert_eq!(key_from_event(&ev(KeyCode::Char('u'), KeyModifiers::CONTROL, KeyEventKind::Press)), Some(Key::CtrlU));
        assert_eq!(key_from_event(&ev(KeyCode::Char('w'), KeyModifiers::CONTROL, KeyEventKind::Press)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('b'), KeyModifiers::ALT, KeyEventKind::Press)), None);
        // shift continua sendo letra (maiúscula)
        assert_eq!(key_from_event(&ev(KeyCode::Char('R'), KeyModifiers::SHIFT, KeyEventKind::Press)), Some(Key::Char('R')));
    }

    #[test]
    fn exit_outcome_goes_to_tui_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/tui.log");
        log_outcome(&log, &Ok(Exit::Key("q".into())));
        log_outcome(&log, &Ok(Exit::Signal(libc::SIGTERM)));
        log_outcome(&log, &Err(anyhow::anyhow!("pty sumiu").context("event::read")));
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("tui stopped: key q"), "{text}");
        assert!(lines[1].ends_with("tui stopped: signal SIGTERM"), "{text}");
        assert!(lines[2].ends_with("tui stopped with an error: event::read: pty sumiu"), "{text}");
    }

    #[test]
    fn exit_reason_names_signals() {
        assert_eq!(Exit::Signal(libc::SIGHUP).to_string(), "signal SIGHUP");
        assert_eq!(Exit::Signal(libc::SIGINT).to_string(), "signal SIGINT");
        assert_eq!(Exit::Signal(99).to_string(), "signal 99");
    }

    #[test]
    fn log_command_quotes_path() {
        assert_eq!(log_command(Path::new("/a b/it's.log")), "less +F '/a b/it'\\''s.log'");
    }

    #[test]
    fn base64_matches_standard_encoding() {
        assert_eq!(crate::pst::base64(b"url"), "dXJs");
    }

    struct Fx {
        _dir: tempfile::TempDir,
        paths: Paths,
        cfg: Config,
        wt: std::path::PathBuf,
        mux: FakeMux,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let cfg = Config::parse("worktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/repo\"\nsetup = []\n", dir.path()).unwrap();
        let mux = FakeMux::new();
        mux.apps.borrow_mut().push(ClientInfo { id: "client:mac".into(), label: Some("macbook".into()) });
        Fx { _dir: dir, paths, cfg, wt, mux }
    }

    fn ctx<'a>(fx: &'a Fx, runner: &'a dyn Runner) -> WorkerCtx<'a> {
        WorkerCtx { mux: &fx.mux, runner, cfg: &fx.cfg, paths: &fx.paths, client: RefCell::new(None), block: None }
    }

    fn save(fx: &Fx, phase: Phase) {
        let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
        rec.path = fx.wt.clone();
        rec.phase = phase;
        let mut s = State::default();
        s.insert(rec);
        s.save(&fx.paths.state_file()).unwrap();
    }

    #[test]
    fn focus_rereads_state_and_reports_the_entered_pr() {
        let fx = fx();
        let runner = FakeRunner::new();
        let ctx = ctx(&fx, &runner);
        let key = PrKey::new("o/r", 7);

        save(&fx, Phase::Removing);
        let done = perform(&Action::Focus(key.clone()), &ctx);
        assert!(done.msg.contains("removed") && done.entered.is_none(), "{done:?}");
        assert!(fx.mux.selected.borrow().is_empty());

        save(&fx, Phase::Ready);
        let done = perform(&Action::Focus(key.clone()), &ctx);
        assert_eq!(done, Done { msg: "opened a new session for o/r#7".into(), entered: Some(key.clone()) });
        assert_eq!(perform(&Action::Focus(key.clone()), &ctx).msg, "opening o/r#7");
        assert_eq!(fx.mux.selected.borrow().len(), 2);

        std::fs::remove_file(fx.paths.state_file()).unwrap();
        std::fs::write(fx.paths.state_file(), "{lixo").unwrap();
        let done = perform(&Action::Focus(key), &ctx);
        assert!(done.msg.contains("state"), "{done:?}");
    }

    #[test]
    fn combo_and_first_ready_go_through_focus() {
        let fx = fx();
        let runner = FakeRunner::new();
        let ctx = ctx(&fx, &runner);
        let key = PrKey::new("o/r", 7);
        assert_eq!(perform(&Action::FirstReady, &ctx).msg, "no review is ready");
        save(&fx, Phase::Ready);
        let done = perform(&Action::Combo(key.clone()), &ctx);
        assert_eq!(done.msg, "o/r#7: claude opened, reviewr opened");
        assert_eq!(done.entered, Some(key.clone()));
        let done = perform(&Action::FirstReady, &ctx);
        assert_eq!(done, Done { msg: "opening o/r#7".into(), entered: Some(key) });
    }

    #[test]
    fn no_app_is_reported_in_the_footer() {
        let fx = fx();
        fx.mux.apps.borrow_mut().clear();
        save(&fx, Phase::Ready);
        let runner = FakeRunner::new();
        let done = perform(&Action::Focus(PrKey::new("o/r", 7)), &ctx(&fx, &runner));
        assert!(done.msg.contains("macbook not connected") && done.entered.is_none(), "{done:?}");
    }

    #[test]
    fn open_url_only_reports_the_clipboard_and_runs_nothing() {
        let fx = fx();
        save(&fx, Phase::Ready);
        let runner = FakeRunner::new();
        // a URL vem do estado relido, não da que a tecla carregou
        let action = Action::OpenUrl(RowId::Review(PrKey::new("o/r", 7)), "https://velha".into());
        let done = perform(&action, &ctx(&fx, &runner));
        assert_eq!(done.msg, "URL sent to the terminal clipboard: https://github.com/o/r/pull/7");
        assert!(runner.calls.borrow().is_empty(), "nada de `open` no navegador desta máquina");
        assert!(fx.mux.calls.borrow().is_empty());
    }

    #[test]
    fn open_url_and_show_log_reread_state_and_refuse_vanished_pr() {
        let fx = fx();
        save(&fx, Phase::Ready);
        let runner = FakeRunner::new();
        let ctx = ctx(&fx, &runner);
        let gone = PrKey::new("o/r", 8);
        let log = fx.paths.setup_log(&gone);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ok").unwrap();
        let m = perform(&Action::OpenUrl(RowId::Review(gone.clone()), "https://github.com/o/r/pull/8".into()), &ctx).msg;
        assert!(m.contains("is no longer in the panel"), "{m}");
        let m = perform(&Action::ShowLog(gone), &ctx).msg;
        assert!(m.contains("is no longer in the panel"), "{m}");

        std::fs::write(fx.paths.state_file(), "{lixo").unwrap();
        let m = perform(&Action::OpenUrl(RowId::Review(PrKey::new("o/r", 7)), "x".into()), &ctx).msg;
        assert!(m.contains("state"), "{m}");
    }

    #[test]
    fn open_url_mine_rereads_my_prs_and_refuses_vanished() {
        let fx = fx();
        let mut s = State::default();
        s.my_prs.repos.insert(
            "o/r".into(),
            crate::state::MyPrsRepo {
                prs: vec![crate::state::MyPr {
                    number: 9,
                    title: "Meu".into(),
                    url: "https://github.com/o/r/pull/9".into(),
                    draft: false,
                    created_at: chrono::Utc::now(),
                    head_sha: "m".into(),
                    ci: None,
                }],
                opened_24h: 1,
                as_of: chrono::Utc::now(),
            },
        );
        s.save(&fx.paths.state_file()).unwrap();
        let runner = FakeRunner::new();
        let ctx = ctx(&fx, &runner);
        let m = perform(&Action::OpenUrl(RowId::Mine(PrKey::new("o/r", 9)), "https://velha".into()), &ctx).msg;
        assert!(m.contains("https://github.com/o/r/pull/9"), "{m}");
        let m = perform(&Action::OpenUrl(RowId::Mine(PrKey::new("o/r", 8)), "x".into()), &ctx).msg;
        assert!(m.contains("is no longer in the panel"), "{m}");
        // o mesmo número como review não existe: Mine não cai em State.prs
        let m = perform(&Action::OpenUrl(RowId::Review(PrKey::new("o/r", 9)), "x".into()), &ctx).msg;
        assert!(m.contains("is no longer in the panel"), "{m}");
    }

    #[test]
    fn show_log_splits_next_to_the_panel_block_and_falls_back_to_path() {
        let fx = fx();
        let key = PrKey::new("o/r", 7);
        let runner = FakeRunner::new();
        save(&fx, Phase::Ready);
        assert_eq!(perform(&Action::ShowLog(key.clone()), &ctx(&fx, &runner)).msg, "no log yet");

        let log = fx.paths.setup_logs_dir().join("o-r-pr-7.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ok").unwrap();
        // fora do Rex (sem REX_BLOCK): só o caminho
        assert_eq!(perform(&Action::ShowLog(key.clone()), &ctx(&fx, &runner)).msg, format!("log at {}", log.display()));

        fx.mux.add_session("session:reviews", "reviews", vec![FakeBlock { id: "block:tui".into(), label: None, process: None, written: vec![] }]);
        let with_block = WorkerCtx { block: Some("block:tui".into()), ..ctx(&fx, &runner) };
        assert_eq!(perform(&Action::ShowLog(key.clone()), &with_block).msg, "log opened on the right");
        let s = fx.mux.sessions.borrow();
        let blocks = &s["session:reviews"].blocks;
        assert_eq!(blocks[1].label.as_deref(), Some("log"));
        assert_eq!(blocks[1].written, [format!("{}\r", log_command(&log))]);
        drop(s);

        let gone = WorkerCtx { block: Some("block:sumiu".into()), ..ctx(&fx, &runner) };
        let m = perform(&Action::ShowLog(key), &gone).msg;
        assert!(m.contains("could not open the log split") && m.contains(&log.display().to_string()), "{m}");
    }

    #[test]
    fn emitter_starts_clean_and_then_writes_only_changes() {
        let fx = fx();
        save(&fx, Phase::Ready);
        let state = State::read(&fx.paths.state_file()).unwrap();
        let (mut em, first) = Emitter::start();
        assert_eq!(first, pst::clear(None));
        let seqs = em.step(&state, &Sessions::default());
        assert_eq!(seqs.len(), 2, "raiz + o PR pronto: {seqs:?}");
        assert!(seqs[0].contains("state=idle") && !seqs[0].contains("id="));
        assert!(seqs[1].contains("state=done") && seqs[1].contains("id=pr-7-"));
        assert!(em.step(&state, &Sessions::default()).is_empty(), "nada mudou: nada escrito");

        // o autor entrou: o registro do PR é apagado, a raiz não muda
        let mut sessions = Sessions::default();
        let e = crate::sessions::SessionEntry { session_id: "s".into(), shell_block: "b".into(), created_at: chrono::Utc::now(), entered_at: Some(chrono::Utc::now()) };
        sessions.prs.insert("o/r#7".into(), e);
        let seqs = em.step(&state, &sessions);
        assert_eq!(seqs, vec![pst::clear(Some(&pst::pr_id(&PrKey::new("o/r", 7))))]);

        // gh sem auth: a raiz vira error
        let mut broken = state.clone();
        broken.auth_error_notified = true;
        let seqs = em.step(&broken, &sessions);
        assert_eq!(seqs.len(), 1);
        assert!(seqs[0].contains("state=error"));
    }

    #[test]
    fn exit_clears_on_request_and_leaves_an_error_otherwise() {
        assert_eq!(exit_sequence(&Ok(Exit::Key("q".into()))), pst::clear(None));
        assert_eq!(exit_sequence(&Ok(Exit::Signal(libc::SIGINT))), pst::clear(None));
        for r in [Ok(Exit::Signal(libc::SIGTERM)), Ok(Exit::Signal(libc::SIGABRT)), Err(anyhow::anyhow!("pty sumiu"))] {
            let seq = exit_sequence(&r);
            assert!(seq.contains("state=error") && !seq.contains("id="), "{seq}");
        }
    }

    #[test]
    fn sessions_watch_reloads_on_mtime_or_when_forced() {
        let fx = fx();
        let file = fx.paths.sessions_file();
        let mut w = SessionsWatch::default();
        w.check(&file, false);
        assert!(w.sessions.prs.is_empty() && w.error.is_none(), "arquivo ausente = vazio");
        let e = crate::sessions::SessionEntry { session_id: "s".into(), shell_block: "b".into(), created_at: chrono::Utc::now(), entered_at: None };
        crate::sessions::upsert(&fx.paths, &PrKey::new("o/r", 7), e).unwrap();
        let m = std::fs::metadata(&file).unwrap().modified().unwrap();
        w.mtime = Some(m);
        w.check(&file, false);
        assert!(w.sessions.prs.is_empty(), "mesmo mtime: não relê");
        w.check(&file, true);
        assert_eq!(w.sessions.prs.len(), 1, "depois de um Enter relê na hora");
        std::fs::write(&file, "{ lixo").unwrap();
        w.check(&file, true);
        assert!(w.sessions.prs.is_empty() && w.error.as_deref().unwrap().contains("corrupt"));
    }

    #[test]
    fn guarded_turns_panic_into_message() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let done = guarded(|| panic!("boom"));
        std::panic::set_hook(prev);
        assert!(done.msg.contains("failed") && done.msg.contains("boom"), "{done:?}");
        assert_eq!(guarded(|| Done::msg("ok")).msg, "ok");
    }

    #[test]
    fn watchdog_kills_a_stall_only_when_the_tty_is_gone() {
        let ms = Duration::from_millis;
        let alive = || true;
        let dead = || false;
        let code = |v: Option<(i32, &str)>| v.map(|v| v.0);
        assert_eq!(code(watchdog_verdict(ms(300), None, &dead)), None);
        assert_eq!(code(watchdog_verdict(STALL * 10, None, &alive)), None, "stall com tty vivo: não mata");
        assert_eq!(code(watchdog_verdict(STALL, None, &dead)), Some(1), "stall com tty morto: mata");
        assert_eq!(code(watchdog_verdict(ms(100), Some(ms(500)), &alive)), None, "sinal: o loop ainda tem tempo de sair sozinho");
        assert_eq!(code(watchdog_verdict(ms(100), Some(STOP_GRACE), &alive)), Some(0), "sinal + 1 s: mata");
        // o tty só é consultado quando o loop está parado
        let probes = std::cell::Cell::new(0);
        let counting = || {
            probes.set(probes.get() + 1);
            true
        };
        watchdog_verdict(ms(300), None, &counting);
        assert_eq!(probes.get(), 0);
        watchdog_verdict(STALL, None, &counting);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn watchdog_counts_from_resume_after_oversleep() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let dead = || false;
        let mut w = Watchdog::new(t0);
        // processo parado (SIGSTOP) por 30 s e retomado: o tick da retomada não julga
        let resumed = t0 + Duration::from_secs(30);
        assert_eq!(w.tick(resumed, t0, false, &dead), None);
        // tick normal logo depois, o loop ainda não bateu: conta a partir da retomada, não da batida velha
        assert_eq!(w.tick(resumed + ms(200), t0, false, &dead), None);
        // o loop bate e segue normal
        assert_eq!(w.tick(resumed + ms(400), resumed + ms(300), false, &dead), None);
        // sem batida por STALL depois da retomada e com o tty morto: mata
        let mut now = resumed + ms(400);
        let mut verdict = None;
        while verdict.is_none() && now < resumed + STALL * 2 {
            now += ms(200);
            verdict = w.tick(now, resumed + ms(300), false, &dead);
        }
        assert_eq!(verdict.map(|v| v.0), Some(1));
        assert!(now >= resumed + ms(300) + STALL, "{:?}", now - resumed);
    }

    #[test]
    fn watchdog_kills_one_second_after_an_ignored_signal() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let alive = || true;
        let mut w = Watchdog::new(t0);
        let mut now = t0;
        let mut verdict = None;
        while verdict.is_none() && now < t0 + ms(3000) {
            now += ms(200);
            verdict = w.tick(now, now, true, &alive);
        }
        assert_eq!(verdict.map(|v| v.0), Some(0));
        assert!(now - t0 >= STOP_GRACE, "{:?}", now - t0);
    }

    /// Pty com CLOEXEC nos dois lados. O openpty não marca: um processo disparado em paralelo por
    /// outro teste herdaria o mestre e o manteria aberto depois do `exec`, e o pty nunca "desligaria".
    fn cloexec_pty() -> (libc::c_int, libc::c_int) {
        let (mut master, mut slave) = (0, 0);
        let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        unsafe {
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        (master, slave)
    }

    /// Escravo de um pty cujo mestre fechou. Ainda há uma janela entre o openpty e o CLOEXEC em que
    /// um fork herda o mestre; nesse caso tenta de novo.
    fn hung_up_slave() -> libc::c_int {
        for _ in 0..20 {
            let (master, slave) = cloexec_pty();
            unsafe { libc::close(master) };
            if tty_gone(slave) {
                return slave;
            }
            unsafe { libc::close(slave) };
        }
        panic!("mestre fechado: o terminal sumiu");
    }

    #[test]
    fn tty_probe_sees_hangup_and_bad_fd() {
        let (master, slave) = cloexec_pty();
        assert!(!tty_gone(slave), "pty aberto: vivo");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        let slave = hung_up_slave();
        assert!(tty_gone(slave), "mestre fechado: o terminal sumiu");
        unsafe { libc::close(slave) };
        // fd que nunca existiu (um número baixo poderia ser reaberto por outro teste em paralelo)
        assert!(tty_gone(9_999), "EBADF/POLLNVAL");
    }

    #[test]
    fn state_watch_retries_after_read_error_even_with_same_mtime() {
        let fx = fx();
        let file = fx.paths.state_file();
        let mut w = StateWatch::default();
        save(&fx, Phase::Ready);
        w.check(&file);
        assert!(w.state.is_some() && w.read_error.is_none());

        std::fs::write(&file, "{lixo").unwrap();
        let bad_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        w.check(&file);
        assert!(w.read_error.is_some(), "estado ilegível vira erro");
        assert!(w.state.is_some(), "mantém o último estado bom");

        // conserta com o mesmo mtime (gravação no mesmo instante): relê mesmo assim
        save(&fx, Phase::Removing);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(bad_mtime).unwrap();
        w.check(&file);
        assert!(w.read_error.is_none(), "{:?}", w.read_error);
        assert_eq!(w.state.as_ref().unwrap().get(&PrKey::new("o/r", 7)).unwrap().phase, Phase::Removing);
    }

    #[test]
    fn duplicate_actions_are_dropped_while_pending_or_running() {
        let (q, rx) = ActionQueue::new();
        let focus = Action::Focus(PrKey::new("o/r", 7));
        assert!(q.submit(focus.clone()).unwrap());
        assert!(!q.submit(focus.clone()).unwrap(), "idêntica pendente: descarta");
        assert!(q.submit(Action::ShowLog(PrKey::new("o/r", 7))).unwrap(), "outra ação passa");
        assert!(q.submit(Action::Focus(PrKey::new("o/r", 8))).unwrap(), "mesma ação, outro PR, passa");
        let got = rx.recv().unwrap();
        assert_eq!(got, focus);
        assert!(!q.submit(focus.clone()).unwrap(), "rodando: ainda descarta");
        q.pending.done(&got);
        assert!(q.submit(focus.clone()).unwrap(), "terminou: aceita de novo");
        drop(rx);
        assert!(q.submit(Action::Focus(PrKey::new("o/r", 9))).is_err(), "worker parado");
    }

    /// Stderr de um pty cujo mestre fechou: toda escrita falha com EIO.
    struct EioWriter;

    impl std::io::Write for EioWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
    }

    #[test]
    fn exit_report_survives_eio_on_stderr_and_still_logs() {
        let fx = fx();
        let log = fx.paths.tui_log_file();
        report_exit(&log, &mut EioWriter, "loop stalled and the terminal is gone");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("tui stopped by the watchdog: loop stalled and the terminal is gone"), "{text}");
    }

    #[test]
    fn exit_report_survives_a_hung_up_pty() {
        use std::os::fd::FromRawFd;
        let fx = fx();
        let log = fx.paths.tui_log_file();
        let mut tty = unsafe { std::fs::File::from_raw_fd(hung_up_slave()) };
        assert!(std::io::Write::write_all(&mut tty, b"x\n").is_err(), "pty sem mestre: a escrita falha");
        report_exit(&log, &mut tty, "signal received and the loop did not exit within 1 s");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("signal received"), "{text}");
    }

    #[test]
    fn worker_panic_is_logged_to_tui_log() {
        let fx = fx();
        record_panic(&fx.paths.tui_log_file(), Some("worker"), &"panicked at src/x.rs:1:1:\nboom");
        let text = std::fs::read_to_string(fx.paths.tui_log_file()).unwrap();
        assert!(text.contains("worker") && text.contains("boom"), "{text}");
    }

    #[test]
    fn missing_worktrees_lists_prs_without_path() {
        let fx = fx();
        let mut s = State::default();
        let mut here = PrRecord::fixture("o/r", 1, "x", "s");
        here.path = fx.wt.clone();
        let mut gone = PrRecord::fixture("o/r", 2, "x", "s");
        gone.path = fx.wt.join("sumiu");
        s.insert(here);
        s.insert(gone);
        assert_eq!(missing_worktrees(&s), [PrKey::new("o/r", 2)].into());
    }
}
