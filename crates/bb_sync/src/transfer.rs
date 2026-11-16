// transfer -- trees between a folder and the relay's blob store
//
//   upload(relay, key, root, &tree)        make sure the relay has every chunk of the tree's
//                                          files and the tree's own text -> the TreeRef to hand on
//   fetch_tree(relay, key, &TreeRef)       the Tree behind a TreeRef
//   apply(relay, key, root, &tree, &mut known, &Apply)
//                                          make a folder hold the tree's files: fetch what is
//                                          missing or changed, remove what the last tree had and
//                                          this one has not; everything else in the folder (a
//                                          build's own output) is left alone
//
// Only what differs travels: the relay is asked which chunks it lacks, a folder is asked
// which files it already has.  A chunk is checked against its id after decryption, so a blob
// the relay swapped or damaged is refused.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::client::{pack_add, Relay};
use bb_core::crypt::DataKey;
use bb_core::proto::TreeRef;
use bb_core::CHUNK;

use crate::scan::read_chunk;
use crate::tree::{Entry, Known, KnownFile, Tree};

#[derive(Default, Debug, Clone)]
pub struct UploadStats {
    /// chunks the relay lacked, and their bytes before packing
    pub chunks: u64,
    pub bytes: u64,
    /// what went over the wire
    pub sent: u64,
}

fn flush(relay: &Relay, pack: &mut Vec<u8>, stats: &mut UploadStats) -> Result<(), String> {
    if !pack.is_empty() {
        relay.blobs_put(pack)?;
        stats.sent += pack.len() as u64;
        pack.clear();
    }
    Ok(())
}

/// Upload what the relay lacks of a tree.  `progress(done, total)` in bytes of content to send.
pub fn upload(relay: &Relay, key: &DataKey, root: &str, tree: &Tree, progress: &mut dyn FnMut(u64, u64)) -> Result<(TreeRef, UploadStats), String> {
    let mut stats = UploadStats::default();
    // the files' chunks the relay does not have yet
    let all: Vec<[u8; 32]> = tree.files.iter().flat_map(|f| f.chunks.iter().copied()).collect::<BTreeSet<_>>().into_iter().collect();
    let has = relay.blobs_has(&all)?;
    let mut missing: BTreeSet<[u8; 32]> = all.iter().zip(&has).filter(|(_, h)| !**h).map(|(id, _)| *id).collect();
    let total: u64 = tree.files.iter().map(|f| f.chunks.iter().enumerate().filter(|(_, id)| missing.contains(*id)).map(|(i, _)| chunk_len(f.size, i)).sum::<u64>()).sum();
    let mut done = 0u64;
    let mut pack: Vec<u8> = Vec::new();
    let mut buf = alloc::vec![0u8; CHUNK];
    if !missing.is_empty() {
        progress(0, total);
        for f in &tree.files {
            if !f.chunks.iter().any(|id| missing.contains(id)) {
                continue;
            }
            let abs = bb_core::util::below(root, &f.path);
            let file = plat::File::open(&abs, plat::FO_READ).ok_or_else(|| format!("{} cannot be read", abs))?;
            for (i, id) in f.chunks.iter().enumerate() {
                if !missing.remove(id) {
                    continue;
                }
                let n = read_chunk(&file, (i * CHUNK) as u64, &mut buf).ok_or_else(|| format!("{} cannot be read", abs))?;
                if key.id(&buf[..n]) != *id {
                    return Err(format!("{} changed while it was being sent; run it again once the folder is quiet", f.path));
                }
                let blob = key.seal(id, &buf[..n]);
                if !pack.is_empty() && pack.len() + blob.len() > bb_core::PACK_MAX {
                    flush(relay, &mut pack, &mut stats)?;
                }
                pack_add(&mut pack, id, &blob);
                stats.chunks += 1;
                stats.bytes += n as u64;
                done += n as u64;
                progress(done, total);
            }
        }
    }
    // the tree's own text
    let text = tree.to_text();
    let parts: Vec<&[u8]> = text.as_bytes().chunks(CHUNK).collect();
    let ids: Vec<[u8; 32]> = parts.iter().map(|p| key.id(p)).collect();
    let has = relay.blobs_has(&ids)?;
    for ((part, id), h) in parts.iter().zip(&ids).zip(&has) {
        if !*h {
            let blob = key.seal(id, part);
            if !pack.is_empty() && pack.len() + blob.len() > bb_core::PACK_MAX {
                flush(relay, &mut pack, &mut stats)?;
            }
            pack_add(&mut pack, id, &blob);
        }
    }
    flush(relay, &mut pack, &mut stats)?;
    Ok((TreeRef { chunks: ids.iter().map(|i| crypto::hex(i)).collect(), files: tree.files.len() as u64, bytes: tree.bytes() }, stats))
}

