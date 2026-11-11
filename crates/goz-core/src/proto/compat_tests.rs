//! Wave 9's protocol against itself and against goz v0.6.0's: every new
//! frame, field, sort key and `iflags` bit round-trips; an old client reads
//! what a new daemon sends it; a new client reads an old daemon, and what it
//! would send one an old daemon either reads as before or refuses as a
//! malformed request, never misreads. [`v060`] is v0.6.0's payload types and
//! page codec as tagged `v0.6.0` has them, doc comments dropped.

use super::*;
use crate::types::{SortDir, SortKey, SortSpec, VolumePhase};

/// goz v0.6.0's protocol, verbatim but for its doc comments.
mod v060 {
    use crate::types::VolumePhase;
    use crate::wtf8;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) enum SortKey {
        Name,
        Path,
        Size,
        DateModified,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) enum SortDir {
        Asc,
        Desc,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct SortSpec {
        pub(super) key: SortKey,
        pub(super) dir: SortDir,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "t")]
    pub(super) enum Request {
        Hello {
            proto_min: u16,
            proto_max: u16,
            client: String,
        },
        Query(QueryRequest),
        Status,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub(super) struct QueryRequest {
        pub(super) query: String,
        pub(super) scope: Option<String>,
        pub(super) sort: SortSpec,
        pub(super) offset: u32,
        pub(super) limit: Option<u32>,
        pub(super) want_size: bool,
        pub(super) want_mtime: bool,
        pub(super) match_case: bool,
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        pub(super) want_total_size: bool,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "t")]
    pub(super) enum Response {
        Hello {
            proto: u16,
            server: String,
            ready: bool,
        },
        Results(QueryResults),
        Status(DaemonStatus),
        Error {
            code: ProtoError,
            message: String,
        },
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub(super) struct QueryResults {
        pub(super) total: u64,
        pub(super) items: Vec<ResultItem>,
        pub(super) more: bool,
        pub(super) volumes_incomplete: bool,
        pub(super) metadata_pending: bool,
        pub(super) generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub(super) total_size: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub(super) struct ResultItem {
        pub(super) path: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        pub path_u16: Option<Vec<u16>>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        pub(super) size: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        pub(super) mtime_ft: Option<i64>,
        pub(super) is_dir: bool,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub(super) struct DaemonStatus {
        pub(super) volumes: Vec<VolumeStatus>,
        pub(super) volumes_incomplete: bool,
        #[serde(default)]
        pub(super) process_working_set: u64,
        #[serde(default)]
        pub(super) process_private_bytes: u64,
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub(super) struct MemPair {
        pub(super) used: u64,
        pub(super) alloc: u64,
    }

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub(super) struct VolumeMemory {
        pub(super) entries: MemPair,
        pub(super) arena_raw: MemPair,
        pub(super) arena_folded: MemPair,
        pub(super) frn_map: MemPair,
        pub(super) name_tables: MemPair,
        pub(super) dir_children: MemPair,
        pub(super) frn_map_kind: String,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub(super) struct VolumeStatus {
        pub(super) guid: String,
        pub(super) mounts: Vec<String>,
        pub(super) phase: VolumePhase,
        pub(super) entries: u64,
        pub(super) generation: u64,
        pub(super) metadata_pending: bool,
        #[serde(default)]
        pub(super) journal_max_bytes: Option<u64>,
        #[serde(default)]
        pub(super) has_orphans: bool,
        #[serde(default)]
        pub(super) has_parent_cycle: bool,
        #[serde(default)]
        pub(super) names_waiting: u64,
        #[serde(default)]
        pub(super) placeholders_created: u64,
        #[serde(default)]
        pub(super) delete_of_unknown: u64,
        #[serde(default)]
        pub(super) stale_slots: u64,
        #[serde(default)]
        pub(super) link_reconciles_dropped: u64,
        #[serde(default)]
        pub(super) memory: Option<VolumeMemory>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) enum ProtoError {
        BadQuery,
        Internal,
        TooLarge,
        Unsupported,
    }

    const IFLAG_IS_DIR: u8 = 1;
    const IFLAG_HAS_SIZE: u8 = 1 << 1;
    const IFLAG_HAS_MTIME: u8 = 1 << 2;
    const FLAG_MORE: u8 = 1;
    const FLAG_VOLUMES_INCOMPLETE: u8 = 1 << 1;
    const FLAG_METADATA_PENDING: u8 = 1 << 2;
    const FLAG_TOTAL_SIZE: u8 = 1 << 3;

    pub(super) fn push_item(
        out: &mut Vec<u8>,
        raw_path: &[u8],
        is_dir: bool,
        size: Option<u64>,
        mtime: Option<i64>,
    ) {
        out.extend_from_slice(&(raw_path.len() as u32).to_le_bytes());
        out.extend_from_slice(raw_path);
        let mut iflags = 0u8;
        if is_dir {
            iflags |= IFLAG_IS_DIR;
        }
        if size.is_some() {
            iflags |= IFLAG_HAS_SIZE;
        }
        if mtime.is_some() {
            iflags |= IFLAG_HAS_MTIME;
        }
        out.push(iflags);
        if let Some(s) = size {
            out.extend_from_slice(&s.to_le_bytes());
        }
        if let Some(m) = mtime {
            out.extend_from_slice(&m.to_le_bytes());
        }
    }

    struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        fn bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
            let end = self.pos.checked_add(n).ok_or("overflow")?;
            let slice = self.buf.get(self.pos..end).ok_or("truncated")?;
            self.pos = end;
            Ok(slice)
        }
        fn u8(&mut self) -> Result<u8, &'static str> {
            Ok(self.bytes(1)?[0])
        }
        fn u32(&mut self) -> Result<u32, &'static str> {
            Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64, &'static str> {
            Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
        }
        fn i64(&mut self) -> Result<i64, &'static str> {
            Ok(i64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
        }
    }

    /// v0.6.0's page decoder; it reads bits 0-2 of `iflags` and no other,
    /// and returns how many bytes it read.
    pub(super) fn decode_results_payload(
        body: &[u8],
    ) -> Result<(QueryResults, usize), &'static str> {
        let mut r = Reader { buf: body, pos: 0 };
        let total = r.u64()?;
        let generation = r.u64()?;
        let flags = r.u8()?;
        let count = r.u32()? as usize;
        let total_size = (flags & FLAG_TOTAL_SIZE != 0)
            .then(|| r.u64())
            .transpose()?;
        let mut items = Vec::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            let plen = r.u32()? as usize;
            let path = r.bytes(plen)?;
            let iflags = r.u8()?;
            let size = (iflags & IFLAG_HAS_SIZE != 0)
                .then(|| r.u64())
                .transpose()?;
            let mtime = (iflags & IFLAG_HAS_MTIME != 0)
                .then(|| r.i64())
                .transpose()?;
            let lossy = core::str::from_utf8(path).is_err();
            items.push(ResultItem {
                path: wtf8::to_string_lossy(path),
                path_u16: lossy.then(|| wtf8::to_utf16(path)),
                size,
                mtime_ft: mtime,
                is_dir: iflags & IFLAG_IS_DIR != 0,
            });
        }
        Ok((
            QueryResults {
                total,
                items,
                more: flags & FLAG_MORE != 0,
                volumes_incomplete: flags & FLAG_VOLUMES_INCOMPLETE != 0,
                metadata_pending: flags & FLAG_METADATA_PENDING != 0,
                generation,
                total_size,
            },
            r.pos,
        ))
    }
}

