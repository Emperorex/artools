//! Finding files with identical content (`--duplicates`).
//!
//! The search narrows candidates in three increasingly expensive stages, so
//! most files are never read and no file is read more than it has to be:
//!
//! 1. **Length.** Files with a unique length cannot have a twin. This costs
//!    nothing: the length comes from the directory scan.
//! 2. **Prefix hash.** Files that share a length are compared by a hash of
//!    their first [`PREFIX_LEN`] bytes. For files no longer than that, this
//!    already covers the whole file and the result is final.
//! 3. **Full hash.** Only files that still collide are read completely.
//!
//! Hashing is BLAKE3 (256 bit). Files are not additionally compared byte by
//! byte: a collision of a 256-bit hash is not a practical concern, but the
//! report is a snapshot, so a file modified while the search runs can still
//! be misreported. A file whose size changes while being read is detected
//! and skipped.
//!
//! Nothing here modifies the file system.

use crate::FileEntry;
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

/// How many leading bytes the cheap second stage hashes.
pub const PREFIX_LEN: u64 = 4096;

/// Read buffer per worker thread.
const READ_BUF_LEN: usize = 128 * 1024;

type Digest = [u8; 32];

/// A set of two or more files with identical content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// Logical length of each file, in bytes.
    pub len: u64,
    /// Paths of the identical files, sorted. Always at least two.
    pub paths: Vec<PathBuf>,
}

impl DuplicateGroup {
    /// Bytes that would be freed by keeping a single copy.
    pub fn reclaimable(&self) -> u64 {
        self.len * (self.paths.len() as u64 - 1)
    }
}

/// Counters describing how much work the search did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DuplicateStats {
    /// Files handed to the search.
    pub candidates: usize,
    /// Candidates that share their length with at least one other file.
    pub same_length: usize,
    /// Files whose prefix was hashed.
    pub prefix_hashed: usize,
    /// Files that were read completely in the third stage.
    pub full_hashed: usize,
    /// Total bytes read from disk.
    pub bytes_read: u64,
}

#[derive(Debug, Default)]
pub struct DuplicateReport {
    /// Duplicate groups, most reclaimable space first.
    pub groups: Vec<DuplicateGroup>,
    /// Files that could not be read (or changed while being read), with the
    /// reason. They take no part in the grouping.
    pub unreadable: Vec<(PathBuf, String)>,
    pub stats: DuplicateStats,
}

impl DuplicateReport {
    /// Total bytes that would be freed by keeping one copy of every group.
    pub fn reclaimable(&self) -> u64 {
        self.groups.iter().map(DuplicateGroup::reclaimable).sum()
    }

    /// Number of files that are redundant copies (all but one per group).
    pub fn redundant_files(&self) -> usize {
        self.groups.iter().map(|g| g.paths.len() - 1).sum()
    }
}

/// Finds groups of identical files among `candidates`.
///
/// `FileEntry::size` must hold each file's logical length. Candidates are
/// expected to be distinct regular files (the scan already collapses hard
/// links). `workers` is the number of hashing threads.
pub fn find_duplicates(candidates: Vec<FileEntry>, workers: usize) -> DuplicateReport {
    let mut report = DuplicateReport::default();
    report.stats.candidates = candidates.len();

    // Stage 1: group by length, drop unique lengths.
    let by_length = group_by_length(candidates);
    report.stats.same_length = by_length.iter().map(|(_, paths)| paths.len()).sum();

    // Stage 2: prefix hash.
    let mut jobs: Vec<Job> = Vec::with_capacity(report.stats.same_length);
    for (len, paths) in by_length {
        jobs.extend(paths.into_iter().map(|path| Job { len, path }));
    }
    report.stats.prefix_hashed = jobs.len();
    let prefix_groups = hash_and_regroup(jobs, Some(PREFIX_LEN), workers, &mut report);

    // Files no longer than the prefix were hashed completely: groups of
    // those are final. Longer ones need the full hash.
    let mut confirmed: Vec<(u64, Vec<PathBuf>)> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for (len, paths) in prefix_groups {
        if len <= PREFIX_LEN {
            confirmed.push((len, paths));
        } else {
            jobs.extend(paths.into_iter().map(|path| Job { len, path }));
        }
    }

    // Stage 3: full hash of the survivors.
    report.stats.full_hashed = jobs.len();
    confirmed.extend(hash_and_regroup(jobs, None, workers, &mut report));

    report.groups = confirmed
        .into_iter()
        .map(|(len, mut paths)| {
            paths.sort();
            DuplicateGroup { len, paths }
        })
        .collect();
    report.groups.sort_by(|a, b| {
        b.reclaimable()
            .cmp(&a.reclaimable())
            .then_with(|| b.len.cmp(&a.len))
            .then_with(|| a.paths[0].cmp(&b.paths[0]))
    });
    report.unreadable.sort();
    report
}

