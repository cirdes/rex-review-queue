//! `rrq doctor`: confere o que o rrq precisa para rodar e diz como corrigir cada falha.
//!
//! Só lê. Não cria sessão, não chama `client.perform`, não fecha nada e não grava arquivo: as
//! únicas chamadas ao Rex são `server.version`, `client.list` e `session.list`.

use crate::config::{self, Config};
use crate::daemon::MIN_REX_REVISION;
use crate::mux::{Mux, MuxError};
use crate::paths::Paths;
use crate::runner::Runner;
use crate::service::{self, OLD_AGENT_LABEL};
use crate::sessions;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Ok,
    Fail,
    /// Não deu para conferir porque uma checagem anterior falhou.
    Skip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Ok,
        detail: detail.into(),
        fix: None,
    }
}

fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Fail,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn skip(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Skip,
        detail: detail.into(),
        fix: None,
    }
}

/// Lê a config. Chaves antigas viram uma checagem própria, e o resto da config é conferido sem
/// elas, para as outras checagens ainda terem o `[rex]` e o `[combo]`.
pub fn config_checks(paths: &Paths) -> (Vec<Check>, Option<Config>) {
    let file = &paths.config_file;
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            let c = fail(
                "config",
                format!("could not read {}: {e}", file.display()),
                "copy config.example.toml to that path and edit the [[repos]]",
            );
            return (vec![c], None);
        }
    };
    let old = config::old_keys(&text);
    let old_check = if old.is_empty() {
        ok("old config keys", "none")
    } else {
        fail(
            "old config keys",
            format!("{} still in config.toml", old.join(", ")),
            "remove them; the [rex] and [combo] defaults usually do (README, \"Migração\")",
        )
    };
    let rest = if old.is_empty() {
        Some(text)
    } else {
        config::without_old_keys(&text)
    };
    let parsed = rest
        .ok_or_else(|| anyhow::anyhow!("invalid TOML"))
        .and_then(|t| Config::parse(&t, &paths.home));
    match parsed {
        Ok(cfg) => (
            vec![ok("config", file.display().to_string()), old_check],
            Some(cfg),
        ),
        Err(e) => (
            vec![
                fail(
                    "config",
                    format!("{e:#}"),
                    format!("fix {}", file.display()),
                ),
                old_check,
            ],
            None,
        ),
    }
}

/// O `rex` que o daemon e o painel usam: o da config, ou o padrão quando ela não carrega.
pub fn rex_bin(cfg: Option<&Config>, home: &Path) -> PathBuf {
    match cfg {
        Some(c) => c.rex.bin.clone(),
        None => config::expand_tilde(&config::RexConfig::default().bin, home),
    }
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Onde o programa seria achado com este PATH. Caminho com `/` é conferido como está.
fn which(program: &str, path_env: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return executable(&p).then_some(p);
    }
    path_env
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(program))
        .find(|p| executable(p))
}

