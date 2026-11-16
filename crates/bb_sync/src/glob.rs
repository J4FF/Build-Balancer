// glob -- patterns over relative paths ('/' between the parts)
//
//   path_match(pattern, path)   a part matches a part with shell wildcards (*, ?, [a-z]);
//                               "**" stands for any number of parts, also none
//   Ignore                      .gitignore rules as git reads them (the common part of them):
//                               comments, "!" to take back, a trailing "/" for folders only, a
//                               "/" in the pattern ties it to the folder of its file, without one
//                               it matches a name at any depth; the last rule that matches decides
//
// Both work on text only; the callers walk the folders.

use alloc::string::String;
use alloc::vec::Vec;

/// whether a path matches a pattern, part by part
pub fn path_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|p| !p.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    parts_match(&pat, &parts)
}

/// A table over (pattern parts left, path parts left), filled from the end: every pair is
/// looked at once, so a pattern with many "**" costs their product and not every way to deal
/// the path's parts out among them.
fn parts_match(pat: &[&str], parts: &[&str]) -> bool {
    let (np, nq) = (pat.len(), parts.len());
    // ok[j]: pat[i..] matches parts[j..], for the row i being filled; `next` is row i + 1
    let mut next = alloc::vec![false; nq + 1];
    next[nq] = true;
    for i in (0..np).rev() {
        let mut ok = alloc::vec![false; nq + 1];
        if pat[i] == "**" {
            // "**" takes nothing (row i + 1, same j) or one more part (this row, j + 1)
            ok[nq] = next[nq];
            for j in (0..nq).rev() {
                ok[j] = next[j] || ok[j + 1];
            }
        } else {
            for j in 0..nq {
                ok[j] = next[j + 1] && base::pattern::glob(pat[i], parts[j]);
            }
        }
        next = ok;
    }
    next[0]
}

/// the folder a pattern can match below: its parts before the first wildcard ("build/win" of
/// "build/win/*.exe"), "" when it starts with one
pub fn literal_prefix(pattern: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let parts: Vec<&str> = pattern.split('/').filter(|p| !p.is_empty()).collect();
    for (i, p) in parts.iter().enumerate() {
        // the last part names files, not a folder to walk
        if base::pattern::has_glob(p) || i + 1 == parts.len() {
            break;
        }
        out.push(p);
    }
    out.join("/")
}

/// Whether a folder can be the beginning of a path a pattern matches: "gen" and "gen/api" for
/// "*/api/*.rs" and "**/api/*.rs", not "src" for "gen/**".  Something of the pattern has to be
/// left to name what lies in the folder.
pub fn leads_into(pattern: &str, dir: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|p| !p.is_empty()).collect();
    // at[i]: the folder's parts so far are matched by pat[..i]; a "**" may also take none
    let skip_stars = |at: &mut Vec<bool>| {
        for i in 0..pat.len() {
            if at[i] && pat[i] == "**" {
                at[i + 1] = true;
            }
        }
    };
    let mut at = alloc::vec![false; pat.len() + 1];
    at[0] = true;
    skip_stars(&mut at);
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        let mut next = alloc::vec![false; pat.len() + 1];
        for i in 0..pat.len() {
            if at[i] && pat[i] == "**" {
                next[i] = true;
            } else if at[i] && base::pattern::glob(pat[i], part) {
                next[i + 1] = true;
            }
        }
        skip_stars(&mut next);
        at = next;
    }
    at[..pat.len()].iter().any(|x| *x)
}

struct Rule {
    /// the folder of the file the rule came from ("" = the root), with a trailing '/' unless empty
    base: String,
    pattern: String,
    negate: bool,
    dir_only: bool,
    anchored: bool,
}

impl Rule {
    fn parse(base: &str, line: &str) -> Option<Rule> {
        let l = line.trim_end_matches(['\r', '\n']);
        // trailing blanks do not count unless escaped; a pattern here never needs them
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            return None;
        }
        let (negate, l) = match l.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, l.strip_prefix('\\').filter(|r| r.starts_with(['#', '!'])).unwrap_or(l)),
        };
        let (dir_only, l) = match l.strip_suffix('/') {
            Some(r) => (true, r),
            None => (false, l),
        };
        let anchored = l.contains('/');
        let pattern = l.trim_start_matches('/');
        if pattern.is_empty() {
            return None;
        }
        Some(Rule { base: String::from(base), pattern: String::from(pattern), negate, dir_only, anchored })
    }

    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let Some(rel) = path.strip_prefix(self.base.as_str()) else { return false };
        if self.anchored {
            path_match(&self.pattern, rel)
        } else {
            base::pattern::glob(&self.pattern, rel.rsplit('/').next().unwrap_or(rel))
        }
    }
}

/// The ignore rules in force while a tree is walked: enter() adds a folder's ignore files,
/// leave() takes them away again.
#[derive(Default)]
pub struct Ignore {
    rules: Vec<Rule>,
    /// rules of the project's settings: they come last, so they win
    extra: Vec<Rule>,
    /// the include patterns of the settings once more, to find what they name inside ignored folders
    includes: Vec<Rule>,
}

impl Ignore {
    pub fn new() -> Ignore {
        Ignore::default()
    }

    /// rules from the project's settings: `exclude` patterns, then `include` patterns that take
    /// back what an ignore file or an exclude left out
    pub fn set_extra(&mut self, exclude: &[String], include: &[String]) {
        self.extra.clear();
        self.includes = include.iter().filter_map(|p| Rule::parse("", p)).collect();
        self.extra.extend(exclude.iter().filter_map(|p| Rule::parse("", p)));
        self.extra.extend(include.iter().filter_map(|p| Rule::parse("", p)).map(|mut r| {
            r.negate = !r.negate;
            r
        }));
    }

    /// add the rules of an ignore file that lies in `dir` ("" = the root) -> a mark for leave()
    pub fn mark(&self) -> usize {
        self.rules.len()
    }

    pub fn add_file(&mut self, dir: &str, text: &str) {
        let base = if dir.is_empty() { String::new() } else { alloc::format!("{}/", dir) };
        self.rules.extend(text.lines().filter_map(|l| Rule::parse(&base, l)));
    }

    pub fn leave(&mut self, mark: usize) {
        self.rules.truncate(mark);
    }

    /// whether an include pattern of the settings names this path itself
    pub fn included(&self, path: &str, is_dir: bool) -> bool {
        self.includes.iter().any(|r| r.matches(path, is_dir))
    }

    /// Whether an include pattern names something below this folder, so that the folder is
    /// worth walking although it is ignored.  Only patterns with a path in them count
    /// ("gen/api.rs", "out/**/*.h", "*/gen/*.rs"): a bare name like "*.h" would send the walk
    /// into every ignored folder, build output and all.
    pub fn include_reaches(&self, dir: &str) -> bool {
        self.includes.iter().any(|r| {
            let prefix = literal_prefix(&r.pattern);
            // the folder lies on the way to what the pattern names, or below the folder it names
            r.anchored && (leads_into(&r.pattern, dir) || !prefix.is_empty() && dir.starts_with(&alloc::format!("{}/", prefix)))
        })
    }

    /// whether a path is left out (the last rule that matches decides; none: it stays)
    pub fn ignored(&self, path: &str, is_dir: bool) -> bool {
        self.rules.iter().chain(self.extra.iter()).rev().find(|r| r.matches(path, is_dir)).is_some_and(|r| !r.negate)
    }
}
