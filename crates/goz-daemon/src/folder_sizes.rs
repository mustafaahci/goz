//! Folder sizes, Everything's `index_folder_size`: on by default, as in
//! Everything 1.5. While on, each folder's size is the total of every file
//! below it (see [`goz_core::index::VolumeIndex::folder_sizes`]); they cost no
//! memory, since every folder already has a size slot.
//!
//! The one switch is [`set`]; whoever reads the setting (the daemon's config)
//! calls it. A build sums a volume's folders when it finishes, unless its
//! FILE_LAYOUT pass stopped and left sizes unread: then the volume's folder
//! sizes stay unknown until a build reads them all, rather than totals that
//! leave files out.

use std::sync::atomic::{AtomicBool, Ordering};

use goz_core::index::VolumeIndex;
use parking_lot::RwLockUpgradableReadGuard;

use crate::volume_state::{VolumeSet, VolumeState};

/// Whether folder sizes are wanted. Everything's default is on.
static WANTED: AtomicBool = AtomicBool::new(true);

/// Whether folder sizes are wanted now.
pub(crate) fn wanted() -> bool {
    WANTED.load(Ordering::Acquire)
}

/// Turns folder sizes on or off for every volume. On sums each volume's
/// folders off its write lock; off makes every folder's size unknown again.
/// Builds that finish later follow the new setting.
#[allow(dead_code)] // the config that calls it is wired apart (M7b)
pub(crate) fn set(volumes: &VolumeSet, on: bool) {
    WANTED.store(on, Ordering::Release);
    for volume in volumes.iter() {
        sync(volume);
    }
}

/// Whether a freshly built index should have its folders summed: they are
/// wanted, and the build read every size.
pub(crate) fn wanted_for_build(metadata_pending: bool) -> bool {
    wanted() && !metadata_pending
}

/// Brings one volume's index in line with the setting: sums its folders
/// when they are wanted and every size was read, or makes them unknown. The
/// sum runs under an upgradable read, so queries keep reading while it
/// walks the tree; only writing the totals takes the write lock. The tail
/// also writes under an upgradable read or the write lock, so nothing
/// changes the index between the sum and the install.
pub(crate) fn sync(volume: &VolumeState) {
    let want = wanted_for_build(*volume.metadata_pending.read());
    let guard = volume.index.upgradable_read();
    if guard.folder_sizes() == want {
        return;
    }
    if want {
        let plan = guard.plan_folder_sizes();
        let mut index = RwLockUpgradableReadGuard::upgrade(guard);
        let summed = index.install_folder_sizes(plan);
        debug_assert!(summed, "the index changed under an upgradable read");
    } else {
        RwLockUpgradableReadGuard::upgrade(guard).clear_folder_sizes();
    }
}

/// Sums a freshly built index's folders if the setting wants them, before
/// the index is published.
pub(crate) fn on_build(index: &mut VolumeIndex, metadata_pending: bool) {
    if wanted_for_build(metadata_pending) {
        index.set_folder_sizes(true);
    }
}

/// Whether folder sizes are on and settled: wanted, and every volume's index
/// holds its folders' totals (none still building, none with sizes left
/// unread). `Hello.features` lists `"folder_sizes"` while this holds.
pub(crate) fn settled(volumes: &VolumeSet) -> bool {
    wanted() && volumes.iter().all(|v| v.index.read().folder_sizes())
}
