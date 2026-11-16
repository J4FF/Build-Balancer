// tree -- a folder's files as a list: each file with its size and the ids of its chunks
//
//   Tree         the list, sorted by path; to_text / from_text (one line per file)
//   Known        what a machine remembers of the files it hashed or wrote: size, time and
//                chunk ids per path, so an unchanged file is neither read nor fetched again
//
// The text of a tree, cut into chunks, is itself stored as blobs: a TreeRef names them.
//
//   bbtree 1
//   f x 1832 9f2c...            src/build.sh        file, executable, size, chunk ids, path
//   f - 0 -                     src/empty.txt       an empty file has no chunks
//   f - 9000000 ab..,cd..,ef..  assets/big.bin      a large file: one id per 4 MiB
//   d docs/out                                      an empty folder
//
// Tabs separate the fields; the path comes last and may hold anything but tab and line break.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Entry {
    /// relative, '/' between the parts
    pub path: String,
    /// the owner may run it (Linux; Windows has no such bit)
    pub exec: bool,
    pub size: u64,
    /// one id per chunk of bb_core::CHUNK bytes
    pub chunks: Vec<[u8; 32]>,
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub files: Vec<Entry>,
    /// folders without anything in them
    pub dirs: Vec<String>,
}

fn ids_text(ids: &[[u8; 32]]) -> String {
    if ids.is_empty() { String::from("-") } else { ids.iter().map(|i| crypto::hex(i)).collect::<Vec<_>>().join(",") }
}

fn ids_parse(s: &str) -> Option<Vec<[u8; 32]>> {
    if s == "-" {
        return Some(Vec::new());
    }
    s.split(',').map(bb_core::crypt::key32).collect()
}

impl Tree {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("bbtree 1\n");
        for f in &self.files {
            s += &format!("f\t{}\t{}\t{}\t{}\n", if f.exec { "x" } else { "-" }, f.size, ids_text(&f.chunks), f.path);
        }
        for d in &self.dirs {
            s += &format!("d\t{}\n", d);
        }
        s
    }

    /// None for anything that is not a tree, or names a path that could leave its folder
    pub fn from_text(text: &str) -> Option<Tree> {
        let mut lines = text.lines();
        if lines.next()? != "bbtree 1" {
            return None;
        }
        let mut t = Tree::default();
        for l in lines.filter(|l| !l.is_empty()) {
            let mut p = l.splitn(5, '\t');
            match p.next()? {
                "f" => {
                    let exec = p.next()? == "x";
                    let size: u64 = p.next()?.parse().ok()?;
                    let chunks = ids_parse(p.next()?)?;
                    let path = p.next()?;
                    if !bb_core::util::rel_ok(path) || chunks.len() as u64 != size.div_ceil(bb_core::CHUNK as u64) {
                        return None;
                    }
                    t.files.push(Entry { path: String::from(path), exec, size, chunks });
                }
                "d" => {
                    let path = p.next()?;
                    if !bb_core::util::rel_ok(path) {
                        return None;
                    }
                    t.dirs.push(String::from(path));
                }
                _ => return None,
            }
        }
        Some(t)
    }
}

/// what is remembered of one file
#[derive(Clone, Debug, PartialEq, Default)]
pub struct KnownFile {
    pub size: u64,
    pub mtime_ns: i64,
    pub exec: bool,
    pub chunks: Vec<[u8; 32]>,
}

/// path -> what the file was when it was last hashed (the main machine) or written (a worker)
#[derive(Default)]
pub struct Known {
    pub files: BTreeMap<String, KnownFile>,
}

impl Known {
    pub fn load(path: &str) -> Known {
        let mut k = Known::default();
        let Some(b) = plat::read_file(path) else { return k };
        let text = String::from_utf8_lossy(&b);
        let mut lines = text.lines();
        if lines.next() != Some("bbknown 1") {
            return k;
        }
        for l in lines {
            let mut p = l.splitn(5, '\t');
            let (Some(x), Some(size), Some(mtime), Some(ids), Some(path)) = (p.next(), p.next(), p.next(), p.next(), p.next()) else { continue };
            if let (Ok(size), Ok(mtime_ns), Some(chunks)) = (size.parse(), mtime.parse(), ids_parse(ids)) {
                k.files.insert(String::from(path), KnownFile { size, mtime_ns, exec: x == "x", chunks });
            }
        }
        k
    }

    pub fn save(&self, path: &str) -> bool {
        let mut s = String::from("bbknown 1\n");
        for (p, f) in &self.files {
            s += &format!("{}\t{}\t{}\t{}\t{}\n", if f.exec { "x" } else { "-" }, f.size, f.mtime_ns, ids_text(&f.chunks), p);
        }
        bb_core::util::put_file(path, s.as_bytes())
    }
}
