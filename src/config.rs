use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    pub worktrees_dir: PathBuf,
    #[serde(default = "default_step_timeout")]
    pub step_timeout_secs: u64,
    /// Espera depois que o PR sai da lista antes de remover o worktree.
    #[serde(default = "default_remove_grace")]
    pub remove_grace_secs: u64,
    /// Cadência da coleta "feitas hoje" (mínimo 60).
    #[serde(default = "default_reviews_today_every")]
    pub reviews_today_every_secs: u64,
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub rex: RexConfig,
    #[serde(default)]
    pub combo: ComboConfig,
}

/// Como o rrq fala com o Rex.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RexConfig {
    /// Caminho absoluto do CLI `rex` (o LaunchAgent não herda o PATH do shell). `~` é expandido.
    #[serde(default = "default_rex_bin")]
    pub bin: PathBuf,
    /// Label do app do Rex cuja tela o Enter troca.
    #[serde(default = "default_app_label")]
    pub app_label: String,
}

impl Default for RexConfig {
    fn default() -> Self {
        Self { bin: default_rex_bin(), app_label: default_app_label() }
    }
}

/// O que o `c` abre ao lado do shell do PR. Cada um é um argv; `{worktree}` vira o caminho.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComboConfig {
    #[serde(default = "default_combo_claude")]
    pub claude: Vec<String>,
    #[serde(default = "default_combo_reviewr")]
    pub reviewr: Vec<String>,
}

