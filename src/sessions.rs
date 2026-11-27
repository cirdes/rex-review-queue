//! `sessions.json`: o mapa PR → sessão do Rex. O painel cria entradas (Enter/`c`) e o daemon
//! remove (fechamento). Toda escrita é atômica (tmp + rename) e feita sob `sessions.lock`.

use crate::locks;
use crate::paths::Paths;
use crate::state::PrKey;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: String,
    /// Bloco do shell da worktree: âncora dos splits do combo.
    pub shell_block: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub entered_at: Option<DateTime<Utc>>,
}

/// Chave é `PrKey::to_string()` (`owner/repo#123`), como no `State`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sessions {
    #[serde(default)]
    pub prs: BTreeMap<String, SessionEntry>,
}

#[derive(Debug)]
pub struct Corrupt {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for Corrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is corrupt: {}", self.path.display(), self.reason)
    }
}

impl std::error::Error for Corrupt {}

impl Sessions {
    pub fn get(&self, key: &PrKey) -> Option<&SessionEntry> {
        self.prs.get(&key.to_string())
    }
    pub fn list(&self) -> impl Iterator<Item = (PrKey, &SessionEntry)> {
        self.prs
            .iter()
            .filter_map(|(k, e)| Some((PrKey::parse(k)?, e)))
    }
    pub fn entered(&self, key: &PrKey) -> bool {
        self.get(key).is_some_and(|e| e.entered_at.is_some())
    }
}

/// Lê o arquivo. Ausente = vazio. Ilegível = `Corrupt`, que cada lado trata do seu jeito.
pub fn load(path: &Path) -> std::result::Result<Sessions, Corrupt> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Sessions::default()),
        Err(e) => {
            return Err(Corrupt {
                path: path.into(),
                reason: e.to_string(),
            })
        }
    };
    serde_json::from_str(&text).map_err(|e| Corrupt {
        path: path.into(),
        reason: e.to_string(),
    })
}

/// Para o TUI: arquivo corrompido vira mapa vazio e uma mensagem para o rodapé. Só o daemon
/// isola o arquivo (`quarantine`), para a corrupção nunca se perder num rename feito pelo TUI.
pub fn load_for_panel(path: &Path) -> (Sessions, Option<String>) {
    match load(path) {
        Ok(s) => (s, None),
        Err(c) => (Sessions::default(), Some(c.to_string())),
    }
}

/// Para o daemon: move o arquivo corrompido para `.corrupt-<ts>` e devolve o caminho novo.
pub fn quarantine(path: &Path, now: DateTime<Utc>) -> Result<PathBuf> {
    let base = format!("json.corrupt-{}", now.format("%Y%m%dT%H%M%SZ"));
    // duas quarentenas no mesmo segundo não podem sobrescrever a cópia anterior
    let dest = (0..)
        .map(|n| {
            path.with_extension(if n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            })
        })
        .find(|d| !d.exists())
        .expect("iterador infinito");
    std::fs::rename(path, &dest)
        .with_context(|| format!("could not move {} aside", path.display()))?;
    Ok(dest)
}

fn write_atomic(path: &Path, s: &Sessions) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Lê, aplica `f` e grava, tudo sob `sessions.lock`. Nenhuma chamada ao `rex` pode acontecer
/// dentro de `f`. Arquivo corrompido devolve erro: só o daemon decide isolá-lo.
pub fn update<T>(paths: &Paths, f: impl FnOnce(&mut Sessions) -> T) -> Result<T> {
    let _lock = locks::sessions_lock(paths)?;
    let file = paths.sessions_file();
    let mut s = load(&file)?;
    let out = f(&mut s);
    write_atomic(&file, &s)?;
    Ok(out)
}

pub fn upsert(paths: &Paths, key: &PrKey, entry: SessionEntry) -> Result<()> {
    update(paths, |s| {
        s.prs.insert(key.to_string(), entry);
    })
}

