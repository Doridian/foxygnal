//! Loading block plugins at start-up (docs/PLUGINS.md).

use std::path::{Path, PathBuf};

use foxygnal_core::Registry;

/// Where to look: the `--plugins` directories if any were given (a missing
/// one is an error), else `<base>/plugins` and the user's data directory.
pub fn dirs(explicit: &[PathBuf], base: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if explicit.is_empty() {
        return Ok(default_dirs(base));
    }
    for d in explicit {
        if !d.is_dir() {
            anyhow::bail!("plugin dir {} does not exist", d.display());
        }
    }
    Ok(explicit.to_vec())
}

#[cfg(feature = "plugins")]
fn default_dirs(base: &Path) -> Vec<PathBuf> {
    foxygnal_plugin::default_dirs(base)
}

#[cfg(not(feature = "plugins"))]
fn default_dirs(_base: &Path) -> Vec<PathBuf> {
    Vec::new()
}

/// Load the plugins in `dirs` into `registry`. A plugin that fails to load
/// is logged and skipped.
#[cfg(feature = "plugins")]
pub fn load(registry: &mut Registry, dirs: &[PathBuf]) -> anyhow::Result<()> {
    if dirs.iter().all(|d| !d.is_dir()) {
        return Ok(());
    }
    let host = foxygnal_plugin::PluginHost::new(foxygnal_plugin::Options::default())?;
    host.load_dirs(dirs, registry);
    Ok(())
}

#[cfg(not(feature = "plugins"))]
pub fn load(_registry: &mut Registry, dirs: &[PathBuf]) -> anyhow::Result<()> {
    if !dirs.is_empty() {
        tracing::warn!("this build has no plugin support (cargo feature `plugins`)");
    }
    Ok(())
}
