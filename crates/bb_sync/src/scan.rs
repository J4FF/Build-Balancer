// scan -- from a folder to its Tree
//
//   scan(root, &Options, key, &mut known)   walk the project: ignore rules decide what belongs to
//                                           it, every file is hashed chunk by chunk unless it is
//                                           known with the same size and time
//   scan_files(root, paths, key)            the Tree of named files (a job's outputs)
//   expand(root, patterns)                  the files below root that match glob patterns
//
// Symbolic links and special files are left out and counted: a link would have to mean the
// same on every machine, which it rarely does between Windows and Linux.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::crypt::DataKey;
use plat::fs::FileKind;

use crate::glob::{literal_prefix, path_match, Ignore};
use crate::tree::{Entry, Known, KnownFile, Tree};

/// what belongs to a project
#[derive(Clone, Debug)]
pub struct Options {
    /// read .gitignore files (the usual meaning: build output stays at home)
    pub gitignore: bool,
    /// take the .git folder along (most builds do not need it; some read the commit from it)
    pub git_dir: bool,
    /// more patterns to leave out, in .gitignore's syntax
    pub exclude: Vec<String>,
    /// patterns to take along although an ignore file leaves them out
    pub include: Vec<String>,
}

impl Default for Options {
    fn default() -> Options {
        Options { gitignore: true, git_dir: false, exclude: Vec::new(), include: Vec::new() }
    }
}

#[derive(Default, Debug, Clone)]
pub struct ScanStats {
    pub files: u64,
    pub bytes: u64,
    /// files that had to be read (new or changed)
    pub hashed: u64,
    pub hashed_bytes: u64,
    /// links and special files left out
    pub skipped: u64,
    pub ignored: u64,
}

/// the chunk ids of a file, read piece by piece; None: it cannot be read
pub fn hash_file(path: &str, key: &DataKey) -> Option<(u64, Vec<[u8; 32]>)> {
    let f = plat::File::open(path, plat::FO_READ)?;
    let mut ids = Vec::new();
    let mut buf = alloc::vec![0u8; bb_core::CHUNK];
    let mut off = 0u64;
    loop {
        let n = read_chunk(&f, off, &mut buf)?;
        if n == 0 {
            break;
        }
        ids.push(key.id(&buf[..n]));
        off += n as u64;
        if n < buf.len() {
            break;
        }
    }
    Some((off, ids))
}

/// fill buf from the file at off as far as the file goes -> the bytes read; None on an error
pub fn read_chunk(f: &plat::File, off: u64, buf: &mut [u8]) -> Option<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(off + got as u64, &mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Some(got)
}

fn exec_bit(m: &plat::fs::Meta) -> bool {
    !cfg!(windows) && m.mode & 0o100 != 0
}

struct Walk<'a> {
    root: &'a str,
    opts: &'a Options,
    key: &'a DataKey,
    known: &'a mut Known,
    seen: Known,
    ignore: Ignore,
    tree: Tree,
    stats: ScanStats,
    /// files changed this recently are hashed again next time: their time stamp cannot tell a
    /// second change within the same tick apart
    fresh_ns: i64,
}

impl Walk<'_> {
    /// Walk a folder -> whether anything of it belongs to the tree.  `only_included`: the folder
    /// itself is ignored and is walked for what an include pattern names inside it.
    fn dir(&mut self, rel: &str, only_included: bool) -> Result<bool, String> {
        let abs = bb_core::util::below(self.root, rel);
        let Some(mut list) = plat::fs::read_dir(&abs) else { return Err(format!("{} cannot be listed", abs)) };
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let mark = self.ignore.mark();
        for name in [".gitignore", ".bbignore"] {
            if (name == ".bbignore" || self.opts.gitignore) && list.iter().any(|e| e.name == name && e.kind == FileKind::File)
                && let Some(b) = plat::read_file(&plat::fs::join(&abs, name)) {
                    self.ignore.add_file(rel, &String::from_utf8_lossy(&b));
                }
        }
        let mut any = false;
        for e in list {
            let path = if rel.is_empty() { e.name.clone() } else { format!("{}/{}", rel, e.name) };
            let is_dir = e.kind == FileKind::Dir;
            let git = is_dir && e.name == ".git" && !self.opts.git_dir;
            if git || if only_included { !self.ignore.included(&path, is_dir) } else { self.ignore.ignored(&path, is_dir) } {
                // an include pattern may name something further down: look there, and only for that
                if is_dir && !git && self.ignore.include_reaches(&path) && bb_core::util::rel_ok(&path) {
                    any |= self.dir(&path, true)?;
                } else {
                    self.stats.ignored += 1;
                }
                continue;
            }
            if !bb_core::util::rel_ok(&path) {
                // a name the other system cannot hold (a colon, a trailing dot ...)
                self.stats.skipped += 1;
                continue;
            }
            match e.kind {
                FileKind::Dir => {
                    if !self.dir(&path, false)? {
                        self.tree.dirs.push(path);
                    }
                    any = true;
                }
                FileKind::File => {
                    self.file(path)?;
                    any = true;
                }
                _ => self.stats.skipped += 1,
            }
        }
        self.ignore.leave(mark);
        Ok(any)
    }

    fn file(&mut self, path: String) -> Result<(), String> {
        let abs = bb_core::util::below(self.root, &path);
        // gone between the listing and now (a build tool's temporary file): not part of the tree
        let Some(m) = plat::fs::stat(&abs) else { return Ok(()) };
        let exec = exec_bit(&m);
        let known = self.known.files.get(&path).filter(|k| k.size == m.size && k.mtime_ns == m.mtime_ns).cloned();
        let k = match known {
            Some(k) => KnownFile { exec, ..k },
            None => {
                let (size, chunks) = hash_file(&abs, self.key).ok_or_else(|| format!("{} cannot be read", abs))?;
                self.stats.hashed += 1;
                self.stats.hashed_bytes += size;
                KnownFile { size, mtime_ns: m.mtime_ns, exec, chunks }
            }
        };
        self.stats.files += 1;
        self.stats.bytes += k.size;
        self.tree.files.push(Entry { path: path.clone(), exec, size: k.size, chunks: k.chunks.clone() });
        if k.mtime_ns < self.fresh_ns {
            self.seen.files.insert(path, k);
        }
        Ok(())
    }
}

