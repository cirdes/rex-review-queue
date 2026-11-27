//! `Mux` sobre o CLI `rex`: uma chamada por operação (`rex api call`, `rex block call`).

use crate::mux::*;
use crate::runner::{Cmd, Output, Runner};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const TERMINAL: &str = "com.superlogical.terminal.shell";

pub struct RexCli<'a> {
    runner: &'a dyn Runner,
    bin: PathBuf,
}

impl<'a> RexCli<'a> {
    pub fn new(runner: &'a dyn Runner, bin: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            bin: bin.into(),
        }
    }

    fn exec(&self, args: Vec<String>) -> MuxResult<Value> {
        let cmd = Cmd::new(&self.bin.to_string_lossy())
            .args(args)
            .timeout(CALL_TIMEOUT);
        let out = self
            .runner
            .run(&cmd)
            .map_err(|e| MuxError::Unreachable(format!("{e:#}")))?;
        if out.success() {
            let text = out.stdout.trim();
            return if text.is_empty() {
                Ok(Value::Null)
            } else {
                serde_json::from_str(text)
                    .map_err(|e| MuxError::Other(format!("rex: invalid JSON: {e}")))
            };
        }
        Err(classify(&out))
    }

    fn api(&self, method: &str, args: Value) -> MuxResult<Value> {
        self.exec(vec![
            "api".into(),
            "call".into(),
            method.into(),
            args.to_string(),
        ])
    }

    fn block(
        &self,
        session: &str,
        block: &str,
        method: &str,
        args: Option<Value>,
    ) -> MuxResult<Value> {
        let mut a = vec![
            "block".into(),
            "call".into(),
            "-s".into(),
            session.into(),
            "-b".into(),
            block.into(),
            method.into(),
        ];
        if let Some(v) = args {
            a.push(v.to_string());
        }
        self.exec(a)
    }
}

/// Transforma a falha do CLI num `MuxError`. Fixtures em `tests/fixtures/rex/err_*.txt`.
pub fn classify(out: &Output) -> MuxError {
    let err = out.stderr.trim();
    if out.timed_out {
        return MuxError::Unreachable("rex timed out".into());
    }
    if out.status == Some(4) {
        return MuxError::Unreachable(err.into());
    }
    if err.contains("code=session_not_found") {
        return MuxError::SessionNotFound;
    }
    if err.contains("code=not_found") && err.contains(" client ") {
        return MuxError::ClientNotFound;
    }
    if err.contains("block target") && err.contains("not found") {
        return MuxError::BlockNotFound;
    }
    MuxError::Other(if err.is_empty() {
        format!("rex exited with {:?}", out.status)
    } else {
        err.into()
    })
}

fn s(v: &Value, k: &str) -> MuxResult<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| MuxError::Other(format!("rex: missing field {k}")))
}

fn block_layout(label: &str, cwd: &Path, initial_input: Option<&str>) -> Value {
    let mut options = json!({ "cwd": cwd.to_string_lossy() });
    if let Some(i) = initial_input {
        options["initial_input"] = json!(i);
    }
    json!({ "block": { "flavor": TERMINAL, "label": label, "options": options } })
}

pub fn create_args(req: &CreateReq) -> Value {
    json!({
        "label": req.label,
        "initial_windows": [{ "window_label": "main", "layout": block_layout(&req.block_label, &req.cwd, req.initial_input.as_deref()) }],
    })
}

pub fn split_args(req: &SplitReq) -> Value {
    json!({
        "session_id": req.session,
        "anchor_id": req.anchor,
        "direction": "horizontal",
        "side": "after",
        "ratio": req.ratio,
        "focus": req.focus,
        "layout": block_layout(&req.label, &req.cwd, req.initial_input.as_deref()),
    })
}

pub fn perform_select_args(client: &str, session: &str) -> Value {
    json!({ "client_id": client, "action": "session.select", "args": { "session_id": session } })
}

pub fn parse_created(v: &Value) -> MuxResult<Created> {
    let session_id = s(v, "session_id")?;
    let block_id = v
        .pointer("/initial_windows/0/block_ids/0")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| MuxError::Other("rex: session.create without block_ids".into()))?;
    Ok(Created {
        session_id,
        block_id,
    })
}

fn arr<'v>(v: &'v Value, k: &str, what: &str) -> MuxResult<&'v Vec<Value>> {
    v.get(k)
        .and_then(Value::as_array)
        .ok_or_else(|| MuxError::Other(format!("rex: {what} without {k}")))
}

