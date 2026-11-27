//! Flocks do rrq. Cada arquivo tem um propósito e nunca é apagado com processos rodando:
//! apagar um arquivo de flock que outro processo tem aberto quebra a exclusão.
//!
//! Ordem de aquisição quando dois são necessários: lock do PR, depois `sessions.lock`.

use crate::paths::Paths;
use crate::state::PrKey;
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Espera do `sessions.lock`: só cobre ler, modificar e renomear o arquivo.
pub const SESSIONS_WAIT: Duration = Duration::from_secs(2);
/// Quanto o painel espera pelo lock de emissor antes de concluir que outro painel emite.
pub const PANEL_WAIT: Duration = Duration::from_millis(300);

fn open(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

/// Pega o flock exclusivo de `path`, esperando até `wait`. Solto quando o `File` cai.
pub fn acquire(path: &Path, wait: Duration, what: &str) -> Result<File> {
    let f = open(path)?;
    let start = Instant::now();
    loop {
        if f.try_lock_exclusive().is_ok() {
            return Ok(f);
        }
        let elapsed = start.elapsed();
        if elapsed >= wait {
            bail!("{what} is busy ({})", path.display());
        }
        std::thread::sleep(Duration::from_millis(20).min(wait - elapsed));
    }
}

/// FNV-1a de 32 bits: estável entre versões e sem dependência nova.
pub fn fnv32(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// Nome do arquivo de lock do PR. O hash separa `a-b/c` de `a/b-c`, que o `replace` juntaria.
pub fn pr_lock_file(paths: &Paths, key: &PrKey) -> PathBuf {
    let flat = key.repo.replace('/', "-");
    paths.locks_dir().join(format!(
        "pr-{flat}-{}-{:08x}.lock",
        key.number,
        fnv32(&key.to_string())
    ))
}

pub fn pr_lock(paths: &Paths, key: &PrKey, wait: Duration) -> Result<File> {
    acquire(&pr_lock_file(paths, key), wait, &format!("{key}"))
}

pub fn sessions_lock(paths: &Paths) -> Result<File> {
    acquire(
        &paths.locks_dir().join("sessions.lock"),
        SESSIONS_WAIT,
        "sessions.json",
    )
}

/// O TUI que conseguir este lock é o único que emite OSC 7501. Um segundo painel aberto ao
/// mesmo tempo funciona, mas não emite, para os dois não disputarem o indicador.
pub fn try_panel_emitter(paths: &Paths) -> Result<Option<File>> {
    // espera curta: um fork de outro processo pode segurar o fd por alguns ms até o exec
    match acquire(&paths.locks_dir().join("panel.lock"), PANEL_WAIT, "panel") {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.to_string().contains("is busy") => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths::from_home(dir.to_path_buf())
    }

    #[test]
    fn pr_lock_files_do_not_collide_and_stay_put() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let a = pr_lock_file(&p, &PrKey::new("a-b/c", 1));
        let b = pr_lock_file(&p, &PrKey::new("a/b-c", 1));
        assert_ne!(a, b);
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pr-a-b-c-1-"));
        let held = pr_lock(&p, &PrKey::new("o/r", 7), Duration::ZERO).unwrap();
        drop(held);
        assert!(
            pr_lock_file(&p, &PrKey::new("o/r", 7)).exists(),
            "o arquivo fica depois de soltar"
        );
    }

    #[test]
    fn pr_lock_is_exclusive_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let key = PrKey::new("o/r", 7);
        let held = pr_lock(&p, &key, Duration::ZERO).unwrap();
        let start = Instant::now();
        let err = pr_lock(&p, &key, Duration::from_millis(60))
            .unwrap_err()
            .to_string();
        assert!(err.contains("o/r#7 is busy"), "{err}");
        assert!(start.elapsed() >= Duration::from_millis(60));
        assert!(
            pr_lock(&p, &PrKey::new("o/r", 8), Duration::ZERO).is_ok(),
            "outro PR não espera"
        );
        drop(held);
        // um fork de outro teste pode herdar o fd por alguns ms até o exec: espera curta, como no Enter
        let r = pr_lock(&p, &key, Duration::from_millis(500));
        assert!(r.is_ok(), "{:?}", r.err());
    }

    #[test]
    fn only_one_panel_emits() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let first = try_panel_emitter(&p).unwrap();
        assert!(first.is_some());
        assert!(try_panel_emitter(&p).unwrap().is_none());
        drop(first);
        assert!(try_panel_emitter(&p).unwrap().is_some());
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv32(""), 0x811c_9dc5);
        assert_eq!(fnv32("o/r#7"), fnv32("o/r#7"));
        assert_ne!(fnv32("a-b/c#1"), fnv32("a/b-c#1"));
    }
}
