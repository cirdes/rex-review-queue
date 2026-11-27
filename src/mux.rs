//! O que o rrq precisa do multiplexador (o Rex), atrás de um trait para os testes usarem `FakeMux`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum MuxError {
    /// O servidor Rex não respondeu (exit 4 do CLI, ou o processo nem rodou).
    Unreachable(String),
    ClientNotFound,
    SessionNotFound,
    BlockNotFound,
    Other(String),
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MuxError::Unreachable(m) => write!(f, "Rex server unreachable: {m}"),
            MuxError::ClientNotFound => write!(f, "Rex app not found"),
            MuxError::SessionNotFound => write!(f, "Rex session not found"),
            MuxError::BlockNotFound => write!(f, "Rex block not found"),
            MuxError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for MuxError {}

pub type MuxResult<T> = std::result::Result<T, MuxError>;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockInfo {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientInfo {
    pub id: String,
    pub label: Option<String>,
}

/// Processo em foreground de um bloco. O Rex não expõe o argv completo, só `argv0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    pub path: Option<PathBuf>,
    pub argv0: Option<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateReq {
    pub label: String,
    pub cwd: PathBuf,
    pub block_label: String,
    /// Texto digitado no shell do bloco assim que ele sobe (termine com `\r` para rodar).
    pub initial_input: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub session_id: String,
    pub block_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitReq {
    pub session: String,
    pub anchor: String,
    /// Fração da largura que fica com o bloco âncora (0..1).
    pub ratio: f64,
    pub label: String,
    pub cwd: PathBuf,
    pub initial_input: Option<String>,
    pub focus: bool,
}

pub trait Mux {
    fn server_api_revision(&self) -> MuxResult<u32>;
    fn list_sessions(&self) -> MuxResult<Vec<SessionInfo>>;
    fn create_session(&self, req: &CreateReq) -> MuxResult<Created>;
    fn view(&self, session: &str) -> MuxResult<Vec<BlockInfo>>;
    fn destroy_session(&self, session: &str) -> MuxResult<()>;
    /// `None` quando o bloco não tem foreground (o programa dele terminou).
    fn inspect_block(&self, session: &str, block: &str) -> MuxResult<Option<Process>>;
    fn new_split(&self, req: &SplitReq) -> MuxResult<String>;
    fn write(&self, session: &str, block: &str, text: &str) -> MuxResult<()>;
    fn focus_block(&self, session: &str, block: &str) -> MuxResult<()>;
    fn apps(&self) -> MuxResult<Vec<ClientInfo>>;
    fn select_session(&self, client: &str, session: &str) -> MuxResult<()>;
}

/// Todos os blocos da sessão com o processo de cada um. Um bloco que some entre o `view` e a
/// inspeção é ignorado; qualquer outro erro sobe.
pub fn inspect_session(
    mux: &dyn Mux,
    session: &str,
) -> MuxResult<Vec<(BlockInfo, Option<Process>)>> {
    let mut out = vec![];
    for b in mux.view(session)? {
        match mux.inspect_block(session, &b.id) {
            Ok(p) => out.push((b, p)),
            Err(MuxError::BlockNotFound) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh"];

/// O foreground é um shell parado no prompt (`-zsh` quando é shell de login).
pub fn is_shell(p: &Process) -> bool {
    let argv0 = p.argv0.as_deref().map(|a| a.trim_start_matches('-'));
    SHELLS.contains(&p.name.as_str()) || argv0.is_some_and(|a| SHELLS.contains(&a))
}

/// O cwd do processo está dentro da worktree, comparando caminhos canonicalizados.
pub fn in_worktree(p: &Process, worktree: &Path) -> bool {
    let canon = |q: &Path| std::fs::canonicalize(q).unwrap_or_else(|_| q.to_path_buf());
    p.cwd
        .as_deref()
        .is_some_and(|c| canon(c).starts_with(canon(worktree)))
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, Default)]
    pub struct FakeBlock {
        pub id: String,
        pub label: Option<String>,
        pub process: Option<Process>,
        pub written: Vec<String>,
    }

    #[derive(Debug, Clone, Default)]
    pub struct FakeSession {
        pub label: String,
        pub blocks: Vec<FakeBlock>,
    }

    /// Sessões, blocos e apps em memória. Grava as chamadas e aceita um erro injetado por método.
    #[derive(Default)]
    pub struct FakeMux {
        pub revision: RefCell<u32>,
        pub sessions: RefCell<BTreeMap<String, FakeSession>>,
        pub apps: RefCell<Vec<ClientInfo>>,
        pub selected: RefCell<Vec<(String, String)>>,
        pub focused: RefCell<Vec<String>>,
        pub calls: RefCell<Vec<String>>,
        pub fail: RefCell<BTreeMap<&'static str, MuxError>>,
        next: RefCell<u32>,
    }

    impl FakeMux {
        pub fn new() -> Self {
            let f = Self::default();
            *f.revision.borrow_mut() = 24;
            f
        }
        pub fn fail_on(&self, method: &'static str, err: MuxError) {
            self.fail.borrow_mut().insert(method, err);
        }
        pub fn add_session(&self, id: &str, label: &str, blocks: Vec<FakeBlock>) {
            self.sessions.borrow_mut().insert(
                id.into(),
                FakeSession {
                    label: label.into(),
                    blocks,
                },
            );
        }
        fn enter(&self, method: &'static str) -> MuxResult<()> {
            self.calls.borrow_mut().push(method.into());
            match self.fail.borrow().get(method) {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn id(&self, prefix: &str) -> String {
            let mut n = self.next.borrow_mut();
            *n += 1;
            format!("{prefix}:{n}")
        }
    }

    pub fn shell_at(cwd: &Path) -> Process {
        Process {
            pid: 1,
            name: "zsh".into(),
            path: Some("/bin/zsh".into()),
            argv0: Some("-zsh".into()),
            cwd: Some(cwd.into()),
        }
    }

    pub fn program_at(name: &str, cwd: &Path) -> Process {
        Process {
            pid: 2,
            name: name.into(),
            path: Some(format!("/usr/bin/{name}").into()),
            argv0: Some(name.into()),
            cwd: Some(cwd.into()),
        }
    }

    impl Mux for FakeMux {
        fn server_api_revision(&self) -> MuxResult<u32> {
            self.enter("server_api_revision")?;
            Ok(*self.revision.borrow())
        }
        fn list_sessions(&self) -> MuxResult<Vec<SessionInfo>> {
            self.enter("list_sessions")?;
            Ok(self
                .sessions
                .borrow()
                .iter()
                .map(|(id, s)| SessionInfo {
                    id: id.clone(),
                    label: s.label.clone(),
                })
                .collect())
        }
        fn create_session(&self, req: &CreateReq) -> MuxResult<Created> {
            self.enter("create_session")?;
            let (sid, bid) = (self.id("session"), self.id("block"));
            let block = FakeBlock {
                id: bid.clone(),
                label: Some(req.block_label.clone()),
                process: Some(shell_at(&req.cwd)),
                written: req.initial_input.iter().cloned().collect(),
            };
            self.add_session(&sid, &req.label, vec![block]);
            Ok(Created {
                session_id: sid,
                block_id: bid,
            })
        }
        fn view(&self, session: &str) -> MuxResult<Vec<BlockInfo>> {
            self.enter("view")?;
            let s = self.sessions.borrow();
            let s = s.get(session).ok_or(MuxError::SessionNotFound)?;
            Ok(s.blocks
                .iter()
                .map(|b| BlockInfo {
                    id: b.id.clone(),
                    label: b.label.clone(),
                })
                .collect())
        }
        fn destroy_session(&self, session: &str) -> MuxResult<()> {
            self.enter("destroy_session")?;
            self.sessions
                .borrow_mut()
                .remove(session)
                .map(|_| ())
                .ok_or(MuxError::SessionNotFound)
        }
        fn inspect_block(&self, session: &str, block: &str) -> MuxResult<Option<Process>> {
            self.enter("inspect_block")?;
            let s = self.sessions.borrow();
            let s = s.get(session).ok_or(MuxError::SessionNotFound)?;
            s.blocks
                .iter()
                .find(|b| b.id == block)
                .map(|b| b.process.clone())
                .ok_or(MuxError::BlockNotFound)
        }
        fn new_split(&self, req: &SplitReq) -> MuxResult<String> {
            self.enter("new_split")?;
            let mut all = self.sessions.borrow_mut();
            let s = all.get_mut(&req.session).ok_or(MuxError::SessionNotFound)?;
            if !s.blocks.iter().any(|b| b.id == req.anchor) {
                return Err(MuxError::BlockNotFound);
            }
            let bid = self.id("block");
            s.blocks.push(FakeBlock {
                id: bid.clone(),
                label: Some(req.label.clone()),
                process: Some(shell_at(&req.cwd)),
                written: req.initial_input.iter().cloned().collect(),
            });
            Ok(bid)
        }
        fn write(&self, session: &str, block: &str, text: &str) -> MuxResult<()> {
            self.enter("write")?;
            let mut all = self.sessions.borrow_mut();
            let s = all.get_mut(session).ok_or(MuxError::SessionNotFound)?;
            let b = s
                .blocks
                .iter_mut()
                .find(|b| b.id == block)
                .ok_or(MuxError::BlockNotFound)?;
            b.written.push(text.into());
            Ok(())
        }
        fn focus_block(&self, session: &str, block: &str) -> MuxResult<()> {
            self.enter("focus_block")?;
            let s = self.sessions.borrow();
            let s = s.get(session).ok_or(MuxError::SessionNotFound)?;
            if !s.blocks.iter().any(|b| b.id == block) {
                return Err(MuxError::BlockNotFound);
            }
            self.focused.borrow_mut().push(block.into());
            Ok(())
        }
        fn apps(&self) -> MuxResult<Vec<ClientInfo>> {
            self.enter("apps")?;
            Ok(self.apps.borrow().clone())
        }
        fn select_session(&self, client: &str, session: &str) -> MuxResult<()> {
            self.enter("select_session")?;
            if !self.apps.borrow().iter().any(|a| a.id == client) {
                return Err(MuxError::ClientNotFound);
            }
            if !self.sessions.borrow().contains_key(session) {
                return Err(MuxError::Other(
                    "SessionNotFoundOnServerError (code=internal)".into(),
                ));
            }
            self.selected
                .borrow_mut()
                .push((client.into(), session.into()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn shell_detection_accepts_login_shells_and_rejects_programs() {
        let wt = Path::new("/w");
        assert!(is_shell(&shell_at(wt)));
        let bash = Process {
            pid: 1,
            name: "bash".into(),
            path: None,
            argv0: Some("bash".into()),
            cwd: None,
        };
        assert!(is_shell(&bash));
        assert!(!is_shell(&program_at("claude", wt)));
        // nome curto vindo do binário mas argv0 de shell de login: ainda é shell
        let login = Process {
            pid: 1,
            name: "zsh-5.9".into(),
            path: None,
            argv0: Some("-zsh".into()),
            cwd: None,
        };
        assert!(is_shell(&login));
    }

    #[test]
    fn worktree_check_canonicalizes_and_needs_a_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        assert!(in_worktree(&shell_at(&wt.join("sub")), &wt));
        assert!(!in_worktree(&shell_at(dir.path()), &wt));
        let no_cwd = Process {
            cwd: None,
            ..shell_at(&wt)
        };
        assert!(!in_worktree(&no_cwd, &wt));
    }

    #[test]
    fn inspect_session_skips_vanished_blocks_and_propagates_the_rest() {
        let m = FakeMux::new();
        let wt = Path::new("/w");
        m.add_session(
            "s1",
            "#1 r",
            vec![FakeBlock {
                id: "b1".into(),
                label: Some("shell".into()),
                process: Some(shell_at(wt)),
                ..Default::default()
            }],
        );
        let got = inspect_session(&m, "s1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            inspect_session(&m, "nada").unwrap_err(),
            MuxError::SessionNotFound
        );
        m.fail_on("inspect_block", MuxError::BlockNotFound);
        assert!(inspect_session(&m, "s1").unwrap().is_empty());
        m.fail_on("inspect_block", MuxError::Other("x".into()));
        assert_eq!(
            inspect_session(&m, "s1").unwrap_err(),
            MuxError::Other("x".into())
        );
    }
}