/// Blocos da sessão. Resposta fora do formato é erro, nunca "sessão sem blocos": quem fecha
/// sessões não pode confundir uma resposta que não entendeu com uma sessão vazia.
pub fn parse_view(v: &Value) -> MuxResult<Vec<BlockInfo>> {
    let mut out = vec![];
    for w in arr(v, "windows", "session.view")? {
        for l in arr(w, "layers", "session.view window")? {
            for b in arr(l, "blocks", "session.view layer")? {
                let id = b.get("block_id").and_then(Value::as_str).ok_or_else(|| {
                    MuxError::Other("rex: session.view block without block_id".into())
                })?;
                out.push(BlockInfo {
                    id: id.into(),
                    label: b.get("label").and_then(Value::as_str).map(String::from),
                });
            }
        }
    }
    Ok(out)
}

pub fn parse_process(v: &Value) -> Option<Process> {
    let f = v.get("foreground")?;
    Some(Process {
        pid: f.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        name: f
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        path: f.get("path").and_then(Value::as_str).map(PathBuf::from),
        argv0: f.get("argv0").and_then(Value::as_str).map(String::from),
        cwd: f.get("cwd").and_then(Value::as_str).map(PathBuf::from),
    })
}

pub fn parse_apps(v: &Value) -> MuxResult<Vec<ClientInfo>> {
    let mut out = vec![];
    for c in arr(v, "clients", "client.list")? {
        if c.pointer("/info/kind").and_then(Value::as_str) != Some("app") {
            continue;
        }
        let id = c
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or_else(|| MuxError::Other("rex: client.list entry without client_id".into()))?;
        out.push(ClientInfo {
            id: id.into(),
            label: c
                .pointer("/info/label")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    Ok(out)
}

pub fn parse_sessions(v: &Value) -> MuxResult<Vec<SessionInfo>> {
    arr(v, "sessions", "session.list")?
        .iter()
        .map(|x| {
            let id = x.get("session_id").and_then(Value::as_str).ok_or_else(|| {
                MuxError::Other("rex: session.list entry without session_id".into())
            })?;
            Ok(SessionInfo {
                id: id.into(),
                label: x
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            })
        })
        .collect()
}

impl Mux for RexCli<'_> {
    fn server_api_revision(&self) -> MuxResult<u32> {
        let v = self.api("server.version", json!({}))?;
        v.get("api_revision")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .ok_or_else(|| MuxError::Other("rex: server.version without api_revision".into()))
    }

    fn list_sessions(&self) -> MuxResult<Vec<SessionInfo>> {
        parse_sessions(&self.api("session.list", json!({}))?)
    }

    fn create_session(&self, req: &CreateReq) -> MuxResult<Created> {
        parse_created(&self.api("session.create", create_args(req))?)
    }

    fn view(&self, session: &str) -> MuxResult<Vec<BlockInfo>> {
        parse_view(&self.api("session.view", json!({ "session_id": session }))?)
    }

    fn session_of(&self, block: &str) -> MuxResult<String> {
        let v = self.exec(vec![
            "api".into(),
            "call".into(),
            "-s".into(),
            block.into(),
            "session.view".into(),
        ])?;
        v.get("session_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| MuxError::Other("rex: session.view without session_id".into()))
    }

    fn destroy_session(&self, session: &str) -> MuxResult<()> {
        self.api("session.destroy", json!({ "session_id": session }))
            .map(|_| ())
    }

    fn inspect_block(&self, session: &str, block: &str) -> MuxResult<Option<Process>> {
        Ok(parse_process(&self.block(session, block, "process", None)?))
    }

    fn new_split(&self, req: &SplitReq) -> MuxResult<String> {
        let v = self.api("session.new_split", split_args(req))?;
        v.pointer("/block_ids/0")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| MuxError::Other("rex: new_split without block_ids".into()))
    }

    fn write(&self, session: &str, block: &str, text: &str) -> MuxResult<()> {
        self.block(session, block, "write", Some(json!({ "data": text })))
            .map(|_| ())
    }

    fn focus_block(&self, session: &str, block: &str) -> MuxResult<()> {
        self.api(
            "session.focus_block",
            json!({ "session_id": session, "block_id": block }),
        )
        .map(|_| ())
    }

    fn apps(&self) -> MuxResult<Vec<ClientInfo>> {
        parse_apps(&self.api("client.list", json!({}))?)
    }

    fn select_session(&self, client: &str, session: &str) -> MuxResult<()> {
        self.api("client.perform", perform_select_args(client, session))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/rex")
                .join(name),
        )
        .unwrap()
    }

    fn json_fx(name: &str) -> Value {
        serde_json::from_str(&fx(name)).unwrap()
    }

    /// O texto de erro e o exit code gravados (`exit=N` na última linha).
    fn err_fx(name: &str) -> Output {
        let text = fx(name);
        let (body, code) = text.trim_end().rsplit_once("\nexit=").unwrap();
        Output::fail(code.parse().unwrap(), body)
    }

    #[test]
    fn classifies_every_captured_error() {
        assert_eq!(
            classify(&err_fx("err_client_not_found.txt")),
            MuxError::ClientNotFound
        );
        assert_eq!(
            classify(&err_fx("err_view_session_not_found.txt")),
            MuxError::SessionNotFound
        );
        assert_eq!(
            classify(&err_fx("err_block_not_found.txt")),
            MuxError::BlockNotFound
        );
        assert!(matches!(
            classify(&err_fx("err_unreachable.txt")),
            MuxError::Unreachable(_)
        ));
        // o select numa sessão que sumiu volta como `code=internal`: não dá para chamar de SessionNotFound
        assert!(
            matches!(classify(&err_fx("err_select_missing_session.txt")), MuxError::Other(m) if m.contains("code=internal"))
        );
        assert!(matches!(
            classify(&Output {
                timed_out: true,
                ..Default::default()
            }),
            MuxError::Unreachable(_)
        ));
    }

    #[test]
    fn parses_captured_responses() {
        assert_eq!(json_fx("server_version.json")["api_revision"], 24);
        let c = parse_created(&json_fx("session_create.json")).unwrap();
        assert!(c.session_id.starts_with("session:") && c.block_id.starts_with("block:"));
        let blocks = parse_view(&json_fx("session_view.json")).unwrap();
        assert_eq!(
            blocks
                .iter()
                .map(|b| b.label.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("shell"), Some("claude")]
        );
        let running = parse_process(&json_fx("block_process_running.json")).unwrap();
        assert_eq!(running.name, "sleep");
        assert!(!is_shell(&running));
        let shell = parse_process(&json_fx("block_process_shell.json")).unwrap();
        assert!(is_shell(&shell), "{shell:?}");
        assert_eq!(
            parse_process(&json!({"last_exit": {"exit_code": 3}})),
            None,
            "programa que terminou: sem foreground"
        );
        let apps = parse_apps(&json_fx("client_list.json")).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].label.as_deref(), Some("macbook"));
    }

    #[test]
    fn malformed_responses_are_errors_not_empty_lists() {
        // uma sessão "sem blocos" pareceria ociosa para quem fecha sessões
        assert!(matches!(parse_view(&json!({})), Err(MuxError::Other(_))));
        assert!(matches!(
            parse_view(&json!({"windows": [{"label": "w"}]})),
            Err(MuxError::Other(_))
        ));
        assert!(matches!(
            parse_view(&json!({"windows": [{"layers": [{"blocks": [{"label": "x"}]}]}]})),
            Err(MuxError::Other(_))
        ));
        assert_eq!(parse_view(&json!({"windows": []})).unwrap(), vec![]);
        assert!(matches!(
            parse_apps(&json!({"ok": true})),
            Err(MuxError::Other(_))
        ));
        assert!(matches!(
            parse_apps(&json!({"clients": [{"info": {"kind": "app"}}]})),
            Err(MuxError::Other(_))
        ));
        assert!(matches!(
            parse_sessions(&json!({})),
            Err(MuxError::Other(_))
        ));
        assert!(matches!(
            parse_sessions(&json!({"sessions": [{"label": "x"}]})),
            Err(MuxError::Other(_))
        ));
        assert_eq!(
            parse_sessions(&json_fx("session_list.json")).unwrap().len(),
            1
        );
    }

    #[test]
    fn payloads_put_the_command_in_initial_input_and_the_label_on_the_block() {
        let req = CreateReq {
            label: "#7 repo".into(),
            cwd: "/w/wt".into(),
            block_label: "shell".into(),
            initial_input: Some("rrq tui\r".into()),
        };
        let v = create_args(&req);
        let block = &v["initial_windows"][0]["layout"]["block"];
        assert_eq!(block["label"], "shell");
        assert_eq!(block["flavor"], TERMINAL);
        assert_eq!(block["options"]["initial_input"], "rrq tui\r");
        assert!(block["options"].get("command").is_none());
        let split = split_args(&SplitReq {
            session: "s".into(),
            anchor: "b".into(),
            ratio: 0.45,
            label: "reviewr".into(),
            cwd: "/w".into(),
            initial_input: None,
            focus: false,
        });
        assert_eq!(split["anchor_id"], "b");
        assert_eq!(split["ratio"], 0.45);
        assert!(split["layout"]["block"]["options"]
            .get("initial_input")
            .is_none());
        assert_eq!(perform_select_args("c", "s")["action"], "session.select");
    }

    #[test]
    fn cli_calls_rex_with_the_method_and_maps_failures() {
        let r = FakeRunner::new();
        r.on(
            "api call server.version",
            Output::ok(&fx("server_version.json")),
        );
        r.on(
            "api call client.perform",
            err_fx("err_client_not_found.txt"),
        );
        r.on(
            "block call -s s1 -b b9 process",
            err_fx("err_block_not_found.txt"),
        );
        r.on("api call session.destroy", Output::ok("{}"));
        r.on(
            "api call -s block:7 session.view",
            Output::ok(&fx("session_view.json")),
        );
        let cli = RexCli::new(&r, "/x/rex");
        let sid = cli.session_of("block:7").unwrap();
        assert!(sid.starts_with("session:"), "{sid}");
        assert_eq!(cli.server_api_revision().unwrap(), 24);
        assert_eq!(cli.select_session("c", "s"), Err(MuxError::ClientNotFound));
        assert_eq!(cli.inspect_block("s1", "b9"), Err(MuxError::BlockNotFound));
        cli.destroy_session("s1").unwrap();
        assert!(
            r.lines().iter().all(|l| l.starts_with("/x/rex ")),
            "{:?}",
            r.lines()
        );
        // runner que nem consegue rodar o binário: servidor inacessível
        assert!(matches!(cli.list_sessions(), Err(MuxError::Unreachable(_))));
    }

    /// Contrato contra o servidor Rex real. Cria e fecha uma sessão descartável.
    /// `cargo test -- --ignored rex_contract`
    #[test]
    #[ignore]
    fn rex_contract_against_the_real_server() {
        use crate::runner::RealRunner;
        let bin = std::env::var("RRQ_REX_BIN").unwrap_or_else(|_| "rex".into());
        let cli = RexCli::new(&RealRunner, bin);
        assert!(cli.server_api_revision().unwrap() >= 24);
        let dir = tempfile::tempdir().unwrap();
        let c = cli
            .create_session(&CreateReq {
                label: "rrq-contract".into(),
                cwd: dir.path().into(),
                block_label: "shell".into(),
                initial_input: None,
            })
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(cli
                .list_sessions()
                .unwrap()
                .iter()
                .any(|s| s.id == c.session_id));
            let split = cli
                .new_split(&SplitReq {
                    session: c.session_id.clone(),
                    anchor: c.block_id.clone(),
                    ratio: 0.5,
                    label: "claude".into(),
                    cwd: dir.path().into(),
                    initial_input: Some("sleep 30\r".into()),
                    focus: false,
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            let labels: Vec<_> = cli
                .view(&c.session_id)
                .unwrap()
                .into_iter()
                .filter_map(|b| b.label)
                .collect();
            assert_eq!(labels, vec!["shell", "claude"]);
            let sleeping = cli.inspect_block(&c.session_id, &split).unwrap().unwrap();
            assert_eq!(sleeping.name, "sleep");
            assert!(in_worktree(&sleeping, dir.path()));
            let shell = cli
                .inspect_block(&c.session_id, &c.block_id)
                .unwrap()
                .unwrap();
            assert!(is_shell(&shell), "{shell:?}");
            assert_eq!(cli.session_of(&split).unwrap(), c.session_id);
            cli.write(&c.session_id, &c.block_id, "true\r").unwrap();
            cli.focus_block(&c.session_id, &split).unwrap();
            assert_eq!(
                cli.inspect_block(&c.session_id, "block:01a21d7d-0000-7000-8000-000000000000"),
                Err(MuxError::BlockNotFound)
            );
        }));
        cli.destroy_session(&c.session_id).unwrap();
        assert_eq!(cli.view(&c.session_id), Err(MuxError::SessionNotFound));
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
