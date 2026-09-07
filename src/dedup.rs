
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WINDOW_BYTES: usize = 64 * 1024;

const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

const TOTAL_CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

static TOTAL_CACHED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const IO_BUF_BYTES: usize = 256 * 1024;

pub fn files_identical(a_path: &str, b_path: &str) -> bool {
    match (std::fs::metadata(a_path), std::fs::metadata(b_path)) {
        (Ok(a), Ok(b)) if a.len() != b.len() => return false,
        (Ok(_), Ok(_)) => {}
        _ => return false,
    }
    files_equal(a_path, b_path, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashPhase {
    Prefilter,
    Confirm,
}

pub struct DuplicateGroup {
    pub size: u64,
    pub hash_hex: Option<String>,
    pub file_indices: Vec<usize>,
}

pub fn find_duplicates(
    paths: &[String],
    size_groups: Vec<(u64, Vec<usize>)>,
    on_progress: &dyn Fn(HashPhase, u64, u64),
) -> Vec<DuplicateGroup> {
    let total: u64 = size_groups.iter().map(|(_, idxs)| idxs.len() as u64).sum();
    if total == 0 {
        return Vec::new();
    }
    let pool = WorkerPool::new(worker_thread_count());

    let header_jobs: Vec<(usize, u64)> = size_groups
        .iter()
        .flat_map(|(size, idxs)| idxs.iter().map(move |&i| (i, *size)))
        .collect();
    let header_hashes = run_stage(&pool, &header_jobs, paths, HashPhase::Prefilter, on_progress, |path, size| {
        hash_window(path, 0, (size as usize).min(WINDOW_BYTES))
    });

    let mut by_header: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for &(idx, size) in &header_jobs {
        if let Some(&h) = header_hashes.get(&idx) {
            by_header.entry((size, h)).or_default().push(idx);
        }
    }
    let confirm_groups: Vec<(u64, Vec<usize>)> = by_header
        .into_iter()
        .filter(|(_, idxs)| idxs.len() >= 2)
        .map(|((size, _h), idxs)| (size, idxs))
        .collect();

    run_confirm_stage(&pool, confirm_groups, paths, on_progress)
}

fn run_confirm_stage(
    pool: &WorkerPool,
    groups: Vec<(u64, Vec<usize>)>,
    paths: &[String],
    on_progress: &dyn Fn(HashPhase, u64, u64),
) -> Vec<DuplicateGroup> {
    let total: u64 = groups.iter().map(|(_, idxs)| idxs.len() as u64).sum();
    let mut results = Vec::new();
    if total == 0 {
        return results;
    }
    let n_jobs = groups.len();
    let (tx, rx) = mpsc::channel::<(u64, u64, Vec<(Vec<usize>, Option<String>)>)>();
    for (size, idxs) in groups {
        let files: Vec<(usize, String)> = idxs.iter().map(|&i| (i, paths[i].clone())).collect();
        let file_count = idxs.len() as u64;
        let tx = tx.clone();
        pool.execute(move || {
            let clusters = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                verify_identical(files)
            }))
            .unwrap_or_default();
            let _ = tx.send((size, file_count, clusters));
        });
    }
    drop(tx);

    let mut done = 0u64;
    let mut last_report = Instant::now();
    for received in 1..=n_jobs as u64 {
        if let Ok((size, file_count, clusters)) = rx.recv() {
            done += file_count;
            for (file_indices, hash_hex) in clusters {
                if file_indices.len() >= 2 {
                    results.push(DuplicateGroup { size, hash_hex, file_indices });
                }
            }
        }
        if received == n_jobs as u64 || last_report.elapsed() >= Duration::from_millis(50) {
            on_progress(HashPhase::Confirm, done, total);
            last_report = Instant::now();
        }
    }
    results
}

fn verify_identical(files: Vec<(usize, String)>) -> Vec<(Vec<usize>, Option<String>)> {
    if files.len() < 2 {
        return Vec::new();
    }

    let mut hashed: Vec<(usize, String, [u8; 32])> = Vec::with_capacity(files.len());
    for (idx, path) in files {
        let Some(hash) = hash_file_full(&path) else {
            continue;
        };
        hashed.push((idx, path, hash));
    }

    let mut by_hash: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (order, (_, _, hash)) in hashed.iter().enumerate() {
        by_hash.entry(*hash).or_default().push(order);
    }

    let mut out: Vec<(Vec<usize>, Option<String>)> = Vec::new();
    let mut cluster_order: Vec<[u8; 32]> = Vec::new();
    for (hash, orders) in &by_hash {
        if orders.len() >= 2 {
            cluster_order.push(*hash);
        }
    }
    cluster_order.sort_unstable();
    for hash in cluster_order {
        let orders = &by_hash[&hash];
        let cluster: Vec<(usize, String)> = orders.iter().map(|&o| (hashed[o].0, hashed[o].1.clone())).collect();
        if byte_verify_cluster(&cluster) {
            let hex = blake3::Hash::from_bytes(hash).to_hex().to_string();
            out.push((cluster.into_iter().map(|(i, _)| i).collect(), Some(hex)));
        }
    }
    out
}

