//! Update explícito: a release rolante `main` do GitHub traz `rrq-<target>.tar.gz`, o `.sha256`
//! ao lado e um `version.txt` com o SHA do commit. Nada é baixado sem o autor pedir; o daemon só
//! compara o SHA uma vez por dia e grava `update_available`.

use crate::runner::{Cmd, Runner};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REPO: &str = "cirdes/rex-review-queue";
pub const RELEASE: &str = "main";

/// SHA do commit deste binário (gravado pelo `build.rs`).
pub fn local_sha() -> &'static str {
    env!("RRQ_BUILD_SHA")
}

/// Target do tarball deste Mac.
pub fn target() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

fn gh_download(runner: &dyn Runner, extra: &[&str]) -> Result<crate::runner::Output> {
    let cmd = Cmd::new("gh")
        .args(["release", "download", RELEASE, "--repo", REPO])
        .args(extra.iter().copied())
        .timeout(Duration::from_secs(120));
    let out = runner.run(&cmd)?;
    if !out.success() {
        bail!("gh release download failed: {}", out.stderr.trim());
    }
    Ok(out)
}

/// SHA da build mais nova da `main`.
pub fn remote_sha(runner: &dyn Runner) -> Result<String> {
    let out = gh_download(runner, &["--pattern", "version.txt", "--output", "-"])?;
    let sha = out.stdout.trim().to_string();
    if sha.len() < 7 || sha.len() > 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("version.txt of the {RELEASE} release is not a commit SHA: {sha:?}");
    }
    Ok(sha)
}

/// O SHA remoto, se for diferente do SHA deste binário.
pub fn check(runner: &dyn Runner) -> Result<Option<String>> {
    check_against(runner, local_sha())
}

pub fn check_against(runner: &dyn Runner, local: &str) -> Result<Option<String>> {
    let remote = remote_sha(runner)?;
    Ok((remote != local).then_some(remote))
}

/// O que o `install` fez.
#[derive(Debug, Clone, PartialEq)]
pub enum Installed {
    UpToDate,
    Replaced { sha: String },
}

fn sha256_of(runner: &dyn Runner, file: &Path) -> Result<String> {
    let out = runner.run(
        &Cmd::new("shasum")
            .args(["-a", "256"])
            .arg(file.to_string_lossy()),
    )?;
    if !out.success() {
        bail!("shasum failed: {}", out.stderr.trim());
    }
    out.stdout
        .split_whitespace()
        .next()
        .map(str::to_lowercase)
        .context("shasum printed nothing")
}

/// O `rrq` extraído: na raiz do tarball ou numa pasta só.
fn find_binary(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("rrq");
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("rrq"))
        .find(|p| p.is_file())
}