/// The Tree of a project folder.  `known` is what an earlier scan remembered; it is replaced
/// by what this one saw.
pub fn scan(root: &str, opts: &Options, key: &DataKey, known: &mut Known) -> Result<(Tree, ScanStats), String> {
    if plat::file_exists(root) != 2 {
        return Err(format!("{} is not a folder", root));
    }
    let mut ignore = Ignore::new();
    ignore.set_extra(&opts.exclude, &opts.include);
    let fresh_ns = plat::unix_time_ns() - 2_000_000_000;
    let mut w = Walk { root, opts, key, known, seen: Known::default(), ignore, tree: Tree::default(), stats: ScanStats::default(), fresh_ns };
    w.dir("", false)?;
    let (tree, stats, seen) = (w.tree, w.stats, w.seen);
    *known = seen;
    Ok((tree, stats))
}

/// the Tree of named files below root (paths relative, '/' between the parts)
pub fn scan_files(root: &str, paths: &[String], key: &DataKey) -> Result<Tree, String> {
    let mut tree = Tree::default();
    for p in paths {
        let abs = bb_core::util::below(root, p);
        let m = plat::fs::stat(&abs).ok_or_else(|| format!("{} is gone", abs))?;
        let (size, chunks) = hash_file(&abs, key).ok_or_else(|| format!("{} cannot be read", abs))?;
        tree.files.push(Entry { path: p.clone(), exec: exec_bit(&m), size, chunks });
    }
    tree.files.sort_by(|a, b| a.path.cmp(&b.path));
    tree.files.dedup_by(|a, b| a.path == b.path);
    Ok(tree)
}

/// The files below root that the patterns name: a glob over paths ("build/**/*.exe"), a file,
/// or a folder (everything below it).  Links are not followed.  At most `limit` files.
pub fn expand(root: &str, patterns: &[String], limit: usize) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for raw in patterns {
        let pat = raw.trim().replace('\\', "/");
        let pat = pat.trim_start_matches("./").trim_end_matches('/');
        if pat.is_empty() || pat.split('/').any(|p| p == "..") || bb_core::util::absolute(pat) {
            return Err(format!("output pattern \"{}\": it must stay inside the project (a relative path without ..)", raw));
        }
        let abs = bb_core::util::below(root, pat);
        let plain = !pat.split('/').any(base::pattern::has_glob);
        match (plain, plat::fs::lstat(&abs)) {
            (true, Some(m)) if m.is_file() => out.push(String::from(pat)),
            (true, Some(m)) if m.is_dir() && !m.is_symlink() => walk_all(root, pat, &mut |p| out.push(String::from(p)), limit)?,
            (true, _) => {} // nothing there: the job did not make it
            (false, _) => {
                let start = literal_prefix(pat);
                if start.is_empty() || plat::fs::lstat(&bb_core::util::below(root, &start)).is_some_and(|m| m.is_dir() && !m.is_symlink()) {
                    walk_all(root, &start, &mut |p| {
                        if path_match(pat, p) {
                            out.push(String::from(p));
                        }
                    }, limit * 50)?;
                }
            }
        }
        if out.len() > limit {
            return Err(format!("the output patterns name more than {} files: narrow them down", limit));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// every file below a folder of root (links not followed), at most `limit` visited
fn walk_all(root: &str, start: &str, on: &mut dyn FnMut(&str), limit: usize) -> Result<(), String> {
    let mut todo = alloc::vec![String::from(start)];
    let mut seen = 0usize;
    while let Some(rel) = todo.pop() {
        for e in plat::fs::read_dir(&bb_core::util::below(root, &rel)).unwrap_or_default() {
            let path = if rel.is_empty() { e.name.clone() } else { format!("{}/{}", rel, e.name) };
            match e.kind {
                FileKind::Dir => todo.push(path),
                FileKind::File => {
                    seen += 1;
                    if seen > limit {
                        return Err(format!("more than {} files below \"{}\": narrow the output patterns down", limit, start));
                    }
                    on(&path);
                }
                _ => {}
            }
        }
    }
    Ok(())
}