fn hash_file_full(path: &str) -> Option<[u8; 32]> {
    let mut f = File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF_BYTES];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => { hasher.update(&buf[..n]); }
            Err(_) => return None,
        }
    }
    Some(*hasher.finalize().as_bytes())
}

fn byte_verify_cluster(cluster: &[(usize, String)]) -> bool {
    if cluster.len() < 2 {
        return false;
    }
    let (_, rep_path) = &cluster[0];
    let rep_cache = read_if_cacheable(rep_path);
    for (_, path) in cluster.iter().skip(1) {
        if !files_equal(path, rep_path, rep_cache.as_deref()) {
            return false;
        }
    }
    true
}

fn read_if_cacheable(path: &str) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_CACHE_BYTES {
        return None;
    }
    if TOTAL_CACHED_BYTES.load(std::sync::atomic::Ordering::Relaxed) >= TOTAL_CACHE_BUDGET_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    TOTAL_CACHED_BYTES.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
    Some(data)
}

fn files_equal(path: &str, rep_path: &str, rep_cache: Option<&[u8]>) -> bool {
    match (std::fs::metadata(path), std::fs::metadata(rep_path)) {
        (Ok(a), Ok(b)) if a.len() != b.len() => return false,
        _ => {}
    }
    match rep_cache {
        Some(cached) => {
            let Ok(mut f) = File::open(path) else { return false };
            let mut pos = 0usize;
            let mut buf = vec![0u8; IO_BUF_BYTES];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => return pos == cached.len(),
                    Ok(n) => {
                        if pos + n > cached.len() || cached[pos..pos + n] != buf[..n] {
                            return false;
                        }
                        pos += n;
                    }
                    Err(_) => return false,
                }
            }
        }
        None => {
            let (Ok(mut fa), Ok(mut fb)) = (File::open(path), File::open(rep_path)) else { return false };
            let mut buf_a = vec![0u8; IO_BUF_BYTES];
            let mut buf_b = vec![0u8; IO_BUF_BYTES];
            loop {
                let na = match read_fill(&mut fa, &mut buf_a) {
                    Ok(n) => n,
                    Err(_) => return false,
                };
                let nb = match read_fill(&mut fb, &mut buf_b) {
                    Ok(n) => n,
                    Err(_) => return false,
                };
                if na != nb || buf_a[..na] != buf_b[..nb] {
                    return false;
                }
                if na == 0 {
                    return true;
                }
            }
        }
    }
}

fn read_fill(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

fn run_stage<T, F>(
    pool: &WorkerPool,
    jobs: &[(usize, u64)],
    paths: &[String],
    phase: HashPhase,
    on_progress: &dyn Fn(HashPhase, u64, u64),
    f: F,
) -> HashMap<usize, T>
where
    T: Send + 'static,
    F: Fn(&str, u64) -> Option<T> + Send + Sync + 'static,
{
    let n = jobs.len();
    let mut out = HashMap::with_capacity(n);
    if n == 0 {
        return out;
    }
    let f = Arc::new(f);
    let (tx, rx) = mpsc::channel::<(usize, Option<T>)>();
    for &(idx, size) in jobs {
        let path = paths[idx].clone();
        let tx = tx.clone();
        let f = Arc::clone(&f);
        pool.execute(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path, size)))
                .unwrap_or(None);
            let _ = tx.send((idx, r));
        });
    }
    drop(tx);

    let mut done = 0u64;
    let mut last_report = Instant::now();
    for received in 1..=n as u64 {
        if let Ok((idx, r)) = rx.recv()
            && let Some(v) = r {
                out.insert(idx, v);
            }
        done += 1;
        if received == n as u64 || last_report.elapsed() >= Duration::from_millis(50) {
            on_progress(phase, done, n as u64);
            last_report = Instant::now();
        }
    }
    out
}

fn hash_window(path: &str, offset: u64, len: usize) -> Option<u64> {
    if len == 0 {
        return Some(0);
    }
    let mut f = File::open(path).ok()?;
    if offset > 0 {
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(offset)).ok()?;
    }
    let mut buf = vec![0u8; len];
    let mut total = 0usize;
    while total < len {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => return None,
        }
    }
    let hash = blake3::hash(&buf[..total]);
    let bytes = hash.as_bytes();
    Some(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn worker_thread_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_mul(2).max(2)
}

struct WorkerPool {
    sender: Option<mpsc::Sender<Job>>,
    workers: Vec<thread::JoinHandle<()>>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

impl WorkerPool {
    fn new(size: usize) -> Self {
        let size = size.max(1);
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);
        for _ in 0..size {
            let receiver = Arc::clone(&receiver);
            workers.push(thread::spawn(move || loop {
                let job = {
                    let guard = receiver.lock().unwrap_or_else(|p| p.into_inner());
                    guard.recv()
                };
                match job {
                    Ok(job) => {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                    Err(_) => break,
                }
            }));
        }
        Self { sender: Some(sender), workers }
    }

    fn execute<F: FnOnce() + Send + 'static>(&self, f: F) {
        if let Some(s) = &self.sender {
            let _ = s.send(Box::new(f));
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        drop(self.sender.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