pub fn remove(paths: &Paths, key: &PrKey) -> Result<Option<SessionEntry>> {
    update(paths, |s| s.prs.remove(&key.to_string()))
}

/// Marca que o autor entrou na sessão do PR (apaga o "review pronta" do 7501).
pub fn mark_entered(paths: &Paths, key: &PrKey, now: DateTime<Utc>) -> Result<bool> {
    update(paths, |s| match s.prs.get_mut(&key.to_string()) {
        Some(e) => {
            e.entered_at.get_or_insert(now);
            true
        }
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            session_id: id.into(),
            shell_block: "block:1".into(),
            created_at: Utc::now(),
            entered_at: None,
        }
    }

    #[test]
    fn round_trip_with_string_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        let key = PrKey::new("o/r", 7);
        assert!(
            load(&p.sessions_file()).unwrap().prs.is_empty(),
            "arquivo ausente = vazio"
        );
        upsert(&p, &key, entry("session:1")).unwrap();
        let s = load(&p.sessions_file()).unwrap();
        assert_eq!(s.get(&key).unwrap().session_id, "session:1");
        assert!(!s.entered(&key));
        let raw = std::fs::read_to_string(p.sessions_file()).unwrap();
        assert!(raw.contains("\"o/r#7\""), "{raw}");
        assert_eq!(
            s.list().map(|(k, _)| k).collect::<Vec<_>>(),
            vec![key.clone()]
        );
    }

    #[test]
    fn mark_entered_keeps_the_first_time_and_ignores_unknown_prs() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        let key = PrKey::new("o/r", 7);
        assert!(!mark_entered(&p, &key, Utc::now()).unwrap());
        upsert(&p, &key, entry("session:1")).unwrap();
        let first = Utc::now() - chrono::Duration::minutes(5);
        assert!(mark_entered(&p, &key, first).unwrap());
        mark_entered(&p, &key, Utc::now()).unwrap();
        assert_eq!(
            load(&p.sessions_file())
                .unwrap()
                .get(&key)
                .unwrap()
                .entered_at,
            Some(first)
        );
        assert_eq!(remove(&p, &key).unwrap().unwrap().session_id, "session:1");
        assert!(remove(&p, &key).unwrap().is_none());
    }

    #[test]
    fn writes_leave_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        upsert(&p, &PrKey::new("o/r", 1), entry("s")).unwrap();
        let names: Vec<_> = std::fs::read_dir(&p.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    }

    #[test]
    fn corrupt_file_is_empty_for_the_panel_and_quarantined_by_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        std::fs::create_dir_all(&p.state_dir).unwrap();
        std::fs::write(p.sessions_file(), "{ não é json").unwrap();
        let (s, msg) = load_for_panel(&p.sessions_file());
        assert!(s.prs.is_empty());
        assert!(msg.unwrap().contains("corrupt"));
        assert!(p.sessions_file().exists(), "o painel não mexe no arquivo");
        assert!(
            update(&p, |_| ()).is_err(),
            "escrita não sobrescreve um arquivo corrompido"
        );
        let moved = quarantine(&p.sessions_file(), Utc::now()).unwrap();
        assert!(moved.exists() && !p.sessions_file().exists());
        upsert(&p, &PrKey::new("o/r", 1), entry("s")).unwrap();
    }

    #[test]
    fn two_quarantines_in_the_same_second_keep_both_copies() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        std::fs::create_dir_all(&p.state_dir).unwrap();
        let now = Utc::now();
        std::fs::write(p.sessions_file(), "primeira").unwrap();
        let a = quarantine(&p.sessions_file(), now).unwrap();
        std::fs::write(p.sessions_file(), "segunda").unwrap();
        let b = quarantine(&p.sessions_file(), now).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "primeira");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "segunda");
    }

    #[test]
    fn concurrent_writers_do_not_lose_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::from_home(dir.path().into());
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    upsert(&p, &PrKey::new("o/r", i), entry(&format!("s{i}"))).unwrap()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(load(&p.sessions_file()).unwrap().prs.len(), 8);
    }
}
