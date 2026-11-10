//! Everything 1.5's own answers, as goz must give them.
//!
//! The fixture tree of the query-language work (DESIGN.md §0) is mirrored
//! as a volume, below a mount prefix as deep as the one Everything indexed
//! it under, and each query below runs on it. The expected entries are what
//! a live Everything 1.5.0.1424b answered for the same query over the same
//! tree, through its IPC; `All` is every entry, `AllBut` every entry but
//! those listed. Only queries whose meaning goz shares with Everything are
//! here: operators, groups, quotes, entities, precedence switches and
//! 1.5 wildcards (DESIGN.md §2.1-§2.2; its §3 lists where goz differs).
//! The queries of the same battery whose answers differ for a documented
//! reason are listed, with it, in `DIFFERENT`.

use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::query::{parse_query, run_query};
use goz_core::types::{Frn, SortSpec};
use goz_core::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
use std::collections::{BTreeSet, HashMap};

/// Everything's mount prefix for the tree, depth for depth, with the user's
/// folder and the session's renamed.
const PREFIX: &str = r"C:\Users\user\AppData\Local\Temp\goztmp\s0\s1\scratchpad\gozd\ev\";

/// The tree: `D` for a folder, `F` for a file, then the path below the
/// volume root.
const TREE: &str = r#"D fx
D fx\case1
D fx\case2
D fx\case3
D fx\d2
D fx\dates
D fx\desktop
D fx\dia
D fx\dia3
D fx\ext
D fx\len
D fx\p
D fx\size
D fx\w
D fx\words
F fx\case1\Foo.txt
F fx\case2\FOO.TXT
F fx\case3\foo.txt
F fx\d2\dec2025.txt
F fx\d2\fut10d.txt
F fx\d2\fut3d.txt
F fx\d2\jan2026.txt
F fx\d2\may2025.txt
F fx\d2\mon1109.txt
F fx\d2\oct2026b.txt
F fx\d2\rel29d.txt
F fx\d2\rel2h.txt
F fx\d2\rel32d.txt
F fx\d2\rel6d12h.txt
F fx\d2\rel7d1h.txt
F fx\d2\sat1107.txt
F fx\d2\sep2026.txt
F fx\d2\sun1108.txt
F fx\d2\t1015.txt
F fx\d2\thu1105.txt
F fx\d2\wed1104.txt
F fx\d2\y1930.txt
F fx\d2\y1950.txt
F fx\d2\y2029.txt
F fx\d2\y2049.txt
F fx\dates\d1999.txt
F fx\dates\d2020a.txt
F fx\dates\d2020b.txt
F fx\dates\d2021.txt
F fx\dates\d2024may.txt
F fx\dates\d2024wed.txt
F fx\dates\d2026nov.txt
F fx\dates\d2026oct.txt
D fx\desktop\First Level First
D fx\desktop\other
D fx\desktop\First Level First\inner
F fx\desktop\First Level First\f.txt
F fx\desktop\First Level First\inner\g.txt
F fx\desktop\other\h.txt
F fx\dia\CAFÉ2.txt
F fx\dia\Straße.txt
F fx\dia\aeble.txt
F fx\dia\ano.txt
F fx\dia\año.txt
F fx\dia\cafe.txt
F fx\dia\cafénfd.txt
F fx\dia\café.txt
F fx\dia\cag.txt
F fx\dia\dak.txt
F fx\dia\file.txt
F fx\dia\full.txt
F fx\dia\istanbul.txt
F fx\dia\kirmizi.txt
F fx\dia\kırmızı.txt
F fx\dia\lodz.txt
F fx\dia\naive.txt
F fx\dia\naïve.txt
F fx\dia\oeuvre.txt
F fx\dia\pho.txt
F fx\dia\phở.txt
F fx\dia\q.txt
F fx\dia\q́.txt
F fx\dia\soren.txt
F fx\dia\strasse.txt
F fx\dia\søren.txt
F fx\dia\thdr.txt
F fx\dia\x2y.txt
F fx\dia\x²y.txt
F fx\dia\Æble2.txt
F fx\dia\æble.txt
F fx\dia\çağ.txt
F fx\dia\þðr.txt
F fx\dia\đak.txt
F fx\dia\İstanbul.txt
F fx\dia\łodź.txt
F fx\dia\œuvre.txt
F fx\dia\άλφα.txt
F fx\dia\αλφα.txt
F fx\dia\ẞ3.txt
F fx\dia\ﬁle.txt
F fx\dia\ｆｕｌｌ.txt
F fx\dia3\dzx.txt
F fx\dia3\eb.txt
F fx\dia3\hal.txt
F fx\dia3\ijs.txt
F fx\dia3\la.txt
F fx\dia3\na.txt
F fx\dia3\sun.txt
F fx\dia3\tarx.txt
F fx\dia3\ħal.txt
F fx\dia3\ĳs.txt
F fx\dia3\ŀa.txt
F fx\dia3\ŋa.txt
F fx\dia3\ŧar.txt
F fx\dia3\ſun.txt
F fx\dia3\ǆx.txt
F fx\dia3\əb.txt
F fx\dia3\ας.txt
F fx\dia3\ασ.txt
F fx\dia3\еж.txt
F fx\dia3\иод.txt
F fx\dia3\йод.txt
F fx\dia3\ёж.txt
D fx\ext\dir.txt
D fx\ext\folder.with.dot
F fx\ext\.hidden
F fx\ext\a.txt
F fx\ext\b.TXT
F fx\ext\c.tar.gz
F fx\ext\d.pdf
F fx\ext\e.exe
F fx\ext\exe
F fx\ext\i.jpg
F fx\ext\m.mpeg
F fx\ext\noext
F fx\ext\q.Txt
F fx\ext\two..dots
F fx\ext\x.mp3
F fx\ext\xexe.dat
F fx\ext\y.mp4
F fx\ext\z.zip
F fx\len\l10.abcdef
F fx\len\l7.abcd
F fx\len\lllllllllllllllllllllllll.txt
F fx\len\😀.txt
F fx\p\a#b.txt
F fx\p\a$b.txt
F fx\p\a%b.txt
F fx\p\a&b.txt
F fx\p\a'b.txt
F fx\p\a(b).txt
F fx\p\a,b.txt
F fx\p\a1b.txt
F fx\p\a;b.txt
F fx\p\a=b.txt
F fx\p\a@b.txt
F fx\p\a[b.txt
F fx\p\a^b.txt
F fx\p\a`b.txt
F fx\p\a{b.txt
F fx\p\a~b.txt
F fx\p\a b.txt
F fx\p\a©b.txt
F fx\p\a·b.txt
F fx\p\aéb.txt
F fx\p\áb.txt
F fx\p\aאb.txt
F fx\p\a—b.txt
F fx\p\a€b.txt
F fx\p\a　b.txt
F fx\size\s0.bin
F fx\size\s1.bin
F fx\size\s1023.bin
F fx\size\s1024.bin
F fx\size\s10240.bin
F fx\size\s102400.bin
F fx\size\s102401.bin
F fx\size\s10241.bin
F fx\size\s1048576.bin
F fx\size\s1048577.bin
F fx\size\s1500.bin
F fx\size\s1572864.bin
F fx\size\s1600.bin
F fx\size\s2047.bin
F fx\size\s2048.bin
F fx\size\s2097151.bin
F fx\size\s2097152.bin
D fx\w\mywindows
D fx\w\windows
D fx\w\windows.old
D fx\w\windows2
F fx\w\mywindows\c.exe
D fx\w\windows\emptydir
D fx\w\windows\sub
D fx\w\windows\system32
F fx\w\windows\cmd.exe
F fx\w\windows\notepad.exe
D fx\w\windows\sub\deep
F fx\w\windows\sub\deep\a.exe
D fx\w\windows\system32\drivers
F fx\w\windows\system32\cmd.exe
F fx\w\windows\system32\drivers\x.sys
F fx\w\windows.old\d.exe
F fx\w\windows2\b.exe
F fx\words\!bang.txt
F fx\words\123ever.txt
F fx\words\a+b.txt
F fx\words\ab12cd.txt
F fx\words\camelCaseEver.txt
F fx\words\ever's.txt
F fx\words\ever-and-ever.txt
F fx\words\ever.txt
F fx\words\ever123.txt
F fx\words\everest.txt
F fx\words\foo bar.txt
F fx\words\foo-bar.txt
F fx\words\foo_bar.txt
F fx\words\foobar.txt
F fx\words\foot.txt
F fx\words\forever.txt
F fx\words\gray.txt
F fx\words\grey.txt
F fx\words\hosts
F fx\words\hosts.txt
F fx\words\myhosts
F fx\words\never.txt
F fx\words\spider man.txt
F fx\words\spider-man.txt
F fx\words\spider.man.txt
F fx\words\spider_man.txt
F fx\words\spiderman.txt
F fx\words\wow!.txt
F fx\words\x(1).txt"#;

enum Expected {
    All,
    AllBut(&'static [&'static str]),
    Only(&'static [&'static str]),
}

use Expected::{All, AllBut, Only};

fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}

fn volume() -> VolumeIndex {
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let mut frns: HashMap<&str, Frn> = HashMap::new();
    for (i, line) in TREE.lines().enumerate() {
        let (kind, path) = line.split_at(2);
        let (parent, name) = match path.rsplit_once('\\') {
            Some((parent, name)) => (frns[parent], name),
            None => (NTFS_ROOT_FRN, path),
        };
        let f = frn(1000 + i as u64);
        frns.insert(path, f);
        let is_dir = kind == "D ";
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
    }
    idx.optimize_storage();
    idx
}

/// Every entry, as Everything names it: below `fx`, and `fx` itself as "".
fn every_entry() -> BTreeSet<String> {
    TREE.lines().map(|line| below_fx(&line[2..])).collect()
}

fn below_fx(path: &str) -> String {
    path.strip_prefix("fx\\")
        .unwrap_or(if path == "fx" { "" } else { path })
        .to_string()
}

#[test]
fn goz_answers_as_everything_does() {
    let idx = volume();
    let every = every_entry();
    assert_eq!(every.len(), 228);
    let mut failures = Vec::new();
    for (query, expected) in GOLDEN {
        let want: BTreeSet<String> = match expected {
            All => every.clone(),
            AllBut(gone) => every
                .iter()
                .filter(|p| !gone.contains(&p.as_str()))
                .cloned()
                .collect(),
            Only(listed) => listed.iter().map(|p| p.to_string()).collect(),
        };
        let parsed = match parse_query(query) {
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

#[rustfmt::skip]
const GOLDEN: &[(&str, Expected)] = &[
    (r#"foo bar|ever"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"<foo bar>|ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"foo|ever bar"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"!foo \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"!foo|ever \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"!<foo|ever> \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"foo|"#, All),
    (r#"|foo"#, All),
    (r#"foo||ever"#, All),
    (r#"foo | ever"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"foo ! ever \words\"#, Only(&[])),
    (r#"foo !ever \words\"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"wow!.txt"#, Only(&[r#"words\wow!.txt"#])),
    (r#"wow!"#, Only(&[r#"words\wow!.txt"#])),
    (r#"!bang"#, AllBut(&[r#"words\!bang.txt"#])),
    (r#"\words\ !!foo"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"<>"#, All),
    (r#"<> foo"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"foo and: bar"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"foo AND bar"#, Only(&[])),
    (r#"foo or: ever"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"not: foo \words\"#, Only(&[])),
    (r#"!"#, Only(&[])),
    (r#"|"#, All),
    (r#"spider "|" man"#, Only(&[])),
    (r#""spider man""#, Only(&[r#"words\spider man.txt"#])),
    (r#""spider|man""#, Only(&[])),
    (r#"<spider|foo>"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#])),
    (r#"<<spider>>"#, Only(&[r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#])),
    (r#"!<foo> \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"!foo !ever \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"!foo|!ever \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"<foo|ever> ext:txt \words\"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"<foo>|<ever> \words\"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"!<<foo>|<ever>> \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#""foo bar"|ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"foo " " bar"#, Only(&[r#"words\foo bar.txt"#])),
    (r#"!"foo bar" \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#""!bang""#, Only(&[r#"words\!bang.txt"#])),
    (r#"| ever foo"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"|ever foo"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"ever foo|"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"foo not:ever \words\"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"not:not:foo \words\"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"!not:foo \words\"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"! <foo> \words\"#, Only(&[])),
    (r#"<!foo> \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"foo !<ever|bar> \words\"#, Only(&[r#"words\foot.txt"#])),
    (r#"<"foo|bar">"#, Only(&[])),
    (r#""spider"man"#, Only(&[r#"words\spiderman.txt"#])),
    (r#"spider"man""#, Only(&[r#"words\spiderman.txt"#])),
    (r#"NOT:foo \words\"#, Only(&[r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"foo AND: bar"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"foo bar and:"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"foo or:"#, All),
    (r#"foo|<>"#, All),
    (r#"&excl:bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#""&excl:bang""#, Only(&[r#"words\!bang.txt"#])),
    (r#"&#33:bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#"&#x21:bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#"foo&sp:bar"#, Only(&[r#"words\foo bar.txt"#])),
    (r#""foo&sp:bar""#, Only(&[r#"words\foo bar.txt"#])),
    (r#"spider&vert:man"#, Only(&[])),
    (r#"a&b"#, Only(&[r#"p\a&b.txt"#])),
    (r#"&ast:.exe"#, Only(&[])),
    (r#"*.exe&ast:"#, Only(&[])),
    (r#"&quest:.exe"#, Only(&[])),
    (r#"&amp:"#, Only(&[r#"p\a&b.txt"#])),
    (r#"?and-precedence: foo bar|ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"foo bar|ever ?and-precedence:"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"and-precedence: foo bar|ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"?or-precedence: foo bar|ever"#, Only(&[r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#])),
    (r#"no-!:!bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#"no-!: !bang"#, AllBut(&[r#"words\!bang.txt"#])),
    (r#"?no-!: !bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#"no!:!bang"#, Only(&[r#"words\!bang.txt"#])),
    (r#"!no-!:!bang \words\"#, Only(&[r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"file:!a \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"file:<!a> \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"!file:a \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"file: !a \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\b.TXT"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"folder:!dot \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"folder:<!dot> \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"!folder:dot \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"folder: !dot \ext\"#, Only(&[r#"ext\dir.txt"#])),
    (r#"folder:dot \ext\"#, Only(&[r#"ext\folder.with.dot"#])),
    (r#"folder:!zzz \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\dir.txt"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\folder.with.dot"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"file:!zzz \ext\"#, Only(&[r#"ext\.hidden"#, r#"ext\a.txt"#, r#"ext\b.TXT"#, r#"ext\c.tar.gz"#, r#"ext\d.pdf"#, r#"ext\e.exe"#, r#"ext\exe"#, r#"ext\i.jpg"#, r#"ext\m.mpeg"#, r#"ext\noext"#, r#"ext\q.Txt"#, r#"ext\two..dots"#, r#"ext\x.mp3"#, r#"ext\xexe.dat"#, r#"ext\y.mp4"#, r#"ext\z.zip"#])),
    (r#"dir:!dot \ext\"#, Only(&[r#"ext\dir.txt"#])),
    (r#"path:!words \case1\"#, Only(&[r#"case1\Foo.txt"#])),
    (r#"path:<words ever>"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"path:<words|case1> foo"#, Only(&[r#"case1\Foo.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#])),
    (r#"file:path:words ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"path:words ever"#, Only(&[r#"words\123ever.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\forever.txt"#, r#"words\never.txt"#])),
    (r#"path:words"#, Only(&[r#"words"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"*.exe"#, Only(&[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"windows\*"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#])),
    (r#"windows\*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"windows\**"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\**.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"\windows\*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"w*\n*"#, Only(&[r#"w\windows\notepad.exe"#, r#"words\never.txt"#])),
    (r#"windows\*\"#, Only(&[r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\sys*\"#, Only(&[r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\sys*"#, Only(&[r#"w\windows\system32"#])),
    (r#"**\cmd.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"**cmd*"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"*cmd*"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"cmd*"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#""*.exe""#, Only(&[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#""First Level*""#, Only(&[r#"desktop\First Level First"#])),
    (r#"desktop\*Level" "Fir*"#, Only(&[r#"desktop\First Level First"#])),
    (r#"desktop\*Level" "Fir*\"#, Only(&[r#"desktop\First Level First\f.txt"#, r#"desktop\First Level First\inner"#, r#"desktop\First Level First\inner\g.txt"#])),
    (r#"C:\Users\user\AppData\Local\Temp\goztmp\s0\s1\scratchpad\gozd\ev\fx\w\windows\*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"C:\Users\user\AppData\Local\Temp\goztmp\s0\s1\scratchpad\gozd\ev\fx\w\windows\*"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#])),
    (r#"C:\Users\user\AppData\Local\Temp\goztmp\s0\s1\scratchpad\gozd\ev\fx\w\*\*.exe"#, Only(&[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"*\windows\*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"windows\*\*.exe"#, Only(&[r#"w\windows\system32\cmd.exe"#])),
    (r#"?.exe"#, Only(&[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\sub\deep\a.exe"#])),
    (r#"??.exe"#, Only(&[])),
    (r#"*.?xe"#, Only(&[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"windows*"#, Only(&[r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#])),
    (r#"windows.*"#, Only(&[r#"w\windows.old"#])),
    (r#"*windows*"#, Only(&[r#"w\mywindows"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#])),
    (r#"*\"#, All),
    (r#"w/windows/*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"windows/*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"*"#, All),
    (r#"**"#, All),
    (r#"\*"#, All),
    (r#"*\*\*.exe"#, Only(&[r#"ext\e.exe"#, r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"w\**\x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"**\x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"**x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"*.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"w*\*\*\x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\**\*.exe"#, Only(&[r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"windows\*\**"#, Only(&[r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"path:*\ever*"#, Only(&[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#])),
    (r#"path:words\ever*"#, Only(&[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#])),
    (r#"path:ever*"#, Only(&[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#])),
    (r#""w\windows\*.exe""#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"w\windows\*.EXE"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"w\**\*.exe"#, Only(&[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"\**\x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"C:\**\x.sys"#, Only(&[r#"w\windows\system32\drivers\x.sys"#])),
    (r#"*\cmd.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\system32\cmd.exe"#])),
    (r#"?\cmd.exe"#, Only(&[])),
    (r#"w?ndows\*.exe"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#])),
    (r#"a*\"#, All),
    (r#"!*.exe \w\"#, Only(&[r#"w\mywindows"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows2"#, r#"w\windows\emptydir"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\system32"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"<*.exe|*.sys> \w\"#, Only(&[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\*.exe|*.sys"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\notepad.exe"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"*\*"#, All),
    (r#"*\*\*\*\*"#, All),
    (r#"w\*\*\*\*"#, Only(&[r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\**\"#, Only(&[r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\**\**"#, Only(&[r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"**windows**"#, Only(&[r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"*windows*\*"#, Only(&[r#"w\mywindows\c.exe"#, r#"w\windows.old\d.exe"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#])),
    (r#"w\windows\"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#""w\windows\*""#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\system32"#])),
    (r#"*.exe\"#, Only(&[])),
    (r#"case1\*"#, Only(&[r#"case1\Foo.txt"#])),
    (r#"case*\foo.txt"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#])),
    (r#"CASE1\*.TXT"#, Only(&[r#"case1\Foo.txt"#])),
    (r#"*\Foo.txt"#, Only(&[r#"case1\Foo.txt"#, r#"case2\FOO.TXT"#, r#"case3\foo.txt"#])),
    (r#"**\*"#, All),
    (r#"**\**"#, All),
    (r#"w**\"#, Only(&[r#"w\mywindows"#, r#"w\mywindows\c.exe"#, r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#, r#"words\!bang.txt"#, r#"words\123ever.txt"#, r#"words\a+b.txt"#, r#"words\ab12cd.txt"#, r#"words\camelCaseEver.txt"#, r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#, r#"words\foo bar.txt"#, r#"words\foo-bar.txt"#, r#"words\foo_bar.txt"#, r#"words\foobar.txt"#, r#"words\foot.txt"#, r#"words\forever.txt"#, r#"words\gray.txt"#, r#"words\grey.txt"#, r#"words\hosts"#, r#"words\hosts.txt"#, r#"words\myhosts"#, r#"words\never.txt"#, r#"words\spider man.txt"#, r#"words\spider-man.txt"#, r#"words\spider.man.txt"#, r#"words\spider_man.txt"#, r#"words\spiderman.txt"#, r#"words\wow!.txt"#, r#"words\x(1).txt"#])),
    (r#"*\**"#, All),
    (r#"a\"#, All),
    (r#"**\x.sys\"#, Only(&[])),
    (r#"\words\ever"#, Only(&[r#"words\ever's.txt"#, r#"words\ever-and-ever.txt"#, r#"words\ever.txt"#, r#"words\ever123.txt"#, r#"words\everest.txt"#])),
    (r#"\windows\"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"\windows"#, Only(&[r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"windows\"#, Only(&[r#"w\mywindows\c.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"/windows/"#, Only(&[r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
    (r#"w\windows"#, Only(&[r#"w\windows"#, r#"w\windows.old"#, r#"w\windows.old\d.exe"#, r#"w\windows2"#, r#"w\windows2\b.exe"#, r#"w\windows\cmd.exe"#, r#"w\windows\emptydir"#, r#"w\windows\notepad.exe"#, r#"w\windows\sub"#, r#"w\windows\sub\deep"#, r#"w\windows\sub\deep\a.exe"#, r#"w\windows\system32"#, r#"w\windows\system32\cmd.exe"#, r#"w\windows\system32\drivers"#, r#"w\windows\system32\drivers\x.sys"#])),
];

/// Queries of the battery left out, each with why goz answers otherwise.
const DIFFERENT: &[(&str, &str)] = &[(
    r#"**\a*"#,
    r#"p\áb.txt is stored decomposed; Everything matches `a*` on the raw code points, goz folds NFC and NFD alike (DESIGN.md §3.4), so the name starts with a composed á until the diacritic folding of step S3"#,
)];

/// Each query left out says why, and is no golden case.
#[test]
fn each_difference_is_explained_and_left_out() {
    for (query, why) in DIFFERENT {
        assert!(!why.is_empty(), "{query}");
        assert!(GOLDEN.iter().all(|(golden, _)| golden != query), "{query}");
    }
}
