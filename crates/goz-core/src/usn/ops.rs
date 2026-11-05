//! Reason-bitmask normalization: raw USN records into idempotent index ops.
//!
//! Reason bits accumulate per open-close window: every sub-operation
//! appends a record whose `Reason` is the OR of all reasons so far since the
//! open, and the final record adds `USN_REASON_CLOSE`. The same logical
//! event therefore reappears in later records; consumers must apply ops
//! idempotently (upsert / remove-if-present), and all bit tests here are
//! bitwise `&`, never equality.

use smallvec::SmallVec;

use crate::types::Frn;

use super::record::{
    ParsedUsnRecord, USN_REASON_BASIC_INFO_CHANGE, USN_REASON_DATA_EXTEND,
    USN_REASON_DATA_OVERWRITE, USN_REASON_DATA_TRUNCATION, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
};

/// One normalized index operation derived from a USN record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsnOp {
    /// Create or update the entry for `frn`: the record's name and parent
    /// are the file's current state (emitted for `FILE_CREATE` and for
    /// `RENAME_NEW_NAME`, which is authoritative and self-sufficient).
    Upsert {
        frn: Frn,
        /// FRN of the directory that now contains the name.
        parent: Frn,
        /// Current file name in WTF-8.
        name: Vec<u8>,
        /// The name contained unpaired UTF-16 surrogates.
        name_lossy: bool,
        is_dir: bool,
    },
    /// The file record died (last hard link gone): remove every entry for
    /// `frn`.
    Delete { frn: Frn },
    /// A hard link was added or removed. The record may name a DEAD link, so
    /// the index must never upsert from it.
    ///
    /// Reconciling the file's actual link set needs an external link-set walk
    /// fed to [`crate::index::VolumeIndex::reconcile_links`]. The daemon does
    /// that walk (`goz_winfs::link_paths`) and applies the result, so names
    /// added or removed by a hard-link change are reconciled live. A walk that
    /// cannot complete is skipped and counted rather than applied.
    LinkDirty { frn: Frn },
    /// Size and/or timestamps may have changed; USN records carry neither,
    /// so the entry needs an external re-stat.
    StatDirty { frn: Frn },
    /// A `RENAME_OLD_NAME` record was seen without its NEW counterpart in
    /// the same record. Telemetry only; the index ignores it (the NEW
    /// record alone carries the committed state).
    RenameOldSeen { frn: Frn },
}

/// Reasons that add or remove a name in the record's folder. Each changes
/// the folder's date modified, and NTFS writes no record for the folder.
const NAME_CHANGES: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_HARD_LINK_CHANGE;