struct Job {
    len: u64,
    path: PathBuf,
}

/// Sorts candidates by (length, path) and keeps only runs of two or more.
/// Consumes the input so singletons are freed as early as possible.
fn group_by_length(mut candidates: Vec<FileEntry>) -> Vec<(u64, Vec<PathBuf>)> {
    candidates.sort_unstable_by(|a, b| a.size.cmp(&b.size).then_with(|| a.path.cmp(&b.path)));

    let mut groups: Vec<(u64, Vec<PathBuf>)> = Vec::new();
    let mut current: Option<(u64, Vec<PathBuf>)> = None;
    for FileEntry { size, path } in candidates {
        match &mut current {
            Some((len, paths)) if *len == size => paths.push(path),
            _ => {
                if let Some(done) = current.take()
                    && done.1.len() > 1
                {
                    groups.push(done);
                }
                current = Some((size, vec![path]));
            }
        }
    }
    if let Some(done) = current
        && done.1.len() > 1
    {
        groups.push(done);
    }
    groups
}

/// Hashes the first `max_bytes` (or all) bytes of every job in parallel, then
/// regroups by (length, hash) and returns the groups of two or more.
/// Unreadable or changed files are recorded in `report` and dropped.
fn hash_and_regroup(
    jobs: Vec<Job>,
    max_bytes: Option<u64>,
    workers: usize,
    report: &mut DuplicateReport,
) -> Vec<(u64, Vec<PathBuf>)> {
    let results = hash_jobs(&jobs, max_bytes, workers);

    let mut buckets: HashMap<(u64, Digest), Vec<PathBuf>> = HashMap::new();
    for (job, result) in jobs.into_iter().zip(results) {
        let expected = max_bytes.map_or(job.len, |m| job.len.min(m));
        match result {
            Ok((digest, read)) => {
                report.stats.bytes_read += read;
                if read == expected {
                    buckets.entry((job.len, digest)).or_default().push(job.path);
                } else {
                    report.unreadable.push((
                        job.path,
                        format!("file changed while reading ({read} of {expected} bytes)"),
                    ));
                }
            }
            Err(e) => report.unreadable.push((job.path, e.to_string())),
        }
    }

    buckets
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|((len, _), paths)| (len, paths))
        .collect()
}

/// Hashes every job on up to `workers` threads. The returned vector is
/// index-aligned with `jobs`.
fn hash_jobs(
    jobs: &[Job],
    max_bytes: Option<u64>,
    workers: usize,
) -> Vec<io::Result<(Digest, u64)>> {
    let mut results: Vec<Option<io::Result<(Digest, u64)>>> = Vec::new();
    results.resize_with(jobs.len(), || None);
    if jobs.is_empty() {
        return Vec::new();
    }

    let next = AtomicUsize::new(0);
    let limit = max_bytes.unwrap_or(u64::MAX);
    let threads = workers.clamp(1, jobs.len());

    thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut buf = vec![0u8; READ_BUF_LEN];
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        done.push((i, hash_file(&job.path, limit, &mut buf)));
                    }
                    done
                })
            })
            .collect();
        for handle in handles {
            for (i, result) in handle.join().unwrap() {
                results[i] = Some(result);
            }
        }
    });

    results
        .into_iter()
        .map(|r| r.expect("every job index is claimed by exactly one worker"))
        .collect()
}

/// Hashes up to `limit` bytes of `path`, returning the digest and the number
/// of bytes actually read.
fn hash_file(path: &Path, limit: u64, buf: &mut [u8]) -> io::Result<(Digest, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    while total < limit {
        let want = (limit - total).min(buf.len() as u64) as usize;
        match file.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((*hasher.finalize().as_bytes(), total))
}