fn rt_request(req: &Request) -> String {
    let json = serde_json::to_string(req).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, req, "json: {json}");
    json
}

fn rt_response(resp: &Response) -> String {
    let json = serde_json::to_string(resp).expect("serialize");
    let back: Response = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, resp, "json: {json}");
    json
}

/// Every wave 9 sort key.
const NEW_KEYS: [SortKey; 5] = [
    SortKey::Extension,
    SortKey::DateCreated,
    SortKey::DateAccessed,
    SortKey::Attributes,
    SortKey::DateRecentlyChanged,
];

fn query(sort: SortKey) -> QueryRequest {
    QueryRequest {
        query: "ext:log".into(),
        scope: Some(r"C:\logs".into()),
        sort: SortSpec {
            key: sort,
            dir: SortDir::Desc,
        },
        offset: 4,
        limit: Some(100),
        want_size: true,
        want_mtime: true,
        match_case: false,
        ..QueryRequest::default()
    }
}

fn status(columns: Vec<ColumnStatus>) -> DaemonStatus {
    DaemonStatus {
        volumes: vec![VolumeStatus {
            guid: r"\\?\Volume{1}\".into(),
            mounts: vec![r"C:\".into()],
            phase: VolumePhase::Live,
            entries: 5,
            generation: 9,
            metadata_pending: false,
            journal_max_bytes: Some(1 << 26),
            has_orphans: false,
            has_parent_cycle: false,
            names_waiting: 0,
            placeholders_created: 0,
            delete_of_unknown: 0,
            stale_slots: 0,
            link_reconciles_dropped: 0,
            memory: Some(VolumeMemory::default()),
            columns,
        }],
        volumes_incomplete: false,
        process_working_set: 1,
        process_private_bytes: 2,
    }
}

/// Every request, response and query field wave 9 adds survives a round
/// trip, under the names and shapes the spec gives them.
#[test]
fn every_new_frame_round_trips() {
    // Requests: Watch, GetConfig, SetConfig, and a query with every new flag
    // and every new sort key.
    let json = rt_request(&Request::Watch {
        volumes: Some(vec![r"C:\".into(), r"\\?\Volume{1}\".into()]),
        min_interval_ms: 250,
    });
    assert_eq!(
        json,
        r#"{"t":"Watch","volumes":["C:\\","\\\\?\\Volume{1}\\"],"min_interval_ms":250}"#
    );
    rt_request(&Request::Watch {
        volumes: None,
        min_interval_ms: 0,
    });
    assert_eq!(rt_request(&Request::GetConfig), r#"{"t":"GetConfig"}"#);
    let mut every = ConfigChange::default();
    for key in CONFIG_KEYS {
        *every.slot(key).expect("a slot per key") = Some(true);
    }
    let json = rt_request(&Request::SetConfig(every));
    for key in CONFIG_KEYS {
        assert!(json.contains(&format!(r#""{key}":true"#)), "{json}");
    }
    for key in NEW_KEYS {
        let mut q = query(key);
        q.want_ctime = true;
        q.want_atime = true;
        q.want_attrib = true;
        q.want_rtime = true;
        let json = rt_request(&Request::Query(q));
        for flag in ["want_ctime", "want_atime", "want_attrib", "want_rtime"] {
            assert!(json.contains(&format!(r#""{flag}":true"#)), "{json}");
        }
    }

    // Responses: Hello with every feature, Watching, Changed, Config, a
    // Status with columns, and items with every new column.
    let features: Vec<String> = [
        feature::WATCH,
        feature::SORT_EXTENSION,
        feature::SORT_DATE_CREATED,
        feature::SORT_DATE_ACCESSED,
        feature::SORT_ATTRIBUTES,
        feature::SORT_DATE_RECENTLY_CHANGED,
        feature::COLUMNS,
        feature::FOLDER_SIZES,
        feature::INDEXED_DATE_CREATED,
        feature::INDEXED_DATE_ACCESSED,
        feature::INDEXED_ATTRIBUTES,
        feature::INDEXED_RECENT_CHANGES,
        feature::CONFIG,
    ]
    .map(String::from)
    .to_vec();
    let json = rt_response(&Response::Hello {
        proto: PROTO_VERSION,
        server: "gozd".into(),
        ready: true,
        features,
    });
    assert!(
        json.ends_with(r#""config"]}"#),
        "features is the last field: {json}"
    );
    assert_eq!(rt_response(&Response::Watching), r#"{"t":"Watching"}"#);
    let json = rt_response(&Response::Changed {
        volumes: vec![
            VolumeGen {
                mount: r"C:\".into(),
                generation: 42,
            },
            VolumeGen {
                mount: r"D:\".into(),
                generation: u64::MAX,
            },
        ],
    });
    assert!(
        json.starts_with(r#"{"t":"Changed","volumes":[{"mount":"C:\\","generation":42}"#),
        "{json}"
    );
    let config = IndexConfig {
        index_attributes: true,
        ..IndexConfig::default()
    };
    rt_response(&Response::Config(config));
    rt_response(&Response::Status(status(vec![ColumnStatus {
        column: "index_date_created".into(),
        settled: false,
        built: 3,
        of: 5,
        bytes: 40,
    }])));
    rt_response(&Response::Results(QueryResults {
        total: 1,
        items: vec![ResultItem {
            path: r"C:\a".into(),
            ctime_ft: Some(1),
            atime_ft: Some(2),
            rtime_ft: Some(3),
            attrib: Some(0x20),
            ..ResultItem::default()
        }],
        more: false,
        volumes_incomplete: false,
        metadata_pending: false,
        generation: 0,
        total_size: None,
    }));
}

/// Every one of the 16 combinations of `iflags` bits 3-6 round-trips
/// through the binary page, with every older bit and value beside it; the
/// values follow the old fields in bit order, each only when present.
#[test]
fn every_iflags_bit_round_trips() {
    for mask in 0u8..16 {
        for (is_dir, size, mtime) in [(false, Some(7u64), Some(-8i64)), (true, None, None)] {
            let cols = ItemColumns {
                ctime: (mask & 1 != 0).then_some(i64::MIN + 1),
                rtime: (mask & 2 != 0).then_some(133_000_000_000_000_000),
                attrib: (mask & 4 != 0).then_some(0x0040_0027),
                atime: (mask & 8 != 0).then_some(i64::MAX),
            };
            let item = ResultItem {
                path: "C:\\d\u{e9}j\u{e0}".into(),
                size,
                mtime_ft: mtime,
                is_dir,
                ctime_ft: cols.ctime,
                atime_ft: cols.atime,
                rtime_ft: cols.rtime,
                attrib: cols.attrib,
                ..ResultItem::default()
            };
            assert_eq!(ItemColumns::of(&item), cols);
            let mut bytes = Vec::new();
            push_item_with(&mut bytes, item.path.as_bytes(), is_dir, size, mtime, &cols);
            let flags_at = 4 + item.path.len();
            assert_eq!(
                bytes[flags_at] >> 3,
                mask,
                "bits 3-6 are ctime, rtime, attrib, atime"
            );
            let values = [
                size.map(|s| s.to_le_bytes().to_vec()),
                mtime.map(|m| m.to_le_bytes().to_vec()),
                cols.ctime.map(|v| v.to_le_bytes().to_vec()),
                cols.rtime.map(|v| v.to_le_bytes().to_vec()),
                cols.attrib.map(|v| v.to_le_bytes().to_vec()),
                cols.atime.map(|v| v.to_le_bytes().to_vec()),
            ];
            let tail: Vec<u8> = values.into_iter().flatten().flatten().collect();
            assert_eq!(&bytes[flags_at + 1..], &tail[..], "mask {mask:#06b}");

            let page = QueryResults {
                total: 2,
                items: vec![item.clone(), item],
                more: true,
                generation: 3,
                total_size: Some(14),
                volumes_incomplete: false,
                metadata_pending: false,
            };
            let mut payload = Vec::new();
            encode_results_payload(&page, &mut payload);
            assert_eq!(payload[0], RESP_TAG_RESULTS);
            assert_eq!(decode_results_payload(&payload[1..]).expect("decode"), page);
        }
    }
}

/// An old client against a new daemon: everything it sends reads as before,
/// and everything the daemon sends it reads, its page bytes unchanged
/// because it never asks for a column.
#[test]
fn an_old_client_reads_a_new_daemon() {
    // What v0.6.0 sends reads, with every new field off.
    for key in [
        v060::SortKey::Name,
        v060::SortKey::Path,
        v060::SortKey::Size,
        v060::SortKey::DateModified,
    ] {
        let old = v060::Request::Query(v060::QueryRequest {
            query: "x".into(),
            scope: None,
            sort: v060::SortSpec {
                key,
                dir: v060::SortDir::Asc,
            },
            offset: 0,
            limit: Some(10),
            want_size: true,
            want_mtime: true,
            match_case: false,
            want_total_size: true,
        });
        let json = serde_json::to_string(&old).unwrap();
        let Ok(Request::Query(q)) = serde_json::from_str::<Request>(&json) else {
            panic!("{json}")
        };
        assert!(q.want_total_size && !q.want_ctime && !q.want_atime);
        assert!(!q.want_attrib && !q.want_rtime);
        assert_eq!(
            serde_json::to_string(&Request::Query(q)).unwrap(),
            json,
            "the same bytes back"
        );
    }
    for old in [
        v060::Request::Status,
        v060::Request::Hello {
            proto_min: 1,
            proto_max: 1,
            client: "goz 0.6.0".into(),
        },
    ] {
        let json = serde_json::to_string(&old).unwrap();
        assert!(serde_json::from_str::<Request>(&json).is_ok(), "{json}");
    }

    // What the new daemon sends reads in v0.6.0.
    let hello = serde_json::to_string(&Response::Hello {
        proto: 1,
        server: "gozd 0.7.0".into(),
        ready: true,
        features: vec![feature::WATCH.into(), feature::FOLDER_SIZES.into()],
    })
    .unwrap();
    assert_eq!(
        serde_json::from_str::<v060::Response>(&hello).expect(&hello),
        v060::Response::Hello {
            proto: 1,
            server: "gozd 0.7.0".into(),
            ready: true,
        }
    );
    let status = serde_json::to_string(&Response::Status(status(vec![ColumnStatus {
        column: "index_attributes".into(),
        settled: true,
        built: 0,
        of: 5,
        bytes: 10,
    }])))
    .unwrap();
    let Ok(v060::Response::Status(old)) = serde_json::from_str::<v060::Response>(&status) else {
        panic!("{status}")
    };
    assert_eq!(old.volumes[0].generation, 9);
    let error = serde_json::to_string(&Response::Error {
        code: ProtoError::Unsupported,
        message: "needs administrator".into(),
    })
    .unwrap();
    assert!(serde_json::from_str::<v060::Response>(&error).is_ok());

    // A page for a query that asked for no column: v0.6.0's bytes, read
    // whole by v0.6.0's decoder, folder sizes included.
    let mut new = Vec::new();
    let mut old = Vec::new();
    push_results_header(&mut new, 2, 5, false, false, true, None, 2);
    push_results_header(&mut old, 2, 5, false, false, true, None, 2);
    for (path, is_dir, size, mtime) in [
        (&b"C:\\dir"[..], true, Some(1357u64), Some(9i64)),
        (&b"C:\\dir\\f.txt"[..], false, None, None),
    ] {
        push_item_with(&mut new, path, is_dir, size, mtime, &ItemColumns::default());
        v060::push_item(&mut old, path, is_dir, size, mtime);
    }
    assert_eq!(new, old);
    let (page, read) = v060::decode_results_payload(&new[1..]).expect("v0.6.0 reads it");
    assert_eq!(read, new.len() - 1, "every byte, no more");
    assert_eq!(page.items[0].size, Some(1357), "a folder's total");
}

/// A new client against an old daemon: an old Hello reads as listing no
/// feature, so the client sends nothing new; what is new either reads in
/// v0.6.0 as before (the column flags, ignored, so no column comes back) or
/// is refused there as malformed (a new request kind or sort key), never
/// misread; and v0.6.0's pages and Status read in the new client.
#[test]
fn a_new_client_reads_an_old_daemon() {
    let hello = serde_json::to_string(&v060::Response::Hello {
        proto: 1,
        server: "gozd 0.6.0".into(),
        ready: true,
    })
    .unwrap();
    let Ok(Response::Hello { features, .. }) = serde_json::from_str::<Response>(&hello) else {
        panic!("{hello}")
    };
    assert!(features.is_empty(), "an old daemon lists nothing");

    // Requests a new client sends only when the feature is listed: v0.6.0
    // refuses each as a malformed request.
    let mut refused = vec![
        Request::Watch {
            volumes: None,
            min_interval_ms: 100,
        },
        Request::GetConfig,
        Request::SetConfig(ConfigChange::default()),
    ];
    refused.extend(NEW_KEYS.map(|k| Request::Query(query(k))));
    for req in refused {
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            serde_json::from_str::<v060::Request>(&json).is_err(),
            "v0.6.0 must refuse, not misread: {json}"
        );
    }
    // The column flags read in v0.6.0 as the query without them.
    let mut q = query(SortKey::Size);
    q.want_ctime = true;
    q.want_atime = true;
    q.want_attrib = true;
    q.want_rtime = true;
    let json = serde_json::to_string(&Request::Query(q)).unwrap();
    let Ok(v060::Request::Query(old)) = serde_json::from_str::<v060::Request>(&json) else {
        panic!("{json}")
    };
    assert_eq!(old.sort.key, v060::SortKey::Size);
    assert!(old.want_size && old.want_mtime);

    // v0.6.0's page and Status read in the new client, with no column.
    let mut page = Vec::new();
    push_results_header(&mut page, 1, 2, true, true, false, Some(5), 1);
    v060::push_item(&mut page, b"C:\\a.log", false, Some(5), Some(6));
    let back = decode_results_payload(&page[1..]).expect("decode");
    let item = &back.items[0];
    assert_eq!((item.size, item.mtime_ft), (Some(5), Some(6)));
    assert_eq!(ItemColumns::of(item), ItemColumns::default());
    assert!(back.more && back.volumes_incomplete && back.total_size == Some(5));
    let old_status = v060::Response::Status(v060::DaemonStatus {
        volumes: vec![v060::VolumeStatus {
            guid: "g".into(),
            mounts: vec![r"C:\".into()],
            phase: VolumePhase::Live,
            entries: 1,
            generation: 1,
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
        }],
        volumes_incomplete: false,
        process_working_set: 0,
        process_private_bytes: 0,
    });
    let json = serde_json::to_string(&old_status).unwrap();
    let Ok(Response::Status(s)) = serde_json::from_str::<Response>(&json) else {
        panic!("{json}")
    };
    assert!(s.volumes[0].columns.is_empty());
    // An old daemon's answer to a request it refused reads as an error.
    let refusal = serde_json::to_string(&v060::Response::Error {
        code: v060::ProtoError::BadQuery,
        message: "malformed request: unknown variant `Watch`".into(),
    })
    .unwrap();
    assert!(matches!(
        serde_json::from_str::<Response>(&refusal),
        Ok(Response::Error {
            code: ProtoError::BadQuery,
            ..
        })
    ));
}
