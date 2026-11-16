// bb_sync -- a folder of files, moved between machines as encrypted blobs
//
//   glob      patterns over paths ("build/**/*.exe") and .gitignore rules
//   tree      Tree: the files of a folder with the ids of their content; Known: what a machine
//             remembers of files it hashed or wrote
//   scan      from a folder to its Tree (ignore rules, unchanged files are not read again)
//   transfer  upload a Tree's content to the relay, fetch a Tree, make a folder hold it
//
// The same three steps carry a project to a worker and a job's outputs back:
//
//   scan(project) -> upload -> TreeRef in the job -> fetch_tree -> apply(workspace)
//   expand(outputs) -> scan_files -> upload -> TreeRef in the result -> fetch_tree -> apply(project)
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod glob;
pub mod scan;
pub mod transfer;
pub mod tree;

pub use scan::{expand, scan, scan_files, Options, ScanStats};
pub use transfer::{apply, fetch_tree, need_bytes, upload, Apply, ApplyStats, UploadStats};
pub use tree::{Entry, Known, KnownFile, Tree};
