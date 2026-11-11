//! gozd's index options (Everything's Options > Indexes): kept in memory,
//! read at start, and written by gozd itself on each change, beside its log
//! in `%SystemRoot%\Logs\goz`, as `gozd.ini`: everyone may read it, and only
//! administrators and SYSTEM may change it (see
//! [`goz_winfs::write_admin_only`]). That folder, unlike `%ProgramData%`,
//! is one an ordinary user cannot create first and point elsewhere.
//!
//! The file is Everything's `key=0|1` lines under its key names. A key
//! missing, unknown or malformed keeps Everything's default.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use goz_core::index::Columns;

use goz_core::proto::{CONFIG_KEYS, ConfigChange, IndexConfig};
use parking_lot::RwLock;

/// The options, and the file they persist in.
pub(crate) struct ConfigStore {
    path: Option<PathBuf>,
    current: RwLock<IndexConfig>,
}

impl ConfigStore {
    /// The options in `path`, Everything's defaults where it says nothing or
    /// does not exist; `None` keeps them in memory alone.
    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let current = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map_or_else(IndexConfig::default, |text| parse(&text));
        Self {
            path,
            current: RwLock::new(current),
        }
    }

    pub(crate) fn get(&self) -> IndexConfig {
        *self.current.read()
    }

    /// Applies `change`, writes the file, and returns the options as they
    /// now are. The options change in memory only once the file holds them.
    pub(crate) fn set(&self, change: &ConfigChange) -> Result<IndexConfig, String> {
        let mut current = self.current.write();
        let next = current.with(change);
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("creating {}: {e}", dir.display()))?;
            }
            goz_winfs::write_admin_only(path, render(&next).as_bytes())
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
        }
        *current = next;
        Ok(next)
    }
}

/// `%SystemRoot%\Logs\goz\gozd.ini`, beside the service's log.
pub(crate) fn default_path() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    Some(Path::new(&root).join("Logs").join("goz").join("gozd.ini"))
}

static STORE: OnceLock<ConfigStore> = OnceLock::new();

/// Loads the daemon's options from [`default_path`], once.
pub(crate) fn init() -> &'static ConfigStore {
    STORE.get_or_init(|| ConfigStore::load(default_path()))
}

/// The daemon's options: [`init`]'s, or Everything's defaults in a process
/// that never loaded them (a test, `gozd scan`).
pub(crate) fn current() -> IndexConfig {
    STORE
        .get()
        .map_or_else(IndexConfig::default, ConfigStore::get)
}

/// The optional columns the options switch on.
pub(crate) fn wanted_columns() -> Columns {
    Columns::of_config(&current())
}

/// Reads Everything's `key=value` lines; a value is on unless it is `0`.
fn parse(text: &str) -> IndexConfig {
    let mut config = IndexConfig::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if let (Some(slot), true) = (config.slot(key.trim()), matches!(value, "0" | "1")) {
            *slot = value == "1";
        }
    }
    config
}

fn render(config: &IndexConfig) -> String {
    let mut out = String::from(
        "; gozd's index options, under Everything 1.5's names. Change them with\r\n\
         ; `goz --config set key=0|1` from an elevated prompt.\r\n[gozd]\r\n",
    );
    for key in CONFIG_KEYS {
        let on = config.get(key).unwrap_or(false);
        out.push_str(&format!("{key}={}\r\n", u8::from(on)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("gozd-config-{}", std::process::id()));
        let path = dir.join("gozd.ini");
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::load(Some(path.clone()));
        assert_eq!(
            store.get(),
            IndexConfig::default(),
            "no file: Everything's defaults"
        );
        let change = ConfigChange {
            index_attributes: Some(true),
            index_folder_size: Some(false),
            ..ConfigChange::default()
        };
        let now = store.set(&change).expect("written");
        assert!(now.index_attributes && !now.index_folder_size);
        let again = ConfigStore::load(Some(path.clone()));
        assert_eq!(again.get(), now);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("index_attributes=1\r\n"), "{text}");
        assert!(text.contains("index_folder_size=0\r\n"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_line_keeps_the_default() {
        let c = parse("index_date_created=1\nfast_size_sort=0\nindex_attributes=yes\nnoise\nfoo=1");
        assert!(c.index_date_created && !c.fast_size_sort);
        assert!(!c.index_attributes);
        assert!(c.index_folder_size);
        assert_eq!(parse(&render(&c)), c);
    }

    #[test]
    fn a_store_in_memory_alone_changes() {
        let store = ConfigStore::load(None);
        let now = store
            .set(&ConfigChange {
                index_recent_changes: Some(true),
                ..ConfigChange::default()
            })
            .expect("set");
        assert!(now.index_recent_changes);
        assert_eq!(Columns::of_config(&now).iter().count(), 1);
    }
}