/// `#123 repo` ou `owner/repo#123`: os labels que o rrq dá às sessões de PR.
pub fn is_pr_session_label(label: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some(rest) = label.strip_prefix('#') {
        return match rest.split_once(' ') {
            Some((n, repo)) => digits(n) && !repo.is_empty() && !repo.contains(char::is_whitespace),
            None => false,
        };
    }
    match label.rsplit_once('#') {
        Some((repo, n)) => {
            digits(n)
                && !label.contains(char::is_whitespace)
                && repo
                    .split_once('/')
                    .is_some_and(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
        }
        None => false,
    }
}

fn rex_checks(paths: &Paths, cfg: Option<&Config>, mux: &dyn Mux, out: &mut Vec<Check>) {
    let bin = rex_bin(cfg, &paths.home);
    if executable(&bin) {
        out.push(ok("rex CLI", bin.display().to_string()));
    } else {
        out.push(fail(
            "rex CLI",
            format!("{} is missing or not executable", bin.display()),
            "install Rex.app on this machine and link its CLI there, or set [rex] bin in config.toml",
        ));
        for name in [
            "Rex server",
            "Rex API revision",
            "Rex app",
            "orphan sessions",
        ] {
            out.push(skip(name, "skipped: no rex CLI"));
        }
        return;
    }

    const SKIP: &str = "skipped: no connection to the Rex server";
    let revision = match mux.server_api_revision() {
        Ok(r) => {
            out.push(ok("Rex server", "reachable"));
            Some(r)
        }
        Err(e) => {
            let fix = match e {
                MuxError::Unreachable(_) => "open Rex.app on this machine (login item, see README) and run `rrq doctor` again",
                _ => "check the Rex server with `rex whoami`",
            };
            out.push(fail("Rex server", e.to_string(), fix));
            None
        }
    };
    let Some(revision) = revision else {
        for name in ["Rex API revision", "Rex app", "orphan sessions"] {
            out.push(skip(name, SKIP));
        }
        return;
    };
    if revision >= MIN_REX_REVISION {
        out.push(ok(
            "Rex API revision",
            format!("{revision} (minimum {MIN_REX_REVISION})"),
        ));
    } else {
        out.push(fail(
            "Rex API revision",
            format!("{revision}, below the minimum {MIN_REX_REVISION}"),
            "update Rex.app on this machine",
        ));
    }

    let label = cfg
        .map(|c| c.rex.app_label.clone())
        .unwrap_or_else(|| config::RexConfig::default().app_label);
    match mux.apps() {
        Ok(apps)
            if apps
                .iter()
                .any(|a| a.label.as_deref() == Some(label.as_str())) =>
        {
            out.push(ok("Rex app", format!("\"{label}\" connected")));
        }
        Ok(apps) => {
            let seen: Vec<&str> = apps.iter().filter_map(|a| a.label.as_deref()).collect();
            let seen = if seen.is_empty() {
                "none".to_string()
            } else {
                seen.join(", ")
            };
            out.push(fail(
                "Rex app",
                format!("no app labeled \"{label}\" is connected (apps seen: {seen})"),
                "on the MacBook, connect Rex.app to this host (palette: Connect Host…), or set [rex] app_label",
            ));
        }
        Err(e) => out.push(fail(
            "Rex app",
            e.to_string(),
            "check the Rex server with `rex client ls`",
        )),
    }

    let known = match sessions::load(&paths.sessions_file()) {
        Ok(s) => s
            .prs
            .values()
            .map(|e| e.session_id.clone())
            .collect::<BTreeSet<_>>(),
        Err(e) => {
            out.push(fail(
                "orphan sessions",
                e.to_string(),
                "`rrq restart` sets the file aside and starts an empty map",
            ));
            return;
        }
    };
    match mux.list_sessions() {
        Ok(all) => {
            let orphans: Vec<String> = all
                .into_iter()
                .filter(|s| is_pr_session_label(&s.label) && !known.contains(&s.id))
                .map(|s| s.label)
                .collect();
            if orphans.is_empty() {
                out.push(ok("orphan sessions", "none"));
            } else {
                out.push(fail(
                    "orphan sessions",
                    format!("{} PR session(s) not in sessions.json: {}", orphans.len(), orphans.join(", ")),
                    "rrq never closes them; close them in the Rex sidebar once you no longer need them",
                ));
            }
        }
        Err(e) => out.push(fail(
            "orphan sessions",
            e.to_string(),
            "check the Rex server with `rex session ls`",
        )),
    }
}

fn combo_check(paths: &Paths, cfg: Option<&Config>) -> Check {
    let Some(cfg) = cfg else {
        return skip("combo commands", "skipped: the config did not load");
    };
    let path_env = service::path_env(&paths.home);
    let mut missing = vec![];
    let mut found = vec![];
    for (key, argv) in [
        ("combo.claude", &cfg.combo.claude),
        ("combo.reviewr", &cfg.combo.reviewr),
    ] {
        match argv.first() {
            None => missing.push(format!("{key} is empty")),
            Some(p) => match which(p, &path_env) {
                Some(at) => found.push(at.display().to_string()),
                None => missing.push(format!("{key} ({p}) not found")),
            },
        }
    }
    if missing.is_empty() {
        ok("combo commands", found.join(", "))
    } else {
        fail(
            "combo commands",
            format!("{} in the LaunchAgent PATH", missing.join("; ")),
            "install them in ~/.local/bin (or another directory of that PATH), or set [combo] in config.toml",
        )
    }
}

/// As checagens que dependem da config já lida e do Rex.
pub fn checks(
    paths: &Paths,
    runner: &dyn Runner,
    cfg: Option<&Config>,
    mux: &dyn Mux,
) -> Vec<Check> {
    let mut out = vec![];
    if service::label_loaded(runner, OLD_AGENT_LABEL) {
        out.push(fail(
            "old LaunchAgent",
            format!("{OLD_AGENT_LABEL} is still loaded"),
            "run `herdr-reviewq service uninstall`",
        ));
    } else {
        out.push(ok(
            "old LaunchAgent",
            format!("{OLD_AGENT_LABEL} not loaded"),
        ));
    }
    rex_checks(paths, cfg, mux, &mut out);
    out.push(combo_check(paths, cfg));
    out
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail)
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.status {
            Status::Ok => "ok  ",
            Status::Fail => "FAIL",
            Status::Skip => "skip",
        };
        out.push_str(&format!("{tag}  {}: {}", c.name, c.detail));
        if let Some(fix) = &c.fix {
            out.push_str(&format!(". Fix: {fix}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fake::FakeMux;
    use crate::mux::ClientInfo;
    use crate::runner::{FakeRunner, Output};
    use crate::sessions::SessionEntry;
    use crate::state::PrKey;
    use std::os::unix::fs::PermissionsExt;

    struct Env {
        _dir: tempfile::TempDir,
        paths: Paths,
    }

    fn exe(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// HOME temporário com config válida, `rex`, `claude` e `reviewr` em `~/.local/bin`.
    /// `extra` vai no fim do arquivo: tabelas (`[rex]`, `[combo]`) ou, depois de um `#raiz`, chaves
    /// de raiz, que o helper move para o começo.
    fn env(extra: &str) -> Env {
        let (tables, root) = extra.split_once("#raiz\n").unwrap_or((extra, ""));
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path().to_path_buf());
        for bin in ["rex", "claude", "reviewr"] {
            exe(&paths.home.join(".local/bin").join(bin));
        }
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config_file,
            format!("{root}worktrees_dir = \"~/w\"\n[[repos]]\nname = \"o/r\"\npath = \"~/r\"\nsetup = []\n{tables}"),
        )
        .unwrap();
        Env { _dir: dir, paths }
    }

    fn healthy_mux() -> FakeMux {
        let m = FakeMux::new();
        m.apps.borrow_mut().push(ClientInfo {
            id: "c1".into(),
            label: Some("macbook".into()),
        });
        m
    }

    fn not_loaded() -> FakeRunner {
        let r = FakeRunner::new();
        r.on(
            "launchctl print",
            Output::fail(113, "Could not find service"),
        );
        r
    }

    fn run(e: &Env, runner: &dyn Runner, mux: &dyn Mux) -> Vec<Check> {
        let (mut all, cfg) = config_checks(&e.paths);
        all.extend(checks(&e.paths, runner, cfg.as_ref(), mux));
        all
    }

    fn get<'a>(all: &'a [Check], name: &str) -> &'a Check {
        all.iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} missing: {all:?}"))
    }

    #[test]
    fn healthy_machine_passes_every_check() {
        let e = env("");
        let mux = healthy_mux();
        let all = run(&e, &not_loaded(), &mux);
        assert!(!failed(&all), "{}", render(&all));
        assert!(
            all.iter().all(|c| c.status == Status::Ok),
            "{}",
            render(&all)
        );
        let names: Vec<_> = all.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "config",
                "old config keys",
                "old LaunchAgent",
                "rex CLI",
                "Rex server",
                "Rex API revision",
                "Rex app",
                "orphan sessions",
                "combo commands"
            ]
        );
    }

    #[test]
    fn doctor_only_reads() {
        let e = env("");
        let mux = healthy_mux();
        mux.add_session("s1", "#7 r", vec![]);
        let runner = not_loaded();
        run(&e, &runner, &mux);
        let calls = mux.calls.borrow().clone();
        assert!(
            calls
                .iter()
                .all(|c| ["server_api_revision", "apps", "list_sessions"].contains(&c.as_str())),
            "{calls:?}"
        );
        assert!(
            runner
                .lines()
                .iter()
                .all(|l| l.starts_with("launchctl print ")),
            "{:?}",
            runner.lines()
        );
        assert!(!e.paths.state_dir.exists(), "doctor não cria o state_dir");
    }

    #[test]
    fn old_agent_loaded_fails_with_the_uninstall_command() {
        let e = env("");
        let runner = FakeRunner::new();
        runner.on("launchctl print", Output::ok("state = running"));
        let all = run(&e, &runner, &healthy_mux());
        let c = get(&all, "old LaunchAgent");
        assert_eq!(c.status, Status::Fail);
        assert!(c
            .fix
            .as_deref()
            .unwrap()
            .contains("herdr-reviewq service uninstall"));
        assert!(
            runner.lines()[0].ends_with(OLD_AGENT_LABEL),
            "{:?}",
            runner.lines()
        );
        assert!(failed(&all));
    }

    #[test]
    fn old_config_keys_are_named_and_the_rest_is_still_checked() {
        let e = env("#raiz\nherdr_session = \"main\"\nwindow = \"x\"\n");
        let all = run(&e, &not_loaded(), &healthy_mux());
        let c = get(&all, "old config keys");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("`herdr_session`") && c.detail.contains("`window`"),
            "{}",
            c.detail
        );
        assert_eq!(get(&all, "config").status, Status::Ok);
        assert_eq!(get(&all, "combo commands").status, Status::Ok);
    }

    #[test]
    fn missing_config_fails_and_skips_the_combo() {
        let e = env("");
        std::fs::remove_file(&e.paths.config_file).unwrap();
        let all = run(&e, &not_loaded(), &healthy_mux());
        assert_eq!(get(&all, "config").status, Status::Fail);
        assert_eq!(get(&all, "combo commands").status, Status::Skip);
        // sem config, o `rex` padrão (~/.local/bin/rex) é o conferido
        assert_eq!(get(&all, "rex CLI").status, Status::Ok);
    }

    #[test]
    fn missing_rex_binary_fails() {
        let e = env("[rex]\nbin = \"~/outro/rex\"\n");
        let all = run(&e, &not_loaded(), &healthy_mux());
        let c = get(&all, "rex CLI");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("outro/rex"), "{}", c.detail);
        assert_eq!(get(&all, "Rex server").status, Status::Skip);
    }

    #[test]
    fn unreachable_server_fails_and_skips_what_needs_it() {
        let e = env("");
        let mux = healthy_mux();
        mux.fail_on(
            "server_api_revision",
            MuxError::Unreachable("exit 4".into()),
        );
        let all = run(&e, &not_loaded(), &mux);
        let c = get(&all, "Rex server");
        assert_eq!(c.status, Status::Fail);
        assert!(c.fix.as_deref().unwrap().contains("open Rex.app"));
        for name in ["Rex API revision", "Rex app", "orphan sessions"] {
            assert_eq!(get(&all, name).status, Status::Skip, "{name}");
        }
        assert!(!mux
            .calls
            .borrow()
            .iter()
            .any(|c| c == "apps" || c == "list_sessions"));
    }

    #[test]
    fn old_revision_fails_with_found_and_minimum() {
        let e = env("");
        let mux = healthy_mux();
        *mux.revision.borrow_mut() = 23;
        let all = run(&e, &not_loaded(), &mux);
        let c = get(&all, "Rex API revision");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("23") && c.detail.contains("24"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn app_with_another_label_fails_and_lists_what_it_saw() {
        let e = env("[rex]\napp_label = \"mac-do-trabalho\"\n");
        let mux = healthy_mux();
        let all = run(&e, &not_loaded(), &mux);
        let c = get(&all, "Rex app");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("mac-do-trabalho") && c.detail.contains("macbook"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn combo_commands_are_looked_up_in_the_launch_agent_path() {
        let e = env("[combo]\nreviewr = [\"revisor\", \"{worktree}\"]\nclaude = [\"/bin/sh\"]\n");
        let all = run(&e, &not_loaded(), &healthy_mux());
        let c = get(&all, "combo commands");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("combo.reviewr (revisor) not found"),
            "{}",
            c.detail
        );
        assert!(
            !c.detail.contains("combo.claude"),
            "caminho absoluto existente passa: {}",
            c.detail
        );
        exe(&e.paths.home.join(".local/share/mise/shims/revisor"));
        let all = run(&e, &not_loaded(), &healthy_mux());
        assert_eq!(get(&all, "combo commands").status, Status::Ok);
    }

    #[test]
    fn pr_sessions_outside_the_map_are_listed() {
        let e = env("");
        let mux = healthy_mux();
        mux.add_session("s-known", "#1 r", vec![]);
        mux.add_session("s-orphan", "#2 r", vec![]);
        mux.add_session("s-owner", "o/r#3", vec![]);
        mux.add_session("s-reviews", "reviews", vec![]);
        mux.add_session("s-hand", "#notas da semana", vec![]);
        let entry = SessionEntry {
            session_id: "s-known".into(),
            shell_block: "b".into(),
            created_at: chrono::Utc::now(),
            entered_at: None,
        };
        std::fs::create_dir_all(&e.paths.state_dir).unwrap();
        let mut map = sessions::Sessions::default();
        map.prs.insert(PrKey::new("o/r", 1).to_string(), entry);
        std::fs::write(
            e.paths.sessions_file(),
            serde_json::to_string(&map).unwrap(),
        )
        .unwrap();
        let all = run(&e, &not_loaded(), &mux);
        let c = get(&all, "orphan sessions");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.starts_with("2 PR session(s)")
                && c.detail.contains("#2 r")
                && c.detail.contains("o/r#3"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn corrupt_sessions_file_fails_without_touching_it() {
        let e = env("");
        std::fs::create_dir_all(&e.paths.state_dir).unwrap();
        std::fs::write(e.paths.sessions_file(), "{").unwrap();
        let all = run(&e, &not_loaded(), &healthy_mux());
        assert_eq!(get(&all, "orphan sessions").status, Status::Fail);
        assert_eq!(
            std::fs::read_to_string(e.paths.sessions_file()).unwrap(),
            "{"
        );
    }

    #[test]
    fn pr_session_labels() {
        for yes in [
            "#1 r",
            "#123 rex-review-queue",
            "o/r#12",
            "cirdes/rex-review-queue#7",
        ] {
            assert!(is_pr_session_label(yes), "{yes}");
        }
        for no in [
            "reviews",
            "#notas da semana",
            "#12",
            "# r",
            "o#1",
            "o/r#",
            "o/r/x#1",
            "#1 a b",
        ] {
            assert!(!is_pr_session_label(no), "{no}");
        }
    }

    #[test]
    fn render_has_one_line_per_check_with_the_fix() {
        let all = vec![
            ok("config", "/h/.config/rrq/config.toml"),
            fail("Rex app", "no app", "connect it"),
            skip("orphan sessions", "skipped"),
        ];
        assert_eq!(
            render(&all),
            "ok    config: /h/.config/rrq/config.toml\nFAIL  Rex app: no app. Fix: connect it\nskip  orphan sessions: skipped\n"
        );
    }
}
