use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::anyhow;

use crate::EnvFn;

pub struct Homes {
    /// codex-local's own home (`CODEX_LOCAL_HOME`, default `~/.codex/local`).
    pub local: PathBuf,
    /// Stock codex home: link target and copy source.
    pub stock: PathBuf,
}

/// The codex-local home never comes from an inherited `CODEX_HOME`. If the inherited
/// `CODEX_HOME` points at the codex-local home itself (nested run), stock falls back
/// to `~/.codex` so links never loop.
pub fn resolve_homes(env: EnvFn, user_home: Option<PathBuf>) -> Result<Homes> {
    let default_stock = || {
        user_home.clone().map(|h| h.join(".codex")).ok_or_else(|| {
            anyhow!("cannot determine the user home directory; set CODEX_LOCAL_HOME and CODEX_HOME")
        })
    };
    let local = match env("CODEX_LOCAL_HOME") {
        Some(p) => PathBuf::from(OsString::from(p)),
        None => default_stock()?.join("local"),
    };
    let stock = match env("CODEX_HOME") {
        Some(p) if PathBuf::from(&p) != local => PathBuf::from(p),
        _ => default_stock()?,
    };
    if stock == local {
        return Err(anyhow!(
            "CODEX_LOCAL_HOME must differ from the stock codex home ({})",
            stock.display()
        ));
    }
    Ok(Homes { local, stock })
}