impl Default for ComboConfig {
    fn default() -> Self {
        Self { claude: default_combo_claude(), reviewr: default_combo_reviewr() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    pub path: PathBuf,
    pub setup: Vec<String>,
    #[serde(default = "default_disposable")]
    pub disposable_ignored: Vec<String>,
}

fn default_poll_interval() -> u64 { 60 }
fn default_step_timeout() -> u64 { 900 }
pub const DEFAULT_REMOVE_GRACE_SECS: u64 = 900;
fn default_remove_grace() -> u64 { DEFAULT_REMOVE_GRACE_SECS }
fn default_reviews_today_every() -> u64 { 300 }

/// Limita a um valor que o chrono representa sem pânico.
pub fn grace_duration(secs: u64) -> chrono::Duration {
    chrono::Duration::seconds(secs.min(i32::MAX as u64) as i64)
}
fn default_rex_bin() -> PathBuf { PathBuf::from("~/.local/bin/rex") }
fn default_app_label() -> String { "macbook".into() }
fn default_combo_claude() -> Vec<String> { vec!["claude".into()] }
fn default_combo_reviewr() -> Vec<String> { vec!["reviewr".into(), "{worktree}".into()] }
fn default_disposable() -> Vec<String> {
    ["node_modules", "vendor/bundle", "tmp", "log", ".bundle"].iter().map(|s| s.to_string()).collect()
}

pub fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Chaves do herdr-reviewq que não existem no rrq. Têm mensagem própria, que diz o que fazer.
const OLD_KEYS: &[&str] = &["herdr_session", "backend", "window", "notify"];

/// As chaves antigas presentes no texto, como aparecem para o autor (`herdr_session`, `[notify]`).
pub fn old_keys(text: &str) -> Vec<String> {
    let Ok(toml::Value::Table(t)) = text.parse::<toml::Value>() else { return vec![] };
    OLD_KEYS
        .iter()
        .filter(|k| t.contains_key(**k))
        .map(|k| if *k == "notify" { "[notify]".to_string() } else { format!("`{k}`") })
        .collect()
}

/// O texto sem as chaves antigas, para o `doctor` conferir o resto da config.
pub fn without_old_keys(text: &str) -> Option<String> {
    let toml::Value::Table(mut t) = text.parse::<toml::Value>().ok()? else { return None };
    for k in OLD_KEYS {
        t.remove(*k);
    }
    toml::to_string(&t).ok()
}

fn reject_old_keys(text: &str) -> Result<()> {
    if let Some(what) = old_keys(text).first() {
        bail!("config key {what} is from herdr-reviewq and no longer exists; remove it from config.toml (README, \"Migração\")");
    }
    Ok(())
}

impl Config {
    pub fn parse(text: &str, home: &Path) -> Result<Config> {
        reject_old_keys(text)?;
        let mut cfg: Config = toml::from_str(text).context("invalid config.toml")?;
        cfg.worktrees_dir = expand_tilde(&cfg.worktrees_dir, home);
        for repo in &mut cfg.repos {
            repo.path = expand_tilde(&repo.path, home);
        }
        cfg.rex.bin = expand_tilde(&cfg.rex.bin, home);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path, home: &Path) -> Result<Config> {
        if !path.exists() {
            // Quem instalou pelo marketplace cai aqui na primeira vez que abre o painel
            bail!(
                "missing config: copy {} to {}, edit the [[repos]] and run `rrq start`",
                path.with_file_name("config.example.toml").display(),
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Self::parse(&text, home)
    }

    pub fn remove_grace(&self) -> chrono::Duration {
        grace_duration(self.remove_grace_secs)
    }

    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name == name)
    }

    fn validate(&self) -> Result<()> {
        if self.repos.is_empty() {
            bail!("the config needs at least one [[repos]]");
        }
        for repo in &self.repos {
            let parts: Vec<&str> = repo.name.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
                bail!("invalid repo: {:?} (use owner/name)", repo.name);
            }
        }
        if self.poll_interval_secs < 15 || self.poll_interval_secs > 3600 {
            bail!("poll_interval_secs must be between 15 and 3600");
        }
        if self.reviews_today_every_secs < 60 || self.reviews_today_every_secs > 86400 {
            bail!("reviews_today_every_secs must be between 60 and 86400");
        }
        if self.remove_grace_secs > 604800 {
            bail!("remove_grace_secs must be between 0 and 604800");
        }
        if self.step_timeout_secs < 1 || self.step_timeout_secs > 86400 {
            bail!("step_timeout_secs must be between 1 and 86400");
        }
        if self.rex.app_label.trim().is_empty() {
            bail!("rex.app_label cannot be empty");
        }
        for (name, argv) in [("combo.claude", &self.combo.claude), ("combo.reviewr", &self.combo.reviewr)] {
            if argv.first().is_none_or(|a| a.trim().is_empty()) {
                bail!("{name} needs a program as its first item");
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for repo in &self.repos {
            if !seen.insert(repo.name.as_str()) {
                bail!("duplicate repo in the config: {}", repo.name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_config_says_how_to_create_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let err = format!("{:#}", Config::load(&path, dir.path()).unwrap_err());
        assert!(err.contains("config.example.toml"), "{err}");
        assert!(err.contains("rrq start"), "{err}");
    }

    use super::*;
    use std::path::Path;

    const FULL: &str = r#"
poll_interval_secs = 30
worktrees_dir = "~/Workspaces/.reviews"
[[repos]]
name = "acme/app"
path = "~/Workspaces/app"
setup = ["mise trust", "bundle install"]
"#;

    #[test]
    fn parses_and_expands_tilde() {
        let cfg = Config::parse(FULL, Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.worktrees_dir, Path::new("/Users/x/Workspaces/.reviews"));
        assert_eq!(cfg.repos[0].path, Path::new("/Users/x/Workspaces/app"));
        assert_eq!(cfg.repos[0].disposable_ignored, vec!["node_modules", "vendor/bundle", "tmp", "log", ".bundle"]);
        assert_eq!(cfg.step_timeout_secs, 900);
        assert_eq!(cfg.remove_grace_secs, 900);
        assert!(cfg.repo("acme/app").is_some());
        assert!(cfg.repo("outro/repo").is_none());
    }

    #[test]
    fn rex_and_combo_have_defaults_and_expand_tilde() {
        let cfg = Config::parse(FULL, Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.rex.bin, Path::new("/Users/x/.local/bin/rex"));
        assert_eq!(cfg.rex.app_label, "macbook");
        assert_eq!(cfg.combo.claude, vec!["claude"]);
        assert_eq!(cfg.combo.reviewr, vec!["reviewr", "{worktree}"]);
        let text = format!("{FULL}[rex]\nbin = \"~/bin/rex\"\napp_label = \"mini\"\n[combo]\nclaude = [\"claude\", \"--resume\"]\n");
        let cfg = Config::parse(&text, Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.rex.bin, Path::new("/Users/x/bin/rex"));
        assert_eq!(cfg.rex.app_label, "mini");
        assert_eq!(cfg.combo.claude, vec!["claude", "--resume"]);
        assert_eq!(cfg.combo.reviewr, vec!["reviewr", "{worktree}"], "a chave ausente fica no padrão");
    }

    #[test]
    fn rejects_empty_combo_and_unknown_rex_keys() {
        assert!(Config::parse(&format!("{FULL}[combo]\nclaude = []\n"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("{FULL}[combo]\nreviewr = [\" \"]\n"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("{FULL}[rex]\napp_label = \"\"\n"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("{FULL}[rex]\nlabel = \"x\"\n"), Path::new("/h")).is_err());
    }

    #[test]
    fn rejects_empty_repos() {
        let err = Config::parse("worktrees_dir = \"/w\"\nrepos = []\n", Path::new("/h")).unwrap_err();
        assert!(err.to_string().contains("[[repos]]"));
    }

    #[test]
    fn rejects_bad_repo_name() {
        let text = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"app\"\npath = \"/r\"\nsetup = []\n";
        assert!(Config::parse(text, Path::new("/h")).is_err());
    }

    #[test]
    fn rejects_short_poll() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        assert!(Config::parse(&format!("poll_interval_secs = 5\n{base}"), Path::new("/h")).is_err());
    }

    #[test]
    fn old_keys_are_all_listed_and_can_be_stripped() {
        let text = "herdr_session = \"main\"\nworktrees_dir = \"/w\"\n[notify]\nsound = true\n[[repos]]\nname = \"o/r\"\npath = \"/r\"\nsetup = []\n";
        assert_eq!(old_keys(text), vec!["`herdr_session`", "[notify]"]);
        let rest = without_old_keys(text).unwrap();
        assert!(old_keys(&rest).is_empty());
        assert!(Config::parse(&rest, Path::new("/h")).is_ok(), "{rest}");
        assert!(old_keys("isto não é toml [").is_empty());
    }

    #[test]
    fn herdr_keys_are_rejected_by_name() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        for (old, shown) in [
            ("herdr_session = \"main\"\n", "`herdr_session`"),
            ("backend = \"herdr-app\"\n", "`backend`"),
            ("window = \"remote\"\n", "`window`"),
        ] {
            let err = format!("{:#}", Config::parse(&format!("{old}{base}"), Path::new("/h")).unwrap_err());
            assert!(err.contains(shown) && err.contains("README"), "{err}");
        }
        let err = format!("{:#}", Config::parse(&format!("{base}[notify]\nsound = \"done\"\n"), Path::new("/h")).unwrap_err());
        assert!(err.contains("[notify]") && err.contains("README"), "{err}");
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths() {
        assert_eq!(expand_tilde(Path::new("/abs/p"), Path::new("/h")), Path::new("/abs/p"));
        assert_eq!(expand_tilde(Path::new("~/p"), Path::new("/h")), Path::new("/h/p"));
    }

    #[test]
    fn rejects_unknown_keys() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        let err = Config::parse(&format!("{base}disposable_ignore = [\"tmp\"]\n"), Path::new("/h")).unwrap_err();
        assert!(format!("{err:#}").contains("disposable_ignore"), "{err:#}");
        assert!(Config::parse(&format!("poll_interval = 30\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("{base}[combo]\nclaud = [\"x\"]\n"), Path::new("/h")).is_err());
    }

    #[test]
    fn example_config_parses() {
        let cfg = Config::parse(include_str!("../config.example.toml"), Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.repos[0].name, "owner/repo");
        assert_eq!(cfg.remove_grace_secs, 900);
    }

    #[test]
    fn reviews_today_cadence_and_duplicates() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        let cfg = Config::parse(base, Path::new("/h")).unwrap();
        assert_eq!(cfg.reviews_today_every_secs, 300);
        assert!(Config::parse(&format!("reviews_today_every_secs = 30\n{base}"), Path::new("/h")).is_err());
        let dup = format!("{base}[[repos]]\nname = \"a/b\"\npath = \"/r2\"\nsetup = []\n");
        assert!(Config::parse(&dup, Path::new("/h")).unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn boundary_values_valid() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        // poll_interval_secs: 15–3600
        assert!(Config::parse(&format!("poll_interval_secs = 15\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("poll_interval_secs = 3600\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("poll_interval_secs = 14\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("poll_interval_secs = 3601\n{base}"), Path::new("/h")).is_err());

        // reviews_today_every_secs: 60–86400
        assert!(Config::parse(&format!("reviews_today_every_secs = 60\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("reviews_today_every_secs = 86400\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("reviews_today_every_secs = 59\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("reviews_today_every_secs = 86401\n{base}"), Path::new("/h")).is_err());

        // remove_grace_secs: 0–604800
        assert!(Config::parse(&format!("remove_grace_secs = 0\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("remove_grace_secs = 604800\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("remove_grace_secs = 604801\n{base}"), Path::new("/h")).is_err());

        // step_timeout_secs: 1–86400
        assert!(Config::parse(&format!("step_timeout_secs = 1\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("step_timeout_secs = 86400\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("step_timeout_secs = 0\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("step_timeout_secs = 86401\n{base}"), Path::new("/h")).is_err());
    }
}
