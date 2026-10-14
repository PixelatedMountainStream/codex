use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use crate::Homes;

const AGENTS_COPY_MARKER: &str = ".AGENTS.md.copy";

/// Create the codex-local home and its links to stock. Idempotent: existing entries
/// (links, files or dirs) in the local home are never replaced.
pub fn ensure_home(h: &Homes) -> Result<()> {
    fs::create_dir_all(&h.local).with_context(|| format!("create {}", h.local.display()))?;
    for dir in ["sessions", "archived_sessions"] {
        let target = h.stock.join(dir);
        let link = h.local.join(dir);
        if link.symlink_metadata().is_err() {
            // Stock directories must exist so the link does not dangle.
            fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
            link_dir(&target, &link)?;
        }
    }
    let agents = h.local.join("AGENTS.md");
    let stock_agents = h.stock.join("AGENTS.md");
    if agents.symlink_metadata().is_err() && stock_agents.is_file() {
        link_agents_md(&stock_agents, &agents, &h.local)?;
    }
    if !h.local.join("skills").exists() {
        refresh_skills(h)?;
    }
    Ok(())
}

/// Make `skills/` a real dir holding a link to each stock child except `.system`.
/// Also prunes links of ours whose stock target has vanished.
pub fn refresh_skills(h: &Homes) -> Result<()> {
    let local_skills = h.local.join("skills");
    let stock_skills = h.stock.join("skills");
    fs::create_dir_all(&local_skills)?;
    for entry in fs::read_dir(&local_skills)?.flatten() {
        let path = entry.path();
        let ours = fs::read_link(&path).is_ok_and(|t| t.starts_with(&stock_skills));
        if ours && !path.exists() {
            remove_link(&path)?;
        }
    }
    let Ok(children) = fs::read_dir(&stock_skills) else {
        return Ok(());
    };
    for child in children.flatten() {
        let name = child.file_name();
        if name == ".system" {
            continue;
        }
        let link = local_skills.join(&name);
        if link.symlink_metadata().is_ok() {
            continue;
        }
        let target = child.path();
        if target.is_dir() {
            link_dir(&target, &link)?;
        } else {
            link_file(&target, &link)?;
        }
    }
    Ok(())
}

/// Refresh a copied `AGENTS.md` (Windows fallback only; marked by a sidecar file).
pub(crate) fn refresh_agents_copy(h: &Homes) -> Result<()> {
    let agents = h.local.join("AGENTS.md");
    let stock = h.stock.join("AGENTS.md");
    if h.local.join(AGENTS_COPY_MARKER).exists() && agents.is_file() && stock.is_file() {
        fs::copy(&stock, &agents)?;
    }
    Ok(())
}

fn remove_link(p: &Path) -> Result<()> {
    fs::remove_file(p).or_else(|_| fs::remove_dir(p))?;
    Ok(())
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))
}

#[cfg(unix)]
fn link_file(target: &Path, link: &Path) -> Result<()> {
    link_dir(target, link)
}

#[cfg(unix)]
fn link_agents_md(target: &Path, link: &Path, _home: &Path) -> Result<()> {
    link_file(target, link)
}

/// Windows: directories are NTFS junctions (never a copy).
#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) -> Result<()> {
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .context("run `cmd /C mklink /J`")?;
    if !out.status.success() {
        anyhow::bail!(
            "could not create junction {} -> {}: {}\nHow to fix: ensure the codex-local home is on an NTFS volume and is writable.",
            link.display(),
            target.display(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn link_file(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link)
        .or_else(|_| fs::hard_link(target, link))
        .or_else(|_| fs::copy(target, link).map(|_| ()))
        .with_context(|| format!("link {} -> {}", link.display(), target.display()))
}

/// Windows `AGENTS.md`: file symlink, else hardlink, else a copy that `sync-config`
/// refreshes (marked by a sidecar file) and `verify_install` reports.
#[cfg(windows)]
fn link_agents_md(target: &Path, link: &Path, home: &Path) -> Result<()> {
    if std::os::windows::fs::symlink_file(target, link).is_ok()
        || fs::hard_link(target, link).is_ok()
    {
        return Ok(());
    }
    fs::copy(target, link)
        .with_context(|| format!("copy {} -> {}", target.display(), link.display()))?;
    fs::write(
        home.join(AGENTS_COPY_MARKER),
        "AGENTS.md is a copy; run `codex-local sync-config` to refresh\n",
    )?;
    Ok(())
}