/// Baixa a build nova para `work` (pasta descartável), confere o checksum, troca `bin` de forma
/// atômica (`<bin>.new` + rename na mesma pasta) e chama `restart`. Qualquer falha antes do
/// rename deixa o binário atual intacto.
pub fn install(
    runner: &dyn Runner,
    local: &str,
    bin: &Path,
    work: &Path,
    restart: &dyn Fn() -> Result<()>,
) -> Result<Installed> {
    let Some(sha) = check_against(runner, local)? else {
        return Ok(Installed::UpToDate);
    };
    if work.exists() {
        std::fs::remove_dir_all(work)
            .with_context(|| format!("could not clean {}", work.display()))?;
    }
    std::fs::create_dir_all(work)?;
    let tarball = format!("rrq-{}.tar.gz", target());
    let checksum = format!("{tarball}.sha256");
    gh_download(
        runner,
        &[
            "--pattern",
            &tarball,
            "--pattern",
            &checksum,
            "--dir",
            &work.to_string_lossy(),
        ],
    )?;
    let expected = std::fs::read_to_string(work.join(&checksum))
        .with_context(|| format!("{checksum} was not downloaded"))?
        .split_whitespace()
        .next()
        .map(str::to_lowercase)
        .context("empty checksum file")?;
    let got = sha256_of(runner, &work.join(&tarball))?;
    if got != expected {
        bail!("checksum of {tarball} does not match ({got} != {expected}); the binary was not replaced");
    }
    let unpacked = work.join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    let out = runner.run(
        &Cmd::new("tar")
            .args(["-xzf"])
            .arg(work.join(&tarball).to_string_lossy())
            .args(["-C"])
            .arg(unpacked.to_string_lossy()),
    )?;
    if !out.success() {
        bail!("could not extract {tarball}: {}", out.stderr.trim());
    }
    let new = find_binary(&unpacked).with_context(|| format!("{tarball} has no rrq binary"))?;
    let staged = bin.with_extension("new");
    if let Some(dir) = bin.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(&new, &staged)
        .with_context(|| format!("could not write {}", staged.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, bin)
        .with_context(|| format!("could not replace {}", bin.display()))?;
    let _ = std::fs::remove_dir_all(work);
    restart()
        .context("binary replaced, but the daemon did not restart; run `rrq restart`")?;
    Ok(Installed::Replaced { sha })
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Output, RealRunner};
    use std::cell::{Cell, RefCell};

    const NEW: &str = "abcdef1234567890abcdef1234567890abcdef12";

    /// `gh` falso (sem rede): `--output -` devolve o version.txt e `--dir` copia os assets
    /// preparados. O resto (`shasum`, `tar`) roda de verdade.
    struct FakeGh {
        version: String,
        assets: PathBuf,
        downloads: RefCell<Vec<String>>,
    }

    impl Runner for FakeGh {
        fn run(&self, c: &Cmd) -> Result<Output> {
            if c.program != "gh" {
                return RealRunner.run(c);
            }
            let line = c.line();
            assert!(
                line.contains(&format!("--repo {REPO}")) && line.contains("release download main"),
                "{line}"
            );
            self.downloads.borrow_mut().push(line.clone());
            if line.contains("--output -") {
                return Ok(Output::ok(&format!("{}\n", self.version)));
            }
            let pos = c.args.iter().position(|a| a == "--dir").expect("--dir");
            let dir = PathBuf::from(&c.args[pos + 1]);
            for e in std::fs::read_dir(&self.assets)? {
                let e = e?;
                std::fs::copy(e.path(), dir.join(e.file_name()))?;
            }
            Ok(Output::ok(""))
        }
    }

    /// Tarball com um `rrq` que imprime `conteudo`, e o `.sha256` (certo ou não).
    fn assets(root: &Path, conteudo: &str, good_checksum: bool) -> PathBuf {
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("rrq"), format!("#!/bin/sh\necho {conteudo}\n")).unwrap();
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        let tarball = assets.join(format!("rrq-{}.tar.gz", target()));
        let st = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&src)
            .arg("rrq")
            .status()
            .unwrap();
        assert!(st.success());
        let sum = if good_checksum {
            sha256_of(&RealRunner, &tarball).unwrap()
        } else {
            "0".repeat(64)
        };
        std::fs::write(
            assets.join(format!("rrq-{}.tar.gz.sha256", target())),
            format!("{sum}  rrq-{}.tar.gz\n", target()),
        )
        .unwrap();
        assets
    }

    fn fake(root: &Path, version: &str, good: bool) -> FakeGh {
        FakeGh {
            version: version.into(),
            assets: assets(root, "nova", good),
            downloads: RefCell::new(vec![]),
        }
    }

    fn installed_bin(root: &Path) -> PathBuf {
        let bin = root.join("bin/rrq");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "velha").unwrap();
        bin
    }

    #[test]
    fn check_compares_with_the_local_sha() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(dir.path(), NEW, true);
        assert_eq!(check_against(&gh, NEW).unwrap(), None);
        assert_eq!(check_against(&gh, "1111111").unwrap().as_deref(), Some(NEW));
        let bad = FakeGh {
            version: "<html>".into(),
            ..fake(dir.path(), NEW, true)
        };
        assert!(check_against(&bad, NEW).is_err());
        assert!(!local_sha().is_empty());
    }

    #[test]
    fn same_sha_downloads_nothing_and_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(dir.path(), NEW, true);
        let bin = installed_bin(dir.path());
        let restarted = Cell::new(0);
        let r = install(&gh, NEW, &bin, &dir.path().join("work"), &|| {
            restarted.set(restarted.get() + 1);
            Ok(())
        });
        assert_eq!(r.unwrap(), Installed::UpToDate);
        assert_eq!(gh.downloads.borrow().len(), 1, "só o version.txt");
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "velha");
        assert_eq!(restarted.get(), 0);
    }

    #[test]
    fn wrong_checksum_aborts_without_touching_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(dir.path(), NEW, false);
        let bin = installed_bin(dir.path());
        let restarted = Cell::new(0);
        let err = install(&gh, "1111111", &bin, &dir.path().join("work"), &|| {
            restarted.set(restarted.get() + 1);
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "velha");
        assert!(!bin.with_extension("new").exists());
        assert_eq!(restarted.get(), 0);
    }

    #[test]
    fn success_replaces_the_binary_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(dir.path(), NEW, true);
        let bin = installed_bin(dir.path());
        let restarted = Cell::new(0);
        let r = install(&gh, "1111111", &bin, &dir.path().join("work"), &|| {
            restarted.set(restarted.get() + 1);
            Ok(())
        });
        assert_eq!(r.unwrap(), Installed::Replaced { sha: NEW.into() });
        assert!(std::fs::read_to_string(&bin).unwrap().contains("echo nova"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!bin.with_extension("new").exists() && !dir.path().join("work").exists());
        assert_eq!(restarted.get(), 1);
    }
}