fn chunk_len(size: u64, index: usize) -> u64 {
    (size - (index * CHUNK) as u64).min(CHUNK as u64)
}

/// fetch blobs and hand each one's content to `on`; an id the relay lacks is an error
fn fetch(relay: &Relay, key: &DataKey, ids: &[[u8; 32]], on: &mut dyn FnMut(&[u8; 32], Vec<u8>) -> Result<(), String>) -> Result<(), String> {
    let mut err: Option<String> = None;
    relay.blobs_get(ids, &mut |id, blob| {
        if err.is_some() {
            return;
        }
        let r = match blob {
            None => Err(format!("the relay no longer has blob {} (its store was full and dropped it): send the project again", &crypto::hex(id)[..12])),
            Some(b) => match key.open(id, b, CHUNK) {
                Some(plain) => on(id, plain),
                None => Err(format!("blob {} does not open: damaged at the relay, or sealed with another fleet's key", &crypto::hex(id)[..12])),
            },
        };
        err = r.err();
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// the Tree a TreeRef names
pub fn fetch_tree(relay: &Relay, key: &DataKey, r: &TreeRef) -> Result<Tree, String> {
    let ids: Vec<[u8; 32]> = r.chunks.iter().map(|c| bb_core::crypt::key32(c)).collect::<Option<_>>().ok_or("the tree's reference is damaged")?;
    if ids.is_empty() || ids.len() > 64 {
        return Err(String::from("the tree's reference is empty or too long"));
    }
    let mut parts: BTreeMap<[u8; 32], Vec<u8>> = BTreeMap::new();
    for group in ids.chunks(2) {
        fetch(relay, key, group, &mut |id, plain| {
            parts.insert(*id, plain);
            Ok(())
        })?;
    }
    let mut text = Vec::new();
    for id in &ids {
        text.extend_from_slice(parts.get(id).ok_or("a part of the tree is missing")?);
    }
    Tree::from_text(core::str::from_utf8(&text).map_err(|_| "the tree is not text")?).ok_or_else(|| String::from("the tree does not read: made by a newer version, or it names paths that are not allowed"))
}

/// how apply treats the folder
#[derive(Clone, Copy, Debug)]
pub struct Apply {
    /// remove files the last tree had and this one has not (a worker's workspace); false for
    /// results that are laid over a folder
    pub delete: bool,
    /// a file that is there with the right size is read and compared before it is fetched
    /// (results over a project folder, where nothing is remembered)
    pub compare: bool,
    /// the most bytes to fetch; more is an error before anything is written
    pub budget: u64,
}

#[derive(Default, Debug, Clone)]
pub struct ApplyStats {
    pub written: u64,
    pub bytes: u64,
    pub removed: u64,
    pub kept: u64,
}

fn file_matches(abs: &str, key: &DataKey, e: &Entry) -> bool {
    crate::scan::hash_file(abs, key).is_some_and(|(size, ids)| size == e.size && ids == e.chunks)
}

/// make room for a file or folder at a path: whatever of the wrong kind lies there or on the
/// way to it goes
fn clear_way(root: &str, rel: &str, want_dir: bool) {
    let mut cur = String::new();
    let parts: Vec<&str> = rel.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if !cur.is_empty() {
            cur.push('/');
        }
        cur += part;
        let abs = bb_core::util::below(root, &cur);
        let last = i + 1 == parts.len();
        match plat::fs::lstat(&abs) {
            Some(m) if (!last || want_dir) && !(m.is_dir() && !m.is_symlink()) => {
                bb_core::util::remove_tree(&abs);
            }
            Some(m) if last && !want_dir && !m.is_file() => {
                bb_core::util::remove_tree(&abs);
            }
            _ => {}
        }
    }
}

fn finish_file(abs: &str, e: &Entry, known: &mut Known) {
    if !cfg!(windows) {
        plat::fs::set_mode(abs, if e.exec { 0o755 } else { 0o644 });
    }
    let mtime_ns = plat::fs::stat(abs).map_or(0, |m| m.mtime_ns);
    known.files.insert(e.path.clone(), KnownFile { size: e.size, mtime_ns, exec: e.exec, chunks: e.chunks.clone() });
}

/// The bytes apply() would fetch to make `root` hold the tree (without comparing contents): what
/// a worker checks against its disk quota before it starts.
pub fn need_bytes(root: &str, tree: &Tree, known: &Known) -> u64 {
    tree.files
        .iter()
        .filter(|e| {
            let meta = plat::fs::lstat(&bb_core::util::below(root, &e.path)).filter(|m| m.is_file());
            !matches!((known.files.get(&e.path), &meta), (Some(k), Some(m)) if k.chunks == e.chunks && k.size == m.size && k.mtime_ns == m.mtime_ns)
        })
        .map(|e| e.size)
        .sum()
}

/// Make the folder `root` hold the tree's files.  `known` is what this function wrote there
/// before (a file it wrote that still has its size and time is left alone); it is updated as
/// files arrive, so a sync that fails half way goes on where it stopped.
pub fn apply(relay: &Relay, key: &DataKey, root: &str, tree: &Tree, known: &mut Known, how: &Apply, progress: &mut dyn FnMut(u64, u64)) -> Result<ApplyStats, String> {
    let mut stats = ApplyStats::default();
    if !plat::mkdir(root) {
        return Err(format!("{} cannot be created", root));
    }
    // 1. what is already right
    let mut need: Vec<&Entry> = Vec::new();
    for e in &tree.files {
        let abs = bb_core::util::below(root, &e.path);
        let meta = plat::fs::lstat(&abs).filter(|m| m.is_file());
        let same = match (known.files.get(&e.path), &meta) {
            (Some(k), Some(m)) => k.chunks == e.chunks && k.size == m.size && k.mtime_ns == m.mtime_ns,
            _ => false,
        };
        if same {
            if let Some(k) = known.files.get_mut(&e.path)
                && k.exec != e.exec && !cfg!(windows) {
                    plat::fs::set_mode(&abs, if e.exec { 0o755 } else { 0o644 });
                    k.exec = e.exec;
                }
            stats.kept += 1;
        } else if how.compare && meta.as_ref().is_some_and(|m| m.size == e.size) && file_matches(&abs, key, e) {
            finish_file(&abs, e, known);
            stats.kept += 1;
        } else {
            need.push(e);
        }
    }
    let total: u64 = need.iter().map(|e| e.size).sum();
    if total > how.budget {
        return Err(format!("{} to fetch, but only {} of disk space may be used", bb_core::util::size_text(total), bb_core::util::size_text(how.budget)));
    }
    // 2. what the last tree had and this one has not
    if how.delete {
        let now: BTreeSet<&str> = tree.files.iter().map(|e| e.path.as_str()).collect();
        let gone: Vec<String> = known.files.keys().filter(|p| !now.contains(p.as_str())).cloned().collect();
        for p in gone {
            let abs = bb_core::util::below(root, &p);
            if plat::fs::lstat(&abs).is_some_and(|m| m.is_file()) {
                if !plat::delete(&abs) {
                    // in use right now: it stays remembered, the next sync tries again
                    continue;
                }
                stats.removed += 1;
            }
            known.files.remove(&p);
            // folders that became empty go too, up to the root
            let mut dir = p.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                if !plat::delete(&bb_core::util::below(root, parent)) {
                    break;
                }
                dir = parent;
            }
        }
    }
    // 3. fetch: small files in packs, large ones chunk by chunk
    let mut done = 0u64;
    progress(0, total);
    let (small, large): (Vec<&Entry>, Vec<&Entry>) = need.into_iter().partition(|e| e.chunks.len() <= 1);
    let mut group: Vec<&Entry> = Vec::new();
    let mut group_bytes = 0u64;
    let mut write_group = |group: &mut Vec<&Entry>, known: &mut Known, stats: &mut ApplyStats, done: &mut u64| -> Result<(), String> {
        // equal files share one chunk
        let mut by_id: BTreeMap<[u8; 32], Vec<&Entry>> = BTreeMap::new();
        for e in group.drain(..) {
            match e.chunks.first() {
                Some(id) => by_id.entry(*id).or_default().push(e),
                None => write_whole(root, e, &[], known, stats)?,
            }
        }
        let ids: Vec<[u8; 32]> = by_id.keys().copied().collect();
        fetch(relay, key, &ids, &mut |id, plain| {
            for e in by_id.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                if plain.len() as u64 != e.size {
                    return Err(format!("{}: the tree names a chunk of another size", e.path));
                }
                write_whole(root, e, &plain, known, stats)?;
                *done += e.size;
            }
            progress(*done, total);
            Ok(())
        })
    };
    for e in small {
        if !group.is_empty() && (group_bytes + e.size > (bb_core::PACK_MAX as u64) || group.len() >= 2000) {
            write_group(&mut group, known, &mut stats, &mut done)?;
            group_bytes = 0;
        }
        group_bytes += e.size;
        group.push(e);
    }
    write_group(&mut group, known, &mut stats, &mut done)?;
    for e in large {
        let abs = bb_core::util::below(root, &e.path);
        clear_way(root, &e.path, false);
        plat::mkdir(plat::fs::parent(&abs));
        let out = plat::AtomicFile::create(&abs).ok_or_else(|| format!("{} cannot be written", abs))?;
        for (n, pair) in e.chunks.chunks(3).enumerate() {
            let mut at: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
            for (i, id) in pair.iter().enumerate() {
                at.entry(*id).or_default().push(((n * 3 + i) * CHUNK) as u64);
            }
            let ids: Vec<[u8; 32]> = at.keys().copied().collect();
            fetch(relay, key, &ids, &mut |id, plain| {
                for off in at.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    // every chunk but the last is full, the last holds the rest: a tree that
                    // says otherwise would write beyond the file's size
                    if plain.len() as u64 != chunk_len(e.size, (*off / CHUNK as u64) as usize) {
                        return Err(format!("{}: the tree names a chunk of another size", e.path));
                    }
                    if out.write_at(*off, &plain) != Some(plain.len()) {
                        return Err(format!("{} cannot be written (disk full?)", abs));
                    }
                    done += plain.len() as u64;
                }
                progress(done, total);
                Ok(())
            })?;
        }
        if !out.commit() {
            return Err(format!("{} cannot be put in place", abs));
        }
        finish_file(&abs, e, known);
        stats.written += 1;
        stats.bytes += e.size;
    }
    for d in &tree.dirs {
        clear_way(root, d, true);
        plat::mkdir(&bb_core::util::below(root, d));
    }
    Ok(stats)
}

fn write_whole(root: &str, e: &Entry, data: &[u8], known: &mut Known, stats: &mut ApplyStats) -> Result<(), String> {
    let abs = bb_core::util::below(root, &e.path);
    // Before the write, not after a failed one: whatever of the wrong kind lies on the way goes
    // (a folder where a file belongs, a file where a folder belongs).  A link to a folder
    // would not fail at all -- the file would land wherever the link leads, outside the tree.
    clear_way(root, &e.path, false);
    if !bb_core::util::put_file(&abs, data) {
        return Err(format!("{} cannot be written (disk full, or the file is in use)", abs));
    }
    finish_file(&abs, e, known);
    stats.written += 1;
    stats.bytes += e.size;
    Ok(())
}
