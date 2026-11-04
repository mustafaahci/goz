//! The text `--status` prints, built from the daemon's reply.

use goz_core::proto::{DaemonStatus, MemPair, VolumeStatus};
use std::fmt::Write;

/// The `--status` report: a line per volume, its health notes below it, and
/// the daemon's own memory.
pub(crate) fn render(s: &DaemonStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "gozd status: {} volume(s)", s.volumes.len());
    for v in &s.volumes {
        volume(&mut out, v);
    }
    if s.process_private_bytes > 0 {
        let _ = writeln!(
            out,
            "  daemon: {:.1} MB private, {:.1} MB working set",
            s.process_private_bytes as f64 / 1e6,
            s.process_working_set as f64 / 1e6,
        );
    }
    out
}

fn volume(out: &mut String, v: &VolumeStatus) {
    let mount = v.mounts.first().cloned().unwrap_or_else(|| v.guid.clone());
    let _ = writeln!(
        out,
        "  {mount:<8} {:>12} entries  phase={}{}",
        v.entries,
        v.phase, // user-facing Display, not Debug
        if v.metadata_pending {
            "  (metadata pending)"
        } else {
            ""
        }
    );
    // The journal's size is the replay horizon: everything older than what
    // it holds costs a full rescan on restart.
    if let Some(bytes) = v.journal_max_bytes {
        let _ = writeln!(out, "           journal max {} MiB", bytes / (1024 * 1024));
    }
    // Orphans and a parent cycle make every query walk each candidate's
    // parent chain; a healthy volume has neither, so say when it does.
    if v.has_orphans {
        let _ = writeln!(
            out,
            "           orphans: entries without a full path (a parent never arrived, or its folder was removed first) sit under lost+found; queries pay a presentability walk"
        );
    }
    if v.has_parent_cycle {
        let _ = writeln!(
            out,
            "           parent cycle: entries without a full path (a record read while folders moved, or a missed one) sit in a loop of folders until a journal record breaks it; queries pay a presentability walk"
        );
    }
    if v.names_waiting > 0 {
        let _ = writeln!(
            out,
            "           {} name(s) waiting to join the sort order",
            v.names_waiting
        );
    }
    // Drift counters, shown only when non-zero. These are the only outward
    // sign that the index and the volume have diverged, so reporting them
    // nowhere made a drifting index indistinguishable from a healthy one.
    if v.link_reconciles_dropped > 0 {
        let _ = writeln!(
            out,
            "           {} hard-link change(s) not reconciled: some names may be stale",
            v.link_reconciles_dropped
        );
    }
    if v.stale_slots > 0 || v.delete_of_unknown > 0 || v.placeholders_created > 0 {
        let _ = writeln!(
            out,
            "           drift: {} stale slot(s), {} unknown delete(s), {} placeholder(s)",
            v.stale_slots, v.delete_of_unknown, v.placeholders_created
        );
    }
    // Index memory, component by component. `alloc` differing from `used` is
    // capacity slack held by that component.
    if let Some(m) = &v.memory {
        let mb = |p: &MemPair| format!("{:.1}/{:.1}", p.used as f64 / 1e6, p.alloc as f64 / 1e6);
        let total_used = m.entries.used
            + m.arena_raw.used
            + m.arena_folded.used
            + m.frn_map.used
            + m.name_tables.used
            + m.dir_children.used;
        let total_alloc = m.entries.alloc
            + m.arena_raw.alloc
            + m.arena_folded.alloc
            + m.frn_map.alloc
            + m.name_tables.alloc
            + m.dir_children.alloc;
        let _ = writeln!(
            out,
            "           mem MB used/alloc: total {:.1}/{:.1}  entries {}  names {}  folded {}  frn[{}] {}  ntables {}  dirs {}",
            total_used as f64 / 1e6,
            total_alloc as f64 / 1e6,
            mb(&m.entries),
            mb(&m.arena_raw),
            mb(&m.arena_folded),
            m.frn_map_kind,
            mb(&m.frn_map),
            mb(&m.name_tables),
            mb(&m.dir_children),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goz_core::types::VolumePhase;

    fn healthy() -> VolumeStatus {
        VolumeStatus {
            guid: r"\\?\Volume{0}\".into(),
            mounts: vec![r"C:\".into()],
            phase: VolumePhase::Live,
            entries: 1234,
            generation: 0,
            metadata_pending: false,
            journal_max_bytes: None,
            has_orphans: false,
            has_parent_cycle: false,
            names_waiting: 0,
            placeholders_created: 0,
            delete_of_unknown: 0,
            stale_slots: 0,
            link_reconciles_dropped: 0,
            memory: None,
        }
    }

    fn report(volume: VolumeStatus) -> String {
        render(&DaemonStatus {
            volumes: vec![volume],
            volumes_incomplete: false,
            process_working_set: 0,
            process_private_bytes: 0,
        })
    }

    const ORPHANS: &str = "           orphans: entries without a full path (a parent never arrived, or its folder was removed first) sit under lost+found; queries pay a presentability walk\n";
    const CYCLE: &str = "           parent cycle: entries without a full path (a record read while folders moved, or a missed one) sit in a loop of folders until a journal record breaks it; queries pay a presentability walk\n";

    /// A healthy volume is one line, as it was before the report moved here.
    #[test]
    fn a_healthy_volume_is_one_line() {
        let phase = VolumePhase::Live.to_string();
        let line = format!("  {:<8} {:>12} entries  phase={phase}\n", r"C:\", 1234);
        assert_eq!(
            report(healthy()),
            format!("gozd status: 1 volume(s)\n{line}")
        );
    }

    /// A parent cycle gets its own line, apart from orphans, below the
    /// volume's: either one sends every query down the slow path.
    #[test]
    fn a_parent_cycle_gets_its_own_line() {
        let cycle = report(VolumeStatus {
            has_parent_cycle: true,
            ..healthy()
        });
        assert!(cycle.ends_with(CYCLE), "{cycle}");
        assert!(!cycle.contains("orphans:"), "{cycle}");

        let both = report(VolumeStatus {
            has_orphans: true,
            has_parent_cycle: true,
            ..healthy()
        });
        assert!(both.ends_with(&format!("{ORPHANS}{CYCLE}")), "{both}");
        assert!(!report(healthy()).contains("parent cycle"));
    }
}
