//! Everything 1.5's own answers to the modifiers, functions, sizes and
//! dates of step S2 of the query-language work, as goz must give them.
//!
//! The fixture tree (DESIGN.md §0), each entry with its size and date
//! modified, is mirrored as a volume below a mount prefix as long as the one
//! Everything indexed it under, so the length functions measure the same
//! paths, and each query below runs on it with the clock its battery ran at.
//! The expected entries are what a live Everything 1.5.0.1424b answered for
//! the same query over the same tree, through its IPC. Queries whose answer
//! differs for a documented reason are listed with it in `DIFFERENT`; those
//! goz refuses with a precise error, where Everything answers nothing or
//! something odd, in `REFUSED`, with Everything's count.

use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::query::{DateOrder, LocalClock, parse_query_at, run_query};
use goz_core::types::{Frn, SortSpec};
use goz_core::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
use std::collections::{BTreeSet, HashMap};

/// Everything's mount prefix for the tree, character for character.
const PREFIX: &str = r"C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\";

/// The tree: `D` for a folder, `F` for a file, its size and date modified
/// (FILETIME), then the path below the volume root.
const TREE: &str = r#"D 0 134387749313416199 fx
D 0 134387741803691098 fx\case1
D 0 134387741803691098 fx\case2
D 0 134387741803696099 fx\case3
D 0 134387745361691679 fx\d2
D 0 134387741803934693 fx\dates
D 0 134387741803884599 fx\desktop
D 0 134387741803596061 fx\dia
D 0 134387749313476340 fx\dia3
D 0 134387749313476340 fx\ext
D 0 134387741803904615 fx\len
D 0 134387745361781825 fx\p
D 0 134387749313476340 fx\size
D 0 134387741803874603 fx\w
D 0 134387741803686101 fx\words
F 0 134387741803691098 fx\case1\Foo.txt
F 0 134387741803691098 fx\case2\FOO.TXT
F 0 134387741803696099 fx\case3\foo.txt
F 0 134102628000000000 fx\d2\dec2025.txt
F 0 134396385360000000 fx\d2\fut10d.txt
F 0 134390337360000000 fx\d2\fut3d.txt
F 0 134133732000000000 fx\d2\jan2026.txt
F 0 133917732000000000 fx\d2\may2025.txt
F 0 134386884000000000 fx\d2\mon1109.txt
F 0 134356644000000000 fx\d2\oct2026b.txt
F 0 134362689360000000 fx\d2\rel29d.txt
F 0 134387673360000000 fx\d2\rel2h.txt
F 0 134360097360000000 fx\d2\rel32d.txt
F 0 134382129360000000 fx\d2\rel6d12h.txt
F 0 134381661360000000 fx\d2\rel7d1h.txt
F 0 134385156000000000 fx\d2\sat1107.txt
F 0 134339364000000000 fx\d2\sep2026.txt
F 0 134386020000000000 fx\d2\sun1108.txt
F 0 132235461000000000 fx\d2\t1015.txt
F 0 134383428000000000 fx\d2\thu1105.txt
F 0 134382564000000000 fx\d2\wed1104.txt
F 0 103952484000000000 fx\d2\y1930.txt
F 0 110264004000000000 fx\d2\y1950.txt
F 0 135194724000000000 fx\d2\y2029.txt
F 0 141506244000000000 fx\d2\y2049.txt
F 0 125649867670000000 fx\dates\d1999.txt
F 0 132235470000000000 fx\dates\d2020a.txt
F 0 132539219400000000 fx\dates\d2020b.txt
F 0 132670008000000000 fx\dates\d2021.txt
F 0 133590276000000000 fx\dates\d2024may.txt
F 0 133596180000000000 fx\dates\d2024wed.txt
F 0 134380656000000000 fx\dates\d2026nov.txt
F 0 134369424000000000 fx\dates\d2026oct.txt
D 0 134387741803879603 fx\desktop\First Level First
D 0 134387741803889602 fx\desktop\other
D 0 134387741803884599 fx\desktop\First Level First\inner
F 0 134387741803879603 fx\desktop\First Level First\f.txt
F 0 134387741803884599 fx\desktop\First Level First\inner\g.txt
F 0 134387741803889602 fx\desktop\other\h.txt
F 0 134387741803462077 fx\dia\CAFÉ2.txt
F 0 134387741803472076 fx\dia\Straße.txt
F 0 134387741803482069 fx\dia\aeble.txt
F 0 134387741803566066 fx\dia\ano.txt
F 0 134387741803561061 fx\dia\año.txt
F 0 134387741803457029 fx\dia\cafe.txt
F 0 134387741803467077 fx\dia\cafénfd.txt
F 0 134387741803448891 fx\dia\café.txt
F 0 134387741803532239 fx\dia\cag.txt
F 0 134387741803507152 fx\dia\dak.txt
F 0 134387741803546057 fx\dia\file.txt
F 0 134387741803551067 fx\dia\full.txt
F 0 134387741803522238 fx\dia\istanbul.txt
F 0 134387741803517192 fx\dia\kirmizi.txt
F 0 134387741803507152 fx\dia\kırmızı.txt
F 0 134387741803507152 fx\dia\lodz.txt
F 0 134387741803581065 fx\dia\naive.txt
F 0 134387741803581065 fx\dia\naïve.txt
F 0 134387741803487065 fx\dia\oeuvre.txt
F 0 134387741803576060 fx\dia\pho.txt
F 0 134387741803571063 fx\dia\phở.txt
F 0 134387741803591066 fx\dia\q.txt
F 0 134387741803586063 fx\dia\q́.txt
F 0 134387741803497102 fx\dia\soren.txt
F 0 134387741803472076 fx\dia\strasse.txt
F 0 134387741803497102 fx\dia\søren.txt
F 0 134387741803596061 fx\dia\thdr.txt
F 0 134387741803556063 fx\dia\x2y.txt
F 0 134387741803556063 fx\dia\x²y.txt
F 0 134387741803487065 fx\dia\Æble2.txt
F 0 134387741803482069 fx\dia\æble.txt
F 0 134387741803527236 fx\dia\çağ.txt
F 0 134387741803591066 fx\dia\þðr.txt
F 0 134387741803507152 fx\dia\đak.txt
F 0 134387741803522238 fx\dia\İstanbul.txt
F 0 134387741803502145 fx\dia\łodź.txt
F 0 134387741803487065 fx\dia\œuvre.txt
F 0 134387741803566066 fx\dia\άλφα.txt
F 0 134387741803571063 fx\dia\αλφα.txt
F 0 134387741803477065 fx\dia\ẞ3.txt
F 0 134387741803532239 fx\dia\ﬁle.txt
F 0 134387741803551067 fx\dia\ｆｕｌｌ.txt
F 0 134387749313476340 fx\dia3\dzx.txt
F 0 134387749313446327 fx\dia3\eb.txt
F 0 134387749313421198 fx\dia3\hal.txt
F 0 134387749313441338 fx\dia3\ijs.txt
F 0 134387749313471329 fx\dia3\la.txt
F 0 134387749313436343 fx\dia3\na.txt
F 0 134387749313446327 fx\dia3\sun.txt
F 0 134387749313421198 fx\dia3\tarx.txt
F 0 134387749313421198 fx\dia3\ħal.txt
F 0 134387749313436343 fx\dia3\ĳs.txt
F 0 134387749313471329 fx\dia3\ŀa.txt
F 0 134387749313431265 fx\dia3\ŋa.txt
F 0 134387749313421198 fx\dia3\ŧar.txt
F 0 134387749313441338 fx\dia3\ſun.txt
F 0 134387749313471329 fx\dia3\ǆx.txt
F 0 134387749313446327 fx\dia3\əb.txt
F 0 134387749313461307 fx\dia3\ας.txt
F 0 134387749313466336 fx\dia3\ασ.txt
F 0 134387749313461307 fx\dia3\еж.txt
F 0 134387749313456327 fx\dia3\иод.txt
F 0 134387749313451339 fx\dia3\йод.txt
F 0 134387749313456327 fx\dia3\ёж.txt
D 0 134387741803744336 fx\ext\dir.txt
D 0 134387741803739334 fx\ext\folder.with.dot
F 0 134387741803711150 fx\ext\.hidden
F 0 134387741803701095 fx\ext\a.txt
F 0 134387741803701095 fx\ext\b.TXT
F 0 134387741803706101 fx\ext\c.tar.gz
F 0 134387741803724332 fx\ext\d.pdf
F 0 134387741803724332 fx\ext\e.exe
F 0 134387749313476340 fx\ext\exe
F 0 134387741803729331 fx\ext\i.jpg
F 0 134387741803734337 fx\ext\m.mpeg
F 0 134387741803706101 fx\ext\noext
F 0 134387741803739334 fx\ext\q.Txt
F 0 134387741803734337 fx\ext\two..dots
F 0 134387741803714293 fx\ext\x.mp3
F 0 134387749313476340 fx\ext\xexe.dat
F 0 134387741803719335 fx\ext\y.mp4
F 0 134387741803719335 fx\ext\z.zip
F 0 134387741803894602 fx\len\l10.abcdef
F 0 134387741803894602 fx\len\l7.abcd
F 0 134387741803904615 fx\len\lllllllllllllllllllllllll.txt
F 0 134387741803899604 fx\len\😀.txt
F 0 134387745361721664 fx\p\a#b.txt
F 0 134387745361701656 fx\p\a$b.txt
F 0 134387745361721664 fx\p\a%b.txt
F 0 134387745361721664 fx\p\a&b.txt
F 0 134387745361731693 fx\p\a'b.txt
F 0 134387745361696658 fx\p\a(b).txt
F 0 134387745361706655 fx\p\a,b.txt
F 0 134387745361776789 fx\p\a1b.txt
F 0 134387745361711656 fx\p\a;b.txt
F 0 134387745361716656 fx\p\a=b.txt
F 0 134387745361716656 fx\p\a@b.txt
F 0 134387745361746745 fx\p\a[b.txt
F 0 134387745361741751 fx\p\a^b.txt
F 0 134387745361736744 fx\p\a`b.txt
F 0 134387745361741751 fx\p\a{b.txt
F 0 134387745361701656 fx\p\a~b.txt
F 0 134387745361756736 fx\p\a b.txt
F 0 134387745361766760 fx\p\a©b.txt
F 0 134387745361751738 fx\p\a·b.txt
F 0 134387745361776789 fx\p\aéb.txt
F 0 134387745361766760 fx\p\áb.txt
F 0 134387745361781825 fx\p\aאb.txt
F 0 134387745361751738 fx\p\a—b.txt
F 0 134387745361706655 fx\p\a€b.txt
F 0 134387745361761736 fx\p\a　b.txt
F 0 134387741803744336 fx\size\s0.bin
F 1 134387741803754376 fx\size\s1.bin
F 1023 134387741803759418 fx\size\s1023.bin
F 1024 134387741803764422 fx\size\s1024.bin
F 10240 134387741803779422 fx\size\s10240.bin
F 102400 134387741803784421 fx\size\s102400.bin
F 102401 134387741803794463 fx\size\s102401.bin
F 10241 134387741803784421 fx\size\s10241.bin
F 1048576 134387741803804516 fx\size\s1048576.bin
F 1048577 134387741803809518 fx\size\s1048577.bin
F 1500 134387741803764422 fx\size\s1500.bin
F 1572864 134387741803819514 fx\size\s1572864.bin
F 1600 134387749313476340 fx\size\s1600.bin
F 2047 134387741803769925 fx\size\s2047.bin
F 2048 134387741803774425 fx\size\s2048.bin
F 2097151 134387741803829517 fx\size\s2097151.bin
F 2097152 134387741803839515 fx\size\s2097152.bin
D 0 134387741803869607 fx\w\mywindows
D 0 134387741803889602 fx\w\windows
D 0 134387741803874603 fx\w\windows.old
D 0 134387741803869607 fx\w\windows2
F 0 134387741803869607 fx\w\mywindows\c.exe
D 0 134387741803889602 fx\w\windows\emptydir
D 0 134387741803864616 fx\w\windows\sub
D 0 134387741803849517 fx\w\windows\system32
F 0 134387741803849517 fx\w\windows\cmd.exe
F 0 134387741803849517 fx\w\windows\notepad.exe
D 0 134387741803864616 fx\w\windows\sub\deep
F 0 134387741803864616 fx\w\windows\sub\deep\a.exe
D 0 134387741803859555 fx\w\windows\system32\drivers
F 0 134387741803849517 fx\w\windows\system32\cmd.exe
F 0 134387741803859555 fx\w\windows\system32\drivers\x.sys
F 0 134387741803874603 fx\w\windows.old\d.exe
F 0 134387741803869607 fx\w\windows2\b.exe
F 0 134387741803666102 fx\words\!bang.txt
F 0 134387741803641101 fx\words\123ever.txt
F 0 134387741803676097 fx\words\a+b.txt
F 0 134387741803686101 fx\words\ab12cd.txt
F 0 134387741803646096 fx\words\camelCaseEver.txt
F 0 134387741803651103 fx\words\ever's.txt
F 0 134387741803681103 fx\words\ever-and-ever.txt
F 0 134387741803631097 fx\words\ever.txt
F 0 134387741803641101 fx\words\ever123.txt
F 0 134387741803636103 fx\words\everest.txt
F 0 134387741803621122 fx\words\foo bar.txt
F 0 134387741803626101 fx\words\foo-bar.txt
F 0 134387741803616099 fx\words\foo_bar.txt
F 0 134387741803621122 fx\words\foobar.txt
F 0 134387741803661097 fx\words\foot.txt
F 0 134387741803636103 fx\words\forever.txt
F 0 134387741803671100 fx\words\gray.txt
F 0 134387741803671100 fx\words\grey.txt
F 0 134387741803651103 fx\words\hosts
F 0 134387741803656101 fx\words\hosts.txt
F 0 134387741803656101 fx\words\myhosts
F 0 134387741803626101 fx\words\never.txt
F 0 134387741803611112 fx\words\spider man.txt
F 0 134387741803601101 fx\words\spider-man.txt
F 0 134387741803601101 fx\words\spider.man.txt
F 0 134387741803611112 fx\words\spider_man.txt
F 0 134387741803606099 fx\words\spiderman.txt
F 0 134387741803666102 fx\words\wow!.txt
F 0 134387741803681103 fx\words\x(1).txt"#;

/// The clock of a battery: UTC+3, weeks from Sunday, dates written M/D/Y
/// (en-US), at the time it ran.
struct Battery(i64);

const HOUR: i64 = 36_000_000_000;

impl LocalClock for Battery {
    fn now(&self) -> i64 {
        self.0
    }
    fn to_local(&self, utc: i64) -> Option<i64> {
        utc.checked_add(3 * HOUR)
    }
    fn to_utc(&self, local: i64) -> Option<i64> {
        local.checked_sub(3 * HOUR)
    }
    fn week_start(&self) -> u8 {
        6
    }
    fn date_order(&self) -> DateOrder {
        DateOrder::MonthDayYear
    }
}

/// 2026-11-10T18:28:49Z, while battery 7 ran (18:28:47-51).
const L7: Battery = Battery(134388089290000000);
/// 2026-11-10T18:32:12Z, while battery 8 ran (18:32:11-12).
const L8: Battery = Battery(134388091320000000);

fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}

fn volume() -> VolumeIndex {
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let mut frns: HashMap<&str, Frn> = HashMap::new();
    let mut stats = Vec::new();
    for (i, line) in TREE.lines().enumerate() {
        let mut fields = line.splitn(4, ' ');
        let (kind, size, mtime, path) = (
            fields.next().unwrap(),
            fields.next().unwrap().parse::<u64>().unwrap(),
            fields.next().unwrap().parse::<i64>().unwrap(),
            fields.next().unwrap(),
        );
        let (parent, name) = match path.rsplit_once('\\') {
            Some((parent, name)) => (frns[parent], name),
            None => (NTFS_ROOT_FRN, path),
        };
        let f = frn(1000 + i as u64);
        frns.insert(path, f);
        let is_dir = kind == "D";
        idx.insert_enum(&ParsedUsnRecord {
            major_version: 2,
            frn: f,
            parent_frn: parent,
            usn: 0,
            timestamp_ft: 0,
            reason: USN_REASON_FILE_CREATE,
            attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
            name: name.as_bytes().to_vec(),
            name_lossy: false,
        });
        stats.push((f, size, mtime));
    }
    for (f, size, mtime) in stats {
        idx.set_stat(f, size, mtime);
    }
    idx.optimize_storage();
    idx
}

fn below_fx(path: &str) -> String {
    path.strip_prefix("fx\\")
        .unwrap_or(if path == "fx" { "" } else { path })
        .to_string()
}

#[test]
fn goz_answers_as_everything_does() {
    let idx = volume();
    assert_eq!(TREE.lines().count(), 228);
    let mut failures = Vec::new();
    for &(battery, query, want) in GOLDEN {
        let clock = match battery {
            "L7" => &L7,
            _ => &L8,
        };
        let want: BTreeSet<String> = want.iter().map(|p| p.to_string()).collect();
        let parsed = match parse_query_at(query, false, clock) {
            Ok(parsed) => parsed,
            Err(e) => {
                failures.push(format!("{query}: {e}"));
                continue;
            }
        };
        let out = run_query(&idx, PREFIX, &parsed, None, SortSpec::default(), 0, None);
        let got: BTreeSet<String> = out
            .hits
            .iter()
            .map(|h| below_fx(&String::from_utf8_lossy(&h.path)))
            .collect();
        if got != want || out.total != want.len() as u64 {
            let missing: Vec<&String> = want.difference(&got).collect();
            let extra: Vec<&String> = got.difference(&want).collect();
            failures.push(format!("{query}: missing {missing:?}, extra {extra:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        GOLDEN.len(),
        failures.join("\n")
    );
}

/// The queries goz refuses with an error rather than answer as Everything
/// does (DESIGN.md §3.2-§3.3): each is refused.
#[test]
fn goz_refuses_what_it_cannot_read() {
    for &(query, _everything_found) in REFUSED {
        assert!(parse_query_at(query, false, &L8).is_err(), "{query}");
    }
}

/// The battery, the query, and the entries Everything found, below `fx`.
#[rustfmt::skip]
const GOLDEN: &[(&str, &str, &[&str])] = &[
    (r#"L7"#, r#"case:Foo"#, &[r#"case1\Foo.txt"#]),
    (r#"L7"#, r#"case:FOO"#, &[r#"case2\FOO.TXT"#]),
    (r#"L7"#, r#"case:foo"#, &[r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"nocase:FOO"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"no-case:FOO"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"case:"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other"#, r#"desktop\other\h.txt"#, r#"dia"#, r#"dia3"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext"#, r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"len"#, r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"size"#, r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#, r#"w"#, r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"?case: Foo"#, &[r#"case1\Foo.txt"#]),
    (r#"L7"#, r#"::case:Foo"#, &[r#"case1\Foo.txt"#]),
    (r#"L7"#, r#"case:<Foo FOO>"#, &[]),
    (r#"L7"#, r#"case:nocase:Foo"#, &[r#"case1\Foo.txt"#]),
    (r#"L7"#, r#"nocase:case:Foo"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"path:words"#, &[r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"words"#, &[r#"words"#]),
    (r#"L7"#, r#"nopath:words"#, &[r#"words"#]),
    (r#"L7"#, r#"path:words ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"\:words ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"file: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"folder: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"files: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"folders: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"dir: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"folder:dot"#, &[r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"file:dot"#, &[r#"ext\two..dots"#]),
    (r#"L7"#, r#"file: folder: \ext\"#, &[]),
    (r#"L7"#, r#"folder:dir"#, &[r#"ext\dir.txt"#, r#"w\windows\emptydir"#]),
    (r#"L7"#, r#"whole:hosts"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"exact:hosts"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"wfn:hosts"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"whole-filename:hosts"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"entire:hosts"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"whole:hosts.txt"#, &[r#"words\hosts.txt"#]),
    (r#"L7"#, r#"whole:HOSTS"#, &[r#"words\hosts"#]),
    (r#"L7"#, r#"startwith:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"start-with:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"^:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"endwith:hosts"#, &[r#"words\hosts"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"$:hosts"#, &[r#"words\hosts"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"endwith:ever"#, &[]),
    (r#"L7"#, r#"endwith:.txt"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other\h.txt"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\dir.txt"#, r#"ext\q.Txt"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts.txt"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"startwith:*ver"#, &[]),
    (r#"L7"#, r#"ww:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"ww:spider"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#]),
    (r#"L7"#, r#"ww:man"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#]),
    (r#"L7"#, r#"ww:foo"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#]),
    (r#"L7"#, r#"ww:bar"#, &[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#]),
    (r#"L7"#, r#"ww:123"#, &[]),
    (r#"L7"#, r#"ww:ever's"#, &[r#"words\ever's.txt"#]),
    (r#"L7"#, r#"ww:case"#, &[]),
    (r#"L7"#, r#"ww:Ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"wholeword:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"whole-word:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"whole-words:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"ww:spider-man"#, &[r#"words\spider-man.txt"#]),
    (r#"L7"#, r#"ww:"spider man""#, &[r#"words\spider man.txt"#]),
    (r#"L7"#, r#"ww:ab12cd"#, &[r#"words\ab12cd.txt"#]),
    (r#"L7"#, r#"ww:12"#, &[]),
    (r#"L7"#, r#"ww:txt"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other\h.txt"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\dir.txt"#, r#"ext\q.Txt"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts.txt"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"ww:*ever"#, &[]),
    (r#"L7"#, r#"ww:ev*"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"prefix:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"prefix:man"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#]),
    (r#"L7"#, r#"prefix:foo"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"prefix:bar"#, &[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#]),
    (r#"L7"#, r#"prefix:12"#, &[r#"words\123ever.txt"#]),
    (r#"L7"#, r#"prefix:case"#, &[r#"case1"#, r#"case2"#, r#"case3"#]),
    (r#"L7"#, r#"suffix:ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"suffix:man"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"suffix:foo"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#]),
    (r#"L7"#, r#"suffix:bar"#, &[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#]),
    (r#"L7"#, r#"suffix:hosts"#, &[r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"suffix:cd"#, &[r#"len\l7.abcd"#, r#"words\ab12cd.txt"#]),
    (r#"L7"#, r#"suffix:12"#, &[]),
    (r#"L7"#, r#"nopunc:spiderman"#, &[r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"ignore-punctuation:spiderman"#, &[r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"nows:spiderman"#, &[r#"words\spider man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"ignore-whitespace:spiderman"#, &[r#"words\spider man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"nopunc:nows:spiderman"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"nopunc:foobar"#, &[r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#]),
    (r#"L7"#, r#"nows:foobar"#, &[r#"words\foo bar.txt"#, r#"words\foobar.txt"#]),
    (r#"L7"#, r#"len:7 \len\"#, &[r#"len\l7.abcd"#]),
    (r#"L7"#, r#"len:10 \len\"#, &[r#"len\l10.abcdef"#]),
    (r#"L7"#, r#"len:>20 \len\"#, &[r#"len\lllllllllllllllllllllllll.txt"#]),
    (r#"L7"#, r#"len:<8 \len\"#, &[r#"len\l7.abcd"#, r#"len\😀.txt"#]),
    (r#"L7"#, r#"len:6 \len\"#, &[r#"len\😀.txt"#]),
    (r#"L7"#, r#"len:5 \len\"#, &[]),
    (r#"L7"#, r#"len:7..10 \len\"#, &[r#"len\l10.abcdef"#, r#"len\l7.abcd"#]),
    (r#"L7"#, r#"path:len:>100 \len\"#, &[r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#]),
    (r#"L7"#, r#"name-length:7 \len\"#, &[r#"len\l7.abcd"#]),
    (r#"L7"#, r#"namelen:7"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"desktop"#, r#"dia3\dzx.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\sun.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\ﬁle.txt"#, r#"ext\.hidden"#, r#"ext\dir.txt"#, r#"len\l7.abcd"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"w\windows"#, r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"words\a+b.txt"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"full-path-length:>200"#, &[]),
    (r#"L7"#, r#"len:"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other"#, r#"desktop\other\h.txt"#, r#"dia"#, r#"dia3"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext"#, r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"len"#, r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"size"#, r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#, r#"w"#, r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"ext:txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:TXT \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:.txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:*.txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:*txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:mp* \ext\"#, &[r#"ext\m.mpeg"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"ext:mp? \ext\"#, &[r#"ext\x.mp3"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"ext: \ext\"#, &[r#"ext\exe"#, r#"ext\noext"#]),
    (r#"L7"#, r#"ext:txt;exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:txt ext:exe \ext\"#, &[]),
    (r#"L7"#, r#"!ext:txt \ext\"#, &[r#"ext\.hidden"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"ext:gz \ext\"#, &[r#"ext\c.tar.gz"#]),
    (r#"L7"#, r#"ext:tar.gz \ext\"#, &[r#"ext\c.tar.gz"#]),
    (r#"L7"#, r#"ext:dot \ext\"#, &[]),
    (r#"L7"#, r#"ext:dots \ext\"#, &[r#"ext\two..dots"#]),
    (r#"L7"#, r#"ext:hidden \ext\"#, &[r#"ext\.hidden"#]),
    (r#"L7"#, r#"ext:txt|ext:exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"case:ext:TXT \ext\"#, &[r#"ext\b.TXT"#]),
    (r#"L7"#, r#"case:ext:txt \ext\"#, &[r#"ext\a.txt"#]),
    (r#"L7"#, r#"extension:t \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\xexe.dat"#]),
    (r#"L7"#, r#"extension:mp \ext\"#, &[r#"ext\m.mpeg"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"!extension: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\noext"#]),
    (r#"L7"#, r#"ext:t \ext\"#, &[]),
    (r#"L7"#, r#"ext:"txt;exe" \ext\"#, &[]),
    (r#"L7"#, r#"size:1kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#]),
    (r#"L7"#, r#"size:=1kb \size\"#, &[r#"size\s1024.bin"#]),
    (r#"L7"#, r#"size:==1kb \size\"#, &[r#"size\s1024.bin"#]),
    (r#"L7"#, r#"size:>1kb \size\"#, &[r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:>=1kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:<1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#]),
    (r#"L7"#, r#"size:<=1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#]),
    (r#"L7"#, r#"size:1kb..2kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:1kb-2kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:!=1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:!1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:1024 \size\"#, &[r#"size\s1024.bin"#]),
    (r#"L7"#, r#"size:1500 \size\"#, &[r#"size\s1500.bin"#]),
    (r#"L7"#, r#"size:1.5kb \size\"#, &[r#"size\s1600.bin"#]),
    (r#"L7"#, r#"size:1.5mb \size\"#, &[r#"size\s1572864.bin"#]),
    (r#"L7"#, r#"size:1mb \size\"#, &[r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2097151.bin"#]),
    (r#"L7"#, r#"size:>1mb \size\"#, &[r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:<2mb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#]),
    (r#"L7"#, r#"size:tiny \size\"#, &[r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:empty \size\"#, &[r#"size\s0.bin"#]),
    (r#"L7"#, r#"size:small \size\"#, &[r#"size\s102400.bin"#, r#"size\s10241.bin"#]),
    (r#"L7"#, r#"size:medium \size\"#, &[r#"size\s102401.bin"#, r#"size\s1048576.bin"#]),
    (r#"L7"#, r#"size:large \size\"#, &[r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size: \size\"#, &[r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:unknown \size\"#, &[]),
    (r#"L7"#, r#"size:1024;2048 \size\"#, &[r#"size\s1024.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:<1024|2048> \size\"#, &[r#"size\s1024.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:1kib \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#]),
    (r#"L7"#, r#"size:1k \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#]),
    (r#"L7"#, r#"size:1KB \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#]),
    (r#"L7"#, r#"size:2mb \size\"#, &[r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:>1mb size:<2mb \size\"#, &[r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2097151.bin"#]),
    (r#"L7"#, r#"size:10kb..1mb \size\"#, &[r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#]),
    (r#"L7"#, r#"!size:1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:0 \size\"#, &[r#"size\s0.bin"#]),
    (r#"L7"#, r#"size:1b \size\"#, &[r#"size\s1.bin"#]),
    (r#"L7"#, r#"size:2kb.. \size\"#, &[r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:..1kb \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#]),
    (r#"L7"#, r#"size:1048576 \size\"#, &[r#"size\s1048576.bin"#]),
    (r#"L7"#, r#"size:>1048576 \size\"#, &[r#"size\s1048577.bin"#, r#"size\s1572864.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"dm:2020 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"dm:20 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"dm:99 \dates\"#, &[r#"dates\d1999.txt"#]),
    (r#"L7"#, r#"dm:2020-01 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15T10 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15T10:30 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15T11 \dates\"#, &[]),
    (r#"L7"#, r#"dm:20200115 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:200115 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:1/15/2020 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:01/2020 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020/01 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020/01/15 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:jan \dates\"#, &[]),
    (r#"L7"#, r#"dm:january \dates\"#, &[]),
    (r#"L7"#, r#"dm:may \dates\"#, &[]),
    (r#"L7"#, r#"dm:wed \dates\"#, &[]),
    (r#"L7"#, r#"dm:wednesday \dates\"#, &[]),
    (r#"L7"#, r#"dm:mtd \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:ytd \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:qtd \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:today \dates\"#, &[]),
    (r#"L7"#, r#"dm:yesterday \dates\"#, &[]),
    (r#"L7"#, r#"dm:thisweek \dates\"#, &[]),
    (r#"L7"#, r#"dm:currentweek \dates\"#, &[]),
    (r#"L7"#, r#"dm:lastweek \dates\"#, &[]),
    (r#"L7"#, r#"dm:prevweek \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:previousweek \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:prev2weeks \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:thismonth \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:lastmonth \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:last2months \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:past2months \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:pastmonth \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:lastyear \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:thisyear \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:nextweek \dates\"#, &[]),
    (r#"L7"#, r#"dm:comingweek \dates\"#, &[]),
    (r#"L7"#, r#"dm:next2days \dates\"#, &[]),
    (r#"L7"#, r#"dm:tomorrow \dates\"#, &[]),
    (r#"L7"#, r#"dm:now \dates\"#, &[]),
    (r#"L7"#, r#"dm:30days \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:last30days \dates\"#, &[r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:3weeks \dates\"#, &[r#"dates\d2026nov.txt"#]),
    (r#"L7"#, r#"dm:>2020 \dates\"#, &[r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:>=2020 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:<2020 \dates\"#, &[r#"dates\d1999.txt"#]),
    (r#"L7"#, r#"dm:<=2020 \dates\"#, &[r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"dm:2020..2021 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#]),
    (r#"L7"#, r#"dm:2020-2021 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#]),
    (r#"L7"#, r#"dm:!2020 \dates\"#, &[r#"dates\d1999.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:!=2020 \dates\"#, &[r#"dates\d1999.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:2020;2024 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#]),
    (r#"L7"#, r#"date:2020 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"datemodified:2020 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"date-modified:2020 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"dm:unknown \dates\"#, &[]),
    (r#"L7"#, r#"dm: \dates\"#, &[r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#]),
    (r#"L7"#, r#"dm:2020-1-15 \dates\"#, &[r#"dates\d2020a.txt"#]),
    (r#"L7"#, r#"dm:2020-13 \dates\"#, &[r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"infolder:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"in-folder:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"nosubfolders:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"no-sub-folders:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\W\WINDOWS"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"!parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows \w\"#, &[r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows|parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\system32"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows;C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\system32"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows *.exe"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\win*"#, &[]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"c: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"!c: \words\"#, &[]),
    (r#"L7"#, r#"d: \words\"#, &[]),
    (r#"L7"#, r#"c:|d: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"c:\ \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"C: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"c:spider"#, &[]),
    (r#"L7"#, r#"audio:"#, &[r#"ext\x.mp3"#]),
    (r#"L7"#, r#"zip:"#, &[r#"ext\c.tar.gz"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"doc: \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"exe:"#, &[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"image:"#, &[r#"ext\i.jpg"#]),
    (r#"L7"#, r#"video:"#, &[r#"ext\m.mpeg"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"pic:"#, &[r#"ext\i.jpg"#]),
    (r#"L7"#, r#"audio:|video:"#, &[r#"ext\m.mpeg"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"!exe: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"audio:x"#, &[r#"ext\x.mp3"#]),
    (r#"L7"#, r#"name:spider"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"stem:spider \words\"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"stem:txt \ext\"#, &[r#"ext\dir.txt"#]),
    (r#"L7"#, r#"extension:txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"full-path:windows\cmd"#, &[r#"w\windows\cmd.exe"#]),
    (r#"L7"#, r#"location:windows"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"path-part:windows"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"pp:windows"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"basename:hosts"#, &[r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"name:hosts"#, &[r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#]),
    (r#"L7"#, r#"?ww: ever spider"#, &[]),
    (r#"L7"#, r#"?path: words ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"?nopunc: spiderman"#, &[r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"dm:50 \d2\"#, &[r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:49 \d2\"#, &[r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:30 \d2\"#, &[]),
    (r#"L7"#, r#"dm:29 \d2\"#, &[r#"d2\y2029.txt"#]),
    (r#"L7"#, r#"dm:jan \d2\"#, &[r#"d2\jan2026.txt"#]),
    (r#"L7"#, r#"dm:january \d2\"#, &[r#"d2\jan2026.txt"#]),
    (r#"L7"#, r#"dm:may \d2\"#, &[]),
    (r#"L7"#, r#"dm:nov \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:november \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:oct \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#]),
    (r#"L7"#, r#"dm:dec \d2\"#, &[r#"d2\dec2025.txt"#]),
    (r#"L7"#, r#"dm:monday \d2\"#, &[r#"d2\mon1109.txt"#]),
    (r#"L7"#, r#"dm:wed \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:thu \d2\"#, &[r#"d2\thu1105.txt"#]),
    (r#"L7"#, r#"dm:sat \d2\"#, &[r#"d2\sat1107.txt"#]),
    (r#"L7"#, r#"dm:sun \d2\"#, &[r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:thisweek \d2\"#, &[r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:currentweek \d2\"#, &[r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:lastweek \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:last1week \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:1week \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:pastweek \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:prevweek \d2\"#, &[r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:previousweek \d2\"#, &[r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:prevmonth \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#]),
    (r#"L7"#, r#"dm:previousmonth \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#]),
    (r#"L7"#, r#"dm:prevyear \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\may2025.txt"#]),
    (r#"L7"#, r#"dm:lastmonth \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:pastmonth \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:thismonth \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:currentmonth \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:lastyear \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:pastyear \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:thisyear \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:currentyear \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:prev2months \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\sep2026.txt"#]),
    (r#"L7"#, r#"dm:last2weeks \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:7days \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:today \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:yesterday \d2\"#, &[r#"d2\mon1109.txt"#]),
    (r#"L7"#, r#"dm:tomorrow \d2\"#, &[]),
    (r#"L7"#, r#"dm:nextweek \d2\"#, &[r#"d2\fut10d.txt"#]),
    (r#"L7"#, r#"dm:comingweek \d2\"#, &[r#"d2\fut10d.txt"#]),
    (r#"L7"#, r#"dm:next5days \d2\"#, &[r#"d2\fut3d.txt"#]),
    (r#"L7"#, r#"dm:coming5days \d2\"#, &[r#"d2\fut3d.txt"#]),
    (r#"L7"#, r#"dm:nextmonth \d2\"#, &[]),
    (r#"L7"#, r#"dm:now \d2\"#, &[]),
    (r#"L7"#, r#"dm:>now \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:<now \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:>today \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:>=today \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:<yesterday \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:2hours \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:3hours \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:1hour \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:mtd \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:qtd \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:ytd \d2\"#, &[r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:lastweek..today \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026-11-04..2026-11-08 \d2\"#, &[r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026-11-08..2026-11-04 \d2\"#, &[r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15T10:00..2020-01-15T10:20 \d2\"#, &[r#"d2\t1015.txt"#]),
    (r#"L7"#, r#"dm:>=2020-01-15T10:15 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:2020-01-15T10:15:00 \d2\"#, &[r#"d2\t1015.txt"#]),
    (r#"L7"#, r#"dm:11/2026 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026/11 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:11/4/2026 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:4/11/2026 \d2\"#, &[]),
    (r#"L7"#, r#"dm:2026-11 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:202611 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:261104 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:20261104 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"ww:a \p\"#, &[r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#]),
    (r#"L7"#, r#"ww:b \p\"#, &[r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#]),
    (r#"L7"#, r#"prefix:b \p\"#, &[r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#]),
    (r#"L7"#, r#"suffix:a \p\"#, &[r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#]),
    (r#"L7"#, r#"len:7 \p\"#, &[r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#]),
    (r#"L7"#, r#"len:8 \p\"#, &[r#"p\a(b).txt"#]),
    (r#"L7"#, r#"size:<1kb dm:>2020 \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#]),
    (r#"L7"#, r#"size:<1kb|2048> \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:>1kb <\size\>"#, &[r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"dm:<2021 \dates\"#, &[r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#]),
    (r#"L7"#, r#"dm:<2021|2024> \dates\"#, &[r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#]),
    (r#"L7"#, r#"ext:ar.gz \ext\"#, &[]),
    (r#"L7"#, r#"ext:tar \ext\"#, &[]),
    (r#"L7"#, r#"ext:.tar.gz \ext\"#, &[r#"ext\c.tar.gz"#]),
    (r#"L7"#, r#"ext:txt|exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\q.Txt"#, r#"ext\xexe.dat"#]),
    (r#"L7"#, r#"ext:txt;;exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\noext"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:; \ext\"#, &[r#"ext\exe"#, r#"ext\noext"#]),
    (r#"L7"#, r#"!case:foo \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"case:!foo \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"!size: \size\"#, &[r#"size\s0.bin"#]),
    (r#"L7"#, r#"!file: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"case:<Foo|FOO>"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#]),
    (r#"L7"#, r#"case:<foo bar>"#, &[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#]),
    (r#"L7"#, r#"<case:Foo>"#, &[r#"case1\Foo.txt"#]),
    (r#"L7"#, r#"ww:<ever|spider>"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#]),
    (r#"L7"#, r#"!ww:ever \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"\windows\"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"\windows"#, &[r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"windows\"#, &[r#"w\mywindows\c.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"/windows/"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"\words\ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"path:*\ever*"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"path:words\ever*"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"path:ever*"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"nopath:windows\cmd.exe"#, &[]),
    (r#"L7"#, r#"name:*.exe"#, &[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"case:W*\c*"#, &[]),
    (r#"L7"#, r#"case:w*\c*"#, &[r#"w\windows\cmd.exe"#, r#"words\camelCaseEver.txt"#]),
    (r#"L7"#, r#""w\windows\*.exe""#, &[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#]),
    (r#"L7"#, r#"w\windows\*.EXE"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#]),
    (r#"L7"#, r#"\ \words\"#, &[]),
    (r#"L7"#, r#"\|foo"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"dates"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\inner"#, r#"desktop\other"#, r#"dia"#, r#"dia3"#, r#"ext"#, r#"ext\dir.txt"#, r#"ext\folder.with.dot"#, r#"len"#, r#"p"#, r#"size"#, r#"w"#, r#"w\mywindows"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#, r#"w\windows\emptydir"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\system32"#, r#"w\windows\system32\drivers"#, r#"words"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"!\ \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"\ \"#, &[r#""#, r#"case1"#, r#"case2"#, r#"case3"#, r#"d2"#, r#"dates"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\inner"#, r#"desktop\other"#, r#"dia"#, r#"dia3"#, r#"ext"#, r#"ext\dir.txt"#, r#"ext\folder.with.dot"#, r#"len"#, r#"p"#, r#"size"#, r#"w"#, r#"w\mywindows"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#, r#"w\windows\emptydir"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\system32"#, r#"w\windows\system32\drivers"#, r#"words"#]),
    (r#"L7"#, r#""\""#, &[r#""#, r#"case1"#, r#"case2"#, r#"case3"#, r#"d2"#, r#"dates"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\inner"#, r#"desktop\other"#, r#"dia"#, r#"dia3"#, r#"ext"#, r#"ext\dir.txt"#, r#"ext\folder.with.dot"#, r#"len"#, r#"p"#, r#"size"#, r#"w"#, r#"w\mywindows"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#, r#"w\windows\emptydir"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\system32"#, r#"w\windows\system32\drivers"#, r#"words"#]),
    (r#"L7"#, r#"c:\"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other"#, r#"desktop\other\h.txt"#, r#"dia"#, r#"dia3"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext"#, r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"len"#, r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"size"#, r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#, r#"w"#, r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"!c:\"#, &[]),
    (r#"L7"#, r#"foo " " bar"#, &[r#"words\foo bar.txt"#]),
    (r#"L7"#, r#""foo bar"|ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"startwith:foo bar"#, &[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#]),
    (r#"L7"#, r#"endwith:.txt ever"#, &[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"whole:"foo bar.txt""#, &[r#"words\foo bar.txt"#]),
    (r#"L7"#, r#"whole:foo*"#, &[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"whole:*.txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\dir.txt"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"startwith:a \ext\"#, &[r#"ext\a.txt"#]),
    (r#"L7"#, r#"path:startwith:c:\users \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"path:endwith:.txt \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts.txt"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"dm:tue \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:tuesday \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"ext:exe \ext\"#, &[r#"ext\e.exe"#]),
    (r#"L7"#, r#"nopunc:a.b \p\"#, &[]),
    (r#"L7"#, r#"nopunc:a_b \p\"#, &[]),
    (r#"L7"#, r#"size:1.6kb \size\"#, &[]),
    (r#"L7"#, r#"size:1.56kb \size\"#, &[r#"size\s1600.bin"#]),
    (r#"L7"#, r#"path:\ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"nopath:\ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"/ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"\\ \ext\"#, &[]),
    (r#"L7"#, r#""\" \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"path:"\" \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"stem: \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"stem:hidden \ext\"#, &[]),
    (r#"L7"#, r#"stem:.hidden \ext\"#, &[]),
    (r#"L7"#, r#"extension:hidden \ext\"#, &[r#"ext\.hidden"#]),
    (r#"L7"#, r#"extension: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"extlen:3 \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\i.jpg"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"extension-length:3 \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\i.jpg"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"extlen:0 \ext\"#, &[r#"ext\exe"#, r#"ext\noext"#]),
    (r#"L7"#, r#"name-length:5 \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\i.jpg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"path-part-length:>10 \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"location: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"location:ext \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"parent-name:ext"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"parent-name:fx"#, &[r#"case1"#, r#"case2"#, r#"case3"#, r#"d2"#, r#"dates"#, r#"desktop"#, r#"dia"#, r#"dia3"#, r#"ext"#, r#"len"#, r#"p"#, r#"size"#, r#"w"#, r#"words"#]),
    (r#"L7"#, r#"parent-name:windows"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"parent-name:"First Level First""#, &[r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#]),
    (r#"L7"#, r#"name: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"full-path: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"len:>5 \ext\"#, &[r#"ext\.hidden"#, r#"ext\c.tar.gz"#, r#"ext\dir.txt"#, r#"ext\folder.with.dot"#, r#"ext\m.mpeg"#, r#"ext\two..dots"#, r#"ext\xexe.dat"#]),
    (r#"L7"#, r#"len:5 \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\i.jpg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"size: \ext\"#, &[]),
    (r#"L7"#, r#"size:unknown \ext\"#, &[]),
    (r#"L7"#, r#"!size: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"?file: dot"#, &[r#"ext\two..dots"#]),
    (r#"L7"#, r#"?folder: dot"#, &[r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"?folder: dot ?file: x"#, &[]),
    (r#"L7"#, r#"nofile:dot"#, &[r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"no-folder: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"no-files: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"folders-only: \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L7"#, r#"files-only: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"file:folder: \ext\"#, &[]),
    (r#"L7"#, r#"folder:!dot \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"dir:!dot \ext\"#, &[r#"ext\dir.txt"#]),
    (r#"L7"#, r#"file:!dot \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"startwith:ever endwith:.txt"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#]),
    (r#"L7"#, r#"whole:ever.txt"#, &[r#"words\ever.txt"#]),
    (r#"L7"#, r#"ww:ever.txt"#, &[r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"ww:txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\dir.txt"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"prefix:ever.t"#, &[r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"suffix:r.txt"#, &[r#"dia3\ŧar.txt"#, r#"dia\thdr.txt"#, r#"dia\þðr.txt"#, r#"ext\dir.txt"#, r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#]),
    (r#"L7"#, r#"nopunc:ww:spiderman"#, &[r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"nopunc:startwith:spiderman"#, &[r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"nows:endwith:man.txt"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L7"#, r#"case:ww:Ever"#, &[]),
    (r#"L7"#, r#"case:ww:ever"#, &[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#]),
    (r#"L7"#, r#"case:prefix:Ever"#, &[]),
    (r#"L7"#, r#"?case: foo nocase:FOO"#, &[r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#]),
    (r#"L7"#, r#"starall:windows*.exe"#, &[]),
    (r#"L7"#, r#"starall:w*x.sys"#, &[]),
    (r#"L7"#, r#"starall:*\windows\*.exe"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"star-all:w\*.exe"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"no-star-all:w\*.exe"#, &[]),
    (r#"L7"#, r#"nowildcards:*.exe"#, &[]),
    (r#"L7"#, r#"no-wildcards:x(1).txt"#, &[r#"words\x(1).txt"#]),
    (r#"L7"#, r#"no-filename-wildcards:*.exe"#, &[]),
    (r#"L7"#, r#"filename-wildcards:*.exe"#, &[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"metric:size:1kb \size\"#, &[r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#]),
    (r#"L7"#, r#"metric:size:=1kb \size\"#, &[]),
    (r#"L7"#, r#"metric:size:1000 \size\"#, &[]),
    (r#"L7"#, r#"size:2kb..1kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:1kb;2kb \size\"#, &[r#"size\s1024.bin"#, r#"size\s1500.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#]),
    (r#"L7"#, r#"size:!=0 \size\"#, &[r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L7"#, r#"size:huge;gigantic \size\"#, &[]),
    (r#"L7"#, r#"len:!7 \len\"#, &[r#"len\l10.abcdef"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#]),
    (r#"L7"#, r#"namelen:>9 \len\"#, &[r#"len\l10.abcdef"#, r#"len\lllllllllllllllllllllllll.txt"#]),
    (r#"L7"#, r#"fullpathlen:>130 \len\"#, &[r#"len\l10.abcdef"#, r#"len\lllllllllllllllllllllllll.txt"#]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w ext:exe"#, &[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#]),
    (r#"L7"#, r#"!ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w exe:"#, &[r#"ext\e.exe"#]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w"#, &[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows;C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\ext dot"#, &[r#"ext\folder.with.dot"#, r#"ext\two..dots"#]),
    (r#"L7"#, r#"parent:"C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\desktop\First Level First""#, &[r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\desktop\first level first\"#, &[]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx"#, &[r#"case1"#, r#"case2"#, r#"case3"#, r#"d2"#, r#"dates"#, r#"desktop"#, r#"dia"#, r#"dia3"#, r#"ext"#, r#"len"#, r#"p"#, r#"size"#, r#"w"#, r#"words"#]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\nosuch"#, &[]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\cmd.exe"#, &[]),
    (r#"L7"#, r#"ancestor:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows\cmd.exe"#, &[]),
    (r#"L7"#, r#"parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows|parent:C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\ext"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"parent:<C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w\windows|C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\ext>"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#]),
    (r#"L7"#, r#"d:|ext:exe \ext\"#, &[r#"ext\e.exe"#]),
    (r#"L7"#, r#"c:|nosuch"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other"#, r#"desktop\other\h.txt"#, r#"dia"#, r#"dia3"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext"#, r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"len"#, r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"size"#, r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#, r#"w"#, r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"!d: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"C:\ \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"c:\| \ext\"#, &[r#""#, r#"case1"#, r#"case1\Foo.txt"#, r#"case2"#, r#"case2\FOO.TXT"#, r#"case3"#, r#"case3\foo.txt"#, r#"d2"#, r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#, r#"dates"#, r#"dates\d1999.txt"#, r#"dates\d2020a.txt"#, r#"dates\d2020b.txt"#, r#"dates\d2021.txt"#, r#"dates\d2024may.txt"#, r#"dates\d2024wed.txt"#, r#"dates\d2026nov.txt"#, r#"dates\d2026oct.txt"#, r#"desktop"#, r#"desktop\First Level First"#, r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#, r#"desktop\other"#, r#"desktop\other\h.txt"#, r#"dia"#, r#"dia3"#, r#"dia3\dzx.txt"#, r#"dia3\eb.txt"#, r#"dia3\hal.txt"#, r#"dia3\ijs.txt"#, r#"dia3\la.txt"#, r#"dia3\na.txt"#, r#"dia3\sun.txt"#, r#"dia3\tarx.txt"#, r#"dia3\ħal.txt"#, r#"dia3\ĳs.txt"#, r#"dia3\ŀa.txt"#, r#"dia3\ŋa.txt"#, r#"dia3\ŧar.txt"#, r#"dia3\ſun.txt"#, r#"dia3\ǆx.txt"#, r#"dia3\əb.txt"#, r#"dia3\ας.txt"#, r#"dia3\ασ.txt"#, r#"dia3\еж.txt"#, r#"dia3\иод.txt"#, r#"dia3\йод.txt"#, r#"dia3\ёж.txt"#, r#"dia\CAFÉ2.txt"#, r#"dia\Straße.txt"#, r#"dia\aeble.txt"#, r#"dia\ano.txt"#, r#"dia\año.txt"#, r#"dia\cafe.txt"#, r#"dia\cafénfd.txt"#, r#"dia\café.txt"#, r#"dia\cag.txt"#, r#"dia\dak.txt"#, r#"dia\file.txt"#, r#"dia\full.txt"#, r#"dia\istanbul.txt"#, r#"dia\kirmizi.txt"#, r#"dia\kırmızı.txt"#, r#"dia\lodz.txt"#, r#"dia\naive.txt"#, r#"dia\naïve.txt"#, r#"dia\oeuvre.txt"#, r#"dia\pho.txt"#, r#"dia\phở.txt"#, r#"dia\q.txt"#, r#"dia\q́.txt"#, r#"dia\soren.txt"#, r#"dia\strasse.txt"#, r#"dia\søren.txt"#, r#"dia\thdr.txt"#, r#"dia\x2y.txt"#, r#"dia\x²y.txt"#, r#"dia\Æble2.txt"#, r#"dia\æble.txt"#, r#"dia\çağ.txt"#, r#"dia\þðr.txt"#, r#"dia\đak.txt"#, r#"dia\İstanbul.txt"#, r#"dia\łodź.txt"#, r#"dia\œuvre.txt"#, r#"dia\άλφα.txt"#, r#"dia\αλφα.txt"#, r#"dia\ẞ3.txt"#, r#"dia\ﬁle.txt"#, r#"dia\ｆｕｌｌ.txt"#, r#"ext"#, r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#, r#"len"#, r#"len\l10.abcdef"#, r#"len\l7.abcd"#, r#"len\lllllllllllllllllllllllll.txt"#, r#"len\😀.txt"#, r#"p"#, r#"p\a#b.txt"#, r#"p\a$b.txt"#, r#"p\a%b.txt"#, r#"p\a&b.txt"#, r#"p\a'b.txt"#, r#"p\a(b).txt"#, r#"p\a,b.txt"#, r#"p\a1b.txt"#, r#"p\a;b.txt"#, r#"p\a=b.txt"#, r#"p\a@b.txt"#, r#"p\a[b.txt"#, r#"p\a^b.txt"#, r#"p\a`b.txt"#, r#"p\a{b.txt"#, r#"p\a~b.txt"#, r#"p\a b.txt"#, r#"p\a©b.txt"#, r#"p\a·b.txt"#, r#"p\aéb.txt"#, r#"p\áb.txt"#, r#"p\aאb.txt"#, r#"p\a—b.txt"#, r#"p\a€b.txt"#, r#"p\a　b.txt"#, r#"size"#, r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#, r#"w"#, r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L7"#, r#"audio:|zip: \ext\"#, &[r#"ext\c.tar.gz"#, r#"ext\x.mp3"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"exe:e \ext\"#, &[r#"ext\e.exe"#]),
    (r#"L7"#, r#"!audio: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"zip:!z \ext\"#, &[]),
    (r#"L7"#, r#"doc:!a \ext\"#, &[r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"pic:|video: \ext\"#, &[r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\y.mp4"#]),
    (r#"L7"#, r#"ext:z*;t* \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"ext:*.t?t \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:!txt \ext\"#, &[]),
    (r#"L7"#, r#"ext:<txt|exe> \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:<txt exe> \ext\"#, &[]),
    (r#"L7"#, r#"!ext: \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"ext:.hidden \ext\"#, &[r#"ext\.hidden"#]),
    (r#"L7"#, r#"ext:tar.* \ext\"#, &[]),
    (r#"L7"#, r#"ext:*.* \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"ext:* \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L7"#, r#"ext:a.txt \ext\"#, &[]),
    (r#"L7"#, r#"ext:TXT;Exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L7"#, r#"ext:dir.txt \ext\"#, &[]),
    (r#"L7"#, r#"ext:with.dot \ext\"#, &[]),
    (r#"L7"#, r#"case:ext:TXT;txt \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#]),
    (r#"L7"#, r#"dm:2026-11-04T09 \d2\"#, &[]),
    (r#"L7"#, r#"dm:2026-11-04T08 \d2\"#, &[]),
    (r#"L7"#, r#"dm:2026-11-04T09:00:00 \d2\"#, &[]),
    (r#"L7"#, r#"dm:2026-11-04T09:00 \d2\"#, &[]),
    (r#"L7"#, r#"dm:20261104T09 \d2\"#, &[]),
    (r#"L7"#, r#"dm:20261104T0900 \d2\"#, &[]),
    (r#"L7"#, r#"dm:11/4/26 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:11/4 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026/11/4 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026/11/04 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026-11-4 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026-1-20 \d2\"#, &[r#"d2\jan2026.txt"#]),
    (r#"L7"#, r#"dm:last2years \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:prev2years \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\may2025.txt"#]),
    (r#"L7"#, r#"dm:previous2years \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\may2025.txt"#]),
    (r#"L7"#, r#"dm:prev2weeks \d2\"#, &[r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:prev3days \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:prevday \d2\"#, &[r#"d2\mon1109.txt"#]),
    (r#"L7"#, r#"dm:next2weeks \d2\"#, &[r#"d2\fut10d.txt"#]),
    (r#"L7"#, r#"dm:coming2months \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#]),
    (r#"L7"#, r#"dm:next1week \d2\"#, &[r#"d2\fut10d.txt"#]),
    (r#"L7"#, r#"dm:nextyear \d2\"#, &[]),
    (r#"L7"#, r#"dm:nextday \d2\"#, &[]),
    (r#"L7"#, r#"dm:next2days \d2\"#, &[]),
    (r#"L7"#, r#"dm:thisweek..today \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:sep..oct \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\sep2026.txt"#]),
    (r#"L7"#, r#"dm:==today \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:=today \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:thisday \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:lasthour \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:<=yesterday \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:>sun \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:<2026-11-05T00:00:00 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sep2026.txt"#, r#"d2\t1015.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:134382564000000000 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:0x1dd6c3adf39e800 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:>=134382564000000000 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:134382564000000000..134383428000000000 \d2\"#, &[r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2026-11-04.. \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:..2025 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\may2025.txt"#, r#"d2\t1015.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:!today \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:today;yesterday \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:sun..sat \d2\"#, &[r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:dec..jan \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#]),
    (r#"L7"#, r#"dm:yesterday..tomorrow \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#]),
    (r#"L7"#, r#"dm:2d \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:2w \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:3h \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:2y \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:2mo \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:5min \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:30sec \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:2hrs \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:1d..3d \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L7"#, r#"dm:2026 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2025-2026 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:2025-12-15-2026-01-20 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#]),
    (r#"L7"#, r#"dm:may2025 \d2\"#, &[r#"d2\may2025.txt"#]),
    (r#"L7"#, r#"dm:31 \d2\"#, &[]),
    (r#"L7"#, r#"dm:dec2025 \d2\"#, &[r#"d2\dec2025.txt"#]),
    (r#"L7"#, r#"dm:dec-2025 \d2\"#, &[r#"d2\dec2025.txt"#]),
    (r#"L7"#, r#"dm:december \d2\"#, &[r#"d2\dec2025.txt"#]),
    (r#"L7"#, r#"dm:thismonth..nextmonth \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:jan..nov \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:nov..jan \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:mtd..today \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L7"#, r#"dm:today.. \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:..yesterday \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:>2026-11 \d2\"#, &[r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L7"#, r#"dm:<=2025 \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\may2025.txt"#, r#"d2\t1015.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L7"#, r#"dm:>=2026-11-08 dm:<=2026-11-09 \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\sun1108.txt"#]),
    (r#"L8"#, r#"dm:2026-11-04T12 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026-11-04T11 \d2\"#, &[]),
    (r#"L8"#, r#"dm:20261104T12 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:20261104T1200 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026-11-04T12:00:00 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026-11-04T12:00 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:>2days \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:<2days \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:>=7days \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sep2026.txt"#, r#"d2\t1015.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:<=1week \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:>1hour \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:<1hour \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:=2days \d2\"#, &[]),
    (r#"L8"#, r#"dm:!2days \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:2days..7days \d2\"#, &[r#"d2\rel6d12h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:>last2days \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:<last2days \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:>past2days \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#]),
    (r#"L8"#, r#"dm:next2months \d2\"#, &[]),
    (r#"L8"#, r#"dm:coming2weeks \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:coming3days \d2\"#, &[r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:next3days \d2\"#, &[r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:comingmonth \d2\"#, &[]),
    (r#"L8"#, r#"dm:coming1month \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:next1month \d2\"#, &[]),
    (r#"L8"#, r#"dm:coming1week \d2\"#, &[r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:coming2days \d2\"#, &[]),
    (r#"L8"#, r#"dm:coming1day \d2\"#, &[]),
    (r#"L8"#, r#"dm:comingday \d2\"#, &[]),
    (r#"L8"#, r#"dm:comingyear \d2\"#, &[]),
    (r#"L8"#, r#"dm:coming2years \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#]),
    (r#"L8"#, r#"dm:next2years \d2\"#, &[]),
    (r#"L8"#, r#"dm:30s \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:2m \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:2secs \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:2h \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:12h \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:previousday \d2\"#, &[r#"d2\mon1109.txt"#]),
    (r#"L8"#, r#"dm:lastday \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:pastday \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:last1day \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:1day \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\rel2h.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:prev1week \d2\"#, &[r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:prev1month \d2\"#, &[r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel32d.txt"#]),
    (r#"L8"#, r#"dm:nextweek..next2weeks \d2\"#, &[r#"d2\fut10d.txt"#]),
    (r#"L8"#, r#"dm:tues \d2\"#, &[r#"d2\rel2h.txt"#]),
    (r#"L8"#, r#"dm:thur \d2\"#, &[r#"d2\thu1105.txt"#]),
    (r#"L8"#, r#"dm:thurs \d2\"#, &[r#"d2\thu1105.txt"#]),
    (r#"L8"#, r#"dm:4/11 \d2\"#, &[]),
    (r#"L8"#, r#"dm:11/04/2026 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026.11.04 \d2\"#, &[r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:04.11.2026 \d2\"#, &[]),
    (r#"L8"#, r#"dm:2026-11-04..2026-11-05 \d2\"#, &[r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026-11-04-2026-11-05 \d2\"#, &[r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:20261104..20261105 \d2\"#, &[r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:nov2026 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:november2026 \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:2026nov \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#]),
    (r#"L8"#, r#"dm:jan2026..mar2026 \d2\"#, &[r#"d2\jan2026.txt"#]),
    (r#"L8"#, r#"dm:yesterday;today \d2\"#, &[r#"d2\mon1109.txt"#, r#"d2\rel2h.txt"#]),
    (r#"L8"#, r#"dm:ytd.. \d2\"#, &[r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"dm:unknown \d2\"#, &[]),
    (r#"L8"#, r#"dm: \d2\"#, &[r#"d2\dec2025.txt"#, r#"d2\fut10d.txt"#, r#"d2\fut3d.txt"#, r#"d2\jan2026.txt"#, r#"d2\may2025.txt"#, r#"d2\mon1109.txt"#, r#"d2\oct2026b.txt"#, r#"d2\rel29d.txt"#, r#"d2\rel2h.txt"#, r#"d2\rel32d.txt"#, r#"d2\rel6d12h.txt"#, r#"d2\rel7d1h.txt"#, r#"d2\sat1107.txt"#, r#"d2\sep2026.txt"#, r#"d2\sun1108.txt"#, r#"d2\t1015.txt"#, r#"d2\thu1105.txt"#, r#"d2\wed1104.txt"#, r#"d2\y1930.txt"#, r#"d2\y1950.txt"#, r#"d2\y2029.txt"#, r#"d2\y2049.txt"#]),
    (r#"L8"#, r#"name:!spider \words\"#, &[]),
    (r#"L8"#, r#"stem:!spider \words\"#, &[]),
    (r#"L8"#, r#"extension:!txt \ext\"#, &[]),
    (r#"L8"#, r#"full-path:!words \words\"#, &[]),
    (r#"L8"#, r#"location:!words \words\"#, &[]),
    (r#"L8"#, r#"parent-name:!words \words\"#, &[]),
    (r#"L8"#, r#"len:!5 \ext\"#, &[r#"ext\.hidden"#, r#"ext\c.tar.gz"#, r#"ext\dir.txt"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\m.mpeg"#, r#"ext\two..dots"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"extlen:!3 \ext\"#, &[r#"ext\.hidden"#, r#"ext\c.tar.gz"#, r#"ext\exe"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\two..dots"#]),
    (r#"L8"#, r#"ext:!txt;exe \ext\"#, &[r#"ext\e.exe"#]),
    (r#"L8"#, r#"ext:"!txt" \ext\"#, &[]),
    (r#"L8"#, r#"size:!0 \size\"#, &[r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L8"#, r#"size:!unknown \size\"#, &[r#"size\s0.bin"#, r#"size\s1.bin"#, r#"size\s1023.bin"#, r#"size\s1024.bin"#, r#"size\s10240.bin"#, r#"size\s102400.bin"#, r#"size\s102401.bin"#, r#"size\s10241.bin"#, r#"size\s1048576.bin"#, r#"size\s1048577.bin"#, r#"size\s1500.bin"#, r#"size\s1572864.bin"#, r#"size\s1600.bin"#, r#"size\s2047.bin"#, r#"size\s2048.bin"#, r#"size\s2097151.bin"#, r#"size\s2097152.bin"#]),
    (r#"L8"#, r#"nofile:!dot \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"nofolder:!dot \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"nofolder:dot"#, &[r#"ext\two..dots"#]),
    (r#"L8"#, r#"!nofile:dot \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"\:\ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"nopath:/ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"<\> \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"\|ext:exe \ext\"#, &[r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"case:\ \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"\* \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"ext:t* \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L8"#, r#"ext:*t \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"ext:?? \ext\"#, &[r#"ext\c.tar.gz"#]),
    (r#"L8"#, r#"ext:?? ext:z* \ext\"#, &[]),
    (r#"L8"#, r#"ext:.* \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"ext:t?t;exe \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L8"#, r#"ext:TXT;Exe;; \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\e.exe"#, r#"ext\q.Txt"#]),
    (r#"L8"#, r#"!ext:*;txt \ext\"#, &[r#"ext\dir.txt"#, r#"ext\folder.with.dot"#]),
    (r#"L8"#, r#"extension:t* \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#]),
    (r#"L8"#, r#"extension:*t \ext\"#, &[r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\q.Txt"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"stem:*x* \ext\"#, &[r#"ext\dir.txt"#, r#"ext\exe"#, r#"ext\noext"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"stem:x* \ext\"#, &[r#"ext\x.mp3"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"name:x* \ext\"#, &[r#"ext\x.mp3"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"location:*ext \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"parent-name:e?t \ext\"#, &[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#]),
    (r#"L8"#, r#"full-path:*ext\a.txt"#, &[r#"ext\a.txt"#]),
    (r#"L8"#, r#"full-path:*\a.txt"#, &[r#"ext\a.txt"#]),
    (r#"L8"#, r#"startwith:x* \ext\"#, &[r#"ext\x.mp3"#, r#"ext\xexe.dat"#]),
    (r#"L8"#, r#"ww:spider* \words\"#, &[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#]),
    (r#"L8"#, r#"startwith:"spider " \words\"#, &[r#"words\spider man.txt"#]),
    (r#"L8"#, r#"whole:"" \words\"#, &[]),
    (r#"L8"#, r#"whole: \words\"#, &[]),
    (r#"L8"#, r#"startwith: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L8"#, r#"ww: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L8"#, r#"nopunc: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
    (r#"L8"#, r#"case: \words\"#, &[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#]),
];

/// Queries of the same batteries that goz refuses, with how many entries
/// Everything found for them.
#[rustfmt::skip]
const REFUSED: &[(&str, usize)] = &[
    (r#"len:abc"#, 0),
    (r#"dm:15/1/2020 \dates\"#, 0),
    (r#"dm:*-05 \dates\"#, 2),
    (r#"dm:*-*-*T10:00..*-*-*T13:00 \dates\"#, 2),
    (r#"dm:43845 \dates\"#, 0),
    (r#"dm:soon \dates\"#, 0),
    (r#"parent:w\windows"#, 0),
    (r#"parent:windows"#, 0),
    (r#"dm:2026-W45 \d2\"#, 15),
    (r#"size:<1024 | 2048> \size\"#, 3),
    (r#"size:<1kb \size\ x>"#, 0),
    (r#"size:<1kb|>1mb \size\"#, 8),
    (r#"size:>=10kb..1mb \size\"#, 9),
    (r#"len:<len:8 \len\"#, 58),
    (r#"dm:26-11-04 \d2\"#, 0),
    (r#"dm:thisquarter \d2\"#, 0),
    (r#"dm:lastquarter \d2\"#, 0),
    (r#"dm:prevquarter \d2\"#, 0),
    (r#"dm:thishour \d2\"#, 0),
    (r#"dm:thisminute \d2\"#, 0),
    (r#"dm:<today|>now> \d2\"#, 0),
    (r#"dm:2hr \d2\"#, 0),
    (r#"dm:2wk \d2\"#, 0),
    (r#"dm:2yr \d2\"#, 0),
    (r#"dm:2mos \d2\"#, 0),
    (r#"dm:2ds \d2\"#, 0),
    (r#"dm:2ws \d2\"#, 0),
    (r#"dm:2ys \d2\"#, 0),
    (r#"dm:2hs \d2\"#, 0),
    (r#"dm:prevhour \d2\"#, 0),
    (r#"dm:sept \d2\"#, 1),
    (r#"dm:<yesterday|>today> \d2\"#, 0),
    (r#"parent:!C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\ext \ext\"#, 0),
    (r#"ancestor:!C:\Users\user1\AppData\Local\Temp\goztmp\0000000000000000\00000000-0000-0000-0000-000000000000\scratchpad\gozd\ev\fx\w \ext\"#, 0),
    (r#"nodir: \ext\"#, 0),
    (r#"no-dir: \ext\"#, 0),
];

/// Queries of the same batteries whose answer differs, and why.
#[rustfmt::skip]
#[allow(dead_code)]
const DIFFERENT: &[(&str, &str)] = &[
    (r#"case: Foo"#, r#"bare case: and case:1 are goz's whole-query switch (DESIGN.md §3.1); Everything reads bare case: as nothing and case:1 as text"#),
    (r#"case:1 Foo"#, r#"bare case: and case:1 are goz's whole-query switch (DESIGN.md §3.1); Everything reads bare case: as nothing and case:1 as text"#),
    (r#"ww:ab"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"prefix:ab"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"nopunc:ab"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"dm:mon \d2\"#, r#"dm:mon: a live Everything reads mon as one month; goz as Monday, as the docs list it (DESIGN.md §3.7)"#),
    (r#"nopunc:ab \p\"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"nows:ab \p\"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"nopunc:nows:ab \p\"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"nopunc:a-b \p\"#, r#"Everything matches an em dash with a hyphen under nopunc:; goz compares them as the characters they are"#),
    (r#"ww:ab \p\"#, r#"the answer holds a name with an accent the query lacks, which Everything folds and goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"nopunc:"a b" \p\"#, r#"Everything folds a no-break and an ideographic space to a space (NFKD), which goz leaves to step S3 (DESIGN.md §10)"#),
    (r#"size:<10 \ext\"#, r#"Everything sums folder sizes, goz does not: a folder matches no size: term (DESIGN.md §2.4 #51)"#),
    (r#"len:7;10 \len\"#, r#"len:7;10: a live Everything reads only the first value of a length list; goz reads every value, as for size:"#),
    (r#"dm:mon..wed \d2\"#, r#"dm:mon: a live Everything reads mon as one month; goz as Monday, as the docs list it (DESIGN.md §3.7)"#),
    (r#"dm:>=mon \d2\"#, r#"dm:mon: a live Everything reads mon as one month; goz as Monday, as the docs list it (DESIGN.md §3.7)"#),
];