/// Normalizes one record's accumulated reason bitmask into index ops.
///
/// Decision table (rows evaluated top to bottom; rows 2-6 are cumulative,
/// each `&`-test independently appends its op):
///
/// | # | Condition (bitwise `&`)                                              | Effect                              |
/// |---|----------------------------------------------------------------------|-------------------------------------|
/// | 1 | `FILE_DELETE`                                                        | `[Delete, StatDirty(parent)]` and nothing else: delete wins over all accumulated bits |
/// | 2 | `RENAME_NEW_NAME` or `FILE_CREATE`                                   | push `Upsert` (record name/parent are current state; no OLD/NEW pairing state machine exists by design) |
/// | 3 | `RENAME_OLD_NAME` and none of `RENAME_NEW_NAME`/`FILE_CREATE`/`FILE_DELETE` | push `RenameOldSeen` (telemetry) |
/// | 4 | `HARD_LINK_CHANGE`                                                   | push `LinkDirty` (never upsert: the named link may be dead) |
/// | 5 | `FILE_CREATE`/`DATA_OVERWRITE`/`DATA_EXTEND`/`DATA_TRUNCATION`/`BASIC_INFO_CHANGE` | push `StatDirty` (a USN record carries no size/mtime, so a create needs one too) |
/// | 6 | `FILE_CREATE`/`FILE_DELETE`/`RENAME_OLD_NAME`/`RENAME_NEW_NAME`/`HARD_LINK_CHANGE` | push `StatDirty` for the record's parent folder, whose date the name change moved |
/// | 7 | none of the above (e.g. CLOSE-only, security-only)                   | empty |
///
/// A move is two records, the OLD one naming the old folder and the NEW one
/// the new folder, so both folders are re-read.
pub fn ops_for(rec: &ParsedUsnRecord) -> SmallVec<[UsnOp; 3]> {
    let mut ops = SmallVec::new();
    let reason = rec.reason;

    // Row 1: delete wins over everything accumulated in the window, but the
    // folder that lost the name still has a new date (row 6).
    if reason & USN_REASON_FILE_DELETE != 0 {
        ops.push(UsnOp::Delete { frn: rec.frn });
        ops.push(UsnOp::StatDirty {
            frn: rec.parent_frn,
        });
        return ops;
    }

    // Row 2: the record's name/parent are the file's current state.
    if reason & (USN_REASON_RENAME_NEW_NAME | USN_REASON_FILE_CREATE) != 0 {
        ops.push(UsnOp::Upsert {
            frn: rec.frn,
            parent: rec.parent_frn,
            name: rec.name.clone(),
            name_lossy: rec.name_lossy,
            is_dir: rec.is_dir(),
        });
    }

    // Row 3: OLD without NEW/CREATE/DELETE in the same record.
    if reason & USN_REASON_RENAME_OLD_NAME != 0
        && reason & (USN_REASON_RENAME_NEW_NAME | USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE)
            == 0
    {
        ops.push(UsnOp::RenameOldSeen { frn: rec.frn });
    }

    // Row 4.
    if reason & USN_REASON_HARD_LINK_CHANGE != 0 {
        ops.push(UsnOp::LinkDirty { frn: rec.frn });
    }

    // Row 5. FILE_CREATE is here as well as in row 2: a USN record carries no
    // size or timestamp, so a newly created file has no metadata anywhere until
    // something stats it. A file created and never written emits CREATE with no
    // DATA_* bit ever following, so without this its size stays unknown for the
    // life of the index, and the es CSV contract renders an empty Size column as
    // a folder: a real file reported to the consumer as a directory.
    if reason
        & (USN_REASON_FILE_CREATE
            | USN_REASON_DATA_OVERWRITE
            | USN_REASON_DATA_EXTEND
            | USN_REASON_DATA_TRUNCATION
            | USN_REASON_BASIC_INFO_CHANGE)
        != 0
    {
        ops.push(UsnOp::StatDirty { frn: rec.frn });
    }

    // Row 6.
    if reason & NAME_CHANGES != 0 {
        ops.push(UsnOp::StatDirty {
            frn: rec.parent_frn,
        });
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::record::{FILE_ATTRIBUTE_DIRECTORY, USN_REASON_CLOSE};

    /// Reason bit not in any of our decision-table sets
    /// (`USN_REASON_SECURITY_CHANGE`).
    const SECURITY_ONLY: u32 = 0x0000_0800;

    fn rec(reason: u32, attributes: u32) -> ParsedUsnRecord {
        ParsedUsnRecord {
            major_version: 2,
            frn: Frn(7),
            parent_frn: Frn(3),
            usn: 100,
            timestamp_ft: 0,
            reason,
            attributes,
            name: b"a.txt".to_vec(),
            name_lossy: false,
        }
    }

    /// The stat of the record's folder that every name change schedules.
    const FOLDER: UsnOp = UsnOp::StatDirty { frn: Frn(3) };

    fn upsert(is_dir: bool) -> UsnOp {
        UsnOp::Upsert {
            frn: Frn(7),
            parent: Frn(3),
            name: b"a.txt".to_vec(),
            name_lossy: false,
            is_dir,
        }
    }

    /// A create carries no size or mtime, so it must be stat-dirty as well as an
    /// upsert. Without the StatDirty a file created and never written keeps an
    /// unknown size forever, and the es CSV renders that as a folder.
    #[test]
    fn create_emits_upsert_and_stat() {
        let r = rec(USN_REASON_FILE_CREATE, 0x20);
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![upsert(false), UsnOp::StatDirty { frn: Frn(7) }, FOLDER]
        );
    }

    /// The regression that motivated the row-5 CREATE bit: a file created and
    /// never written emits CREATE (plus CLOSE) and no DATA_* bit will ever
    /// follow, so this record is the only chance to learn its size.
    #[test]
    fn created_but_never_written_file_is_still_stat_dirty() {
        let r = rec(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, 0x20);
        assert!(
            ops_for(&r)
                .iter()
                .any(|op| matches!(op, UsnOp::StatDirty { .. })),
            "a create with no DATA_* bit must still schedule a stat, or the size stays unknown"
        );
    }

    #[test]
    fn rename_new_emits_upsert() {
        let r = rec(USN_REASON_RENAME_NEW_NAME, 0x20);
        assert_eq!(ops_for(&r).into_vec(), vec![upsert(false), FOLDER]);
    }

    #[test]
    fn upsert_carries_is_dir_from_attributes() {
        let r = rec(USN_REASON_FILE_CREATE, FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![upsert(true), UsnOp::StatDirty { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn accumulated_create_data_close_emits_upsert_and_stat() {
        let r = rec(
            USN_REASON_FILE_CREATE | USN_REASON_DATA_EXTEND | USN_REASON_CLOSE,
            0x20,
        );
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![upsert(false), UsnOp::StatDirty { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn rename_old_alone_is_telemetry_only() {
        let r = rec(USN_REASON_RENAME_OLD_NAME, 0x20);
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![UsnOp::RenameOldSeen { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn rename_old_and_new_in_one_record_emits_upsert_only() {
        let r = rec(
            USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME,
            0x20,
        );
        assert_eq!(ops_for(&r).into_vec(), vec![upsert(false), FOLDER]);
    }

    #[test]
    fn delete_wins_over_all_accumulated_bits() {
        let r = rec(
            USN_REASON_FILE_DELETE
                | USN_REASON_FILE_CREATE
                | USN_REASON_RENAME_OLD_NAME
                | USN_REASON_HARD_LINK_CHANGE
                | USN_REASON_DATA_EXTEND
                | USN_REASON_CLOSE,
            0x20,
        );
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![UsnOp::Delete { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn hard_link_change_emits_link_dirty_never_upsert() {
        let r = rec(USN_REASON_HARD_LINK_CHANGE | USN_REASON_CLOSE, 0x20);
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![UsnOp::LinkDirty { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn each_stat_reason_emits_stat_dirty() {
        for reason in [
            USN_REASON_DATA_OVERWRITE,
            USN_REASON_DATA_EXTEND,
            USN_REASON_DATA_TRUNCATION,
            USN_REASON_BASIC_INFO_CHANGE,
        ] {
            let r = rec(reason, 0x20);
            assert_eq!(
                ops_for(&r).into_vec(),
                vec![UsnOp::StatDirty { frn: Frn(7) }],
                "reason {reason:#x}"
            );
        }
    }

    #[test]
    fn close_only_emits_nothing() {
        let r = rec(USN_REASON_CLOSE, 0x20);
        assert!(ops_for(&r).is_empty());
    }

    #[test]
    fn security_only_emits_nothing() {
        let r = rec(SECURITY_ONLY, 0x20);
        assert!(ops_for(&r).is_empty());
    }

    #[test]
    fn unrelated_bits_do_not_mask_matches() {
        // & semantics: extra accumulated bits never turn a match off.
        let r = rec(
            USN_REASON_FILE_CREATE | SECURITY_ONLY | USN_REASON_CLOSE,
            0x20,
        );
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![upsert(false), UsnOp::StatDirty { frn: Frn(7) }, FOLDER]
        );
    }

    #[test]
    fn rename_old_plus_hard_link_emits_both() {
        let r = rec(
            USN_REASON_RENAME_OLD_NAME | USN_REASON_HARD_LINK_CHANGE,
            0x20,
        );
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![
                UsnOp::RenameOldSeen { frn: Frn(7) },
                UsnOp::LinkDirty { frn: Frn(7) },
                FOLDER,
            ]
        );
    }

    #[test]
    fn full_accumulation_without_delete_emits_all_applicable_ops() {
        let r = rec(
            USN_REASON_RENAME_NEW_NAME
                | USN_REASON_HARD_LINK_CHANGE
                | USN_REASON_BASIC_INFO_CHANGE
                | USN_REASON_CLOSE,
            0x20,
        );
        assert_eq!(
            ops_for(&r).into_vec(),
            vec![
                upsert(false),
                UsnOp::LinkDirty { frn: Frn(7) },
                UsnOp::StatDirty { frn: Frn(7) },
                FOLDER,
            ]
        );
    }

    /// Adding or removing a name moves its folder's date modified, and NTFS
    /// logs no record for the folder, so each name change re-reads it. A
    /// change to a file's data or attributes leaves the folder alone.
    #[test]
    fn a_name_change_re_reads_its_folder() {
        for reason in [
            USN_REASON_FILE_CREATE,
            USN_REASON_FILE_DELETE,
            USN_REASON_RENAME_OLD_NAME,
            USN_REASON_RENAME_NEW_NAME,
            USN_REASON_HARD_LINK_CHANGE,
        ] {
            let ops = ops_for(&rec(reason | USN_REASON_CLOSE, 0x20));
            assert!(ops.contains(&FOLDER), "reason {reason:#x} gave {ops:?}");
        }
        for reason in [
            USN_REASON_DATA_OVERWRITE,
            USN_REASON_DATA_EXTEND,
            USN_REASON_DATA_TRUNCATION,
            USN_REASON_BASIC_INFO_CHANGE,
            USN_REASON_CLOSE,
        ] {
            let ops = ops_for(&rec(reason, 0x20));
            assert!(!ops.contains(&FOLDER), "reason {reason:#x} gave {ops:?}");
        }
    }
}
