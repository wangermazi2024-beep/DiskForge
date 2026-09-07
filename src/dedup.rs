//! 真正基于内容的重复文件检测：大小分组 → header 哈希预筛 → 逐字节最终确认。
//!
//! # 这一版改了什么、为什么
//!
//! 前几版都是靠哈希值相等来判定"是重复文件"，问题是：**这是概率性的结论，
//! 不是绝对确定**——哪怕用 256 位的 BLAKE3/SHA-256，理论上依然存在哈希碰撞
//! 的可能（虽然现实中这个概率低到可以忽略，但"极低概率"和"绝对不会"终究是
//! 两回事）。这个应用后面要接"创建符号链接"这种不可逆操作（把重复文件里的
//! 其它几份删掉，只留一份、其余全部指向它），前提必须是**确定文件内容完全
//! 一样**，不能靠"哈希相同、大概率一样"这种说法——这一版把最终确认阶段从
//! "算个哈希、比较哈希值"换成了**逐字节比较**，不再依赖任何哈希算法的碰撞
//! 概率，是真正意义上的"一模一样"。
//!
//! 具体做法：header 预筛（读开头一小段算 BLAKE3 哈希）还是保留——这一步只是
//! 用来快速缩小候选范围，筛掉的文件本来就已经不一样了（读的那一小段内容都
//! 不同），不存在"哈希碰撞导致误判"的风险，误判的方向也无所谓（顶多是漏筛，
//! 让不必要的文件进入下一步，不会把"其实不同"的文件误判成"相同"）。真正
//! 决定"是不是重复文件"这个结论的，只有最后那步逐字节比较，不看任何哈希值。
//!
//! # 逐字节比较会不会更慢？——不会，而且更简单
//!
//! 直觉上"多做一步比较"应该更慢，但实际上逐字节比较（`memcmp` 量级的操作）
//! 比算 BLAKE3 这种密码学级哈希函数更快——哈希函数要做大量位运算/置换，
//! 逐字节比较只是拿内存里两段数据做对比，是 CPU 里最快的操作之一。同一个
//! header 分组里的文件，第一个文件的内容会缓存在内存里（不超过 64MB 的话），
//! 后面每个文件只需要流式读一遍、边读边跟内存里缓存的内容比对，读到不一样
//! 的地方立刻停手，不用等读完整个文件——这也是 Beyond Compare 官方技术支持
//! 文档里提到的道理："逐字节比较可以在发现第一个不同字节的地方就提前退出"。
//! 换句话说：这一步不仅更"绝对确定"，通常还更快。
//!
//! # 进度条为什么之前看起来是"一格一格跳"、不像主列表那么顺滑
//!
//! 之前是按"处理满 500 个就上报一次"来节流的，这样上报的数字永远是 500 的
//! 整数倍（134500、135000、135500……），一眼就能看出是"凑出来的"，不是真实
//! 进度——主列表扫描（`scan.rs`）的进度之所以看起来自然，是因为它上报的是
//! "此刻真实处理到的个数"，只是不去凑整数关口，数字本身该是多少就是多少。
//! 这一版改成按时间节流（大约每 50 毫秒上报一次，接近屏幕刷新率），每次上报
//! 的都是当时真实处理到的个数，不会再有那种"整百整千往上跳"的假象。

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// header 预筛读取的字节数。64KB 是从主流去重工具的经验值里取的：
/// 小到几乎不增加 I/O 成本，大到足够刷掉绝大多数假阳性。
const WINDOW_BYTES: usize = 64 * 1024;

/// 逐字节确认阶段：一个"内容哈希分组"里的"代表文件"（第一个）如果不超过
/// 这个大小，就整份读进内存缓存起来，后面同组的其它文件只需要流式读一遍、
/// 边读边跟内存里的内容比对，不用每比较一个文件就把代表文件重新读一遍磁盘。
/// 64MB 覆盖了绝大多数常见的"重复文件"场景（安装包、DLL、图片、文档、
/// 中小型视频……），比这个还大的文件退化成"两边都流式读、同步分块比较"，
/// 正确性不受影响，只是这种情况下 I/O 量会略高一些（这种巨型文件本来数量
/// 就少，对总耗时影响很小）。
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// 全部工作线程"代表文件内容缓存"的总预算。单个代表文件受 [`MAX_CACHE_BYTES`]
/// 限制还不够——极端场景（比如几千个 64MB 的同头互异大文件）下，每组各缓存
/// 一份就可能把内存吃爆（以前没有这个总闸）。超过预算后新的代表文件不再
/// 缓存，退化成"两边都流式读"，正确性不受影响，只是这种罕见情况下慢一些。
const TOTAL_CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// 当前已被所有线程缓存的代表文件总字节数（跨线程共享的预算计数器）。
static TOTAL_CACHED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 文件 I/O 缓冲区大小（哈希/逐字节比较共用）。256KB 是大多数机械盘/SSD
/// 顺序读的性价比拐点，再大收益递减。
const IO_BUF_BYTES: usize = 256 * 1024;

/// 判断两个文件内容是否**逐字节完全一致**——给"创建符号链接"前的最后
/// 安全复查用（`file_ops::replace_with_symlink`）：从重复文件检测完成到
/// 用户真的点下"创建符号链接"可能隔了好几分钟，这期间某个副本完全可能
/// 被用户/其它程序改写过，不复查就删掉它（进回收站）会丢掉新内容。
/// 返回 false 时调用方必须中止替换，保留原文件。
pub fn files_identical(a_path: &str, b_path: &str) -> bool {
    // 先比长度（零 I/O 就能排除绝大多数"扫描后被改过"的文件）。
    match (std::fs::metadata(a_path), std::fs::metadata(b_path)) {
        (Ok(a), Ok(b)) if a.len() != b.len() => return false,
        (Ok(_), Ok(_)) => {}
        _ => return false, // 任一侧拿不到元数据，一律当"不一致"保守处理
    }
    files_equal(a_path, b_path, None)
}

/// 当前在跑哪个阶段——用来在进度回调里区分"预筛"和"最终确认"，两个阶段各自
/// 独立计数（各自从 0 到各自的 100%），不会共用一个计数器导致进度条在阶段
/// 切换时"卡在 100% 不动"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashPhase {
    /// 第一步：读文件开头一小段算哈希，快速排除"大小相同但内容一开始就不同"
    /// 的假阳性——这一步只用来缩小候选范围，不参与"是不是重复文件"这个
    /// 最终结论。
    Prefilter,
    /// 第二步：逐字节比较，确认到底是不是真的一模一样。数据重复率越高，
    /// 这一步要处理的文件越多、耗时占比越大。
    Confirm,
}

/// 一组确认内容完全一致的文件（逐字节比较过，不是哈希碰巧相同）。
/// `file_indices` 是调用方传进来的 `paths` 切片里的下标。
pub struct DuplicateGroup {
    pub size: u64,
    /// 组里"代表文件"（第一个）的 BLAKE3 哈希，仅供展示/日志用——文件小于
    /// [`MAX_CACHE_BYTES`] 时这个值是"顺手"算出来的（反正内容已经读进内存
    /// 缓存了，多算一次哈希基本不花额外时间），不是判定重复与否的依据，
    /// 文件太大没缓存的话就是 `None`。
    pub hash_hex: Option<String>,
    pub file_indices: Vec<usize>,
}

/// 主入口：给一批"已经按大小分好组"的候选文件，跑完整个确认流程。
///
/// `size_groups`：`(文件大小, 下标列表)`，下标指向 `paths`；调用方负责先按
/// 大小分组、只把组内 >= 2 个文件的分组传进来。
///
/// `on_progress(phase, done, total)` 按时间节流（约每 50ms 一次），`done`/
/// `total` 是"当前这个阶段"真实处理到的个数，不是凑整数关口凑出来的。
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

    // ---- 阶段一：header 哈希预筛（BLAKE3，读开头 WINDOW_BYTES 字节）----
    // 只用来缩小候选范围，不参与最终"是不是重复"的结论——见模块顶部说明。
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
        // 拿不到哈希（读取失败：权限不够/文件被占用/扫描之后文件被删了之类）的
        // 直接跳过，不参与任何一组——宁可漏判候选，也不能瞎猜。
    }
    let confirm_groups: Vec<(u64, Vec<usize>)> = by_header
        .into_iter()
        .filter(|(_, idxs)| idxs.len() >= 2)
        .map(|((size, _h), idxs)| (size, idxs))
        .collect();

    // ---- 阶段二：逐字节最终确认（不是哈希碰巧相同，是真的比过内容）----
    run_confirm_stage(&pool, confirm_groups, paths, on_progress)
}

/// 阶段二：把每个 header 分组扔进线程池，逐字节验证组内文件是不是真的一模
/// 一样（`verify_identical`），组与组之间并行、互不影响。
///
/// ## 为什么中间多了一步"全量哈希预聚类"（以前没有）
///
/// 以前这里直接把 header 分组丢给 `verify_identical`：每个新文件依次跟组内
/// 所有已有"代表文件"逐字节比较，直到找到匹配的为止。最坏情况（一个组里有
/// 一万个"开头 64KB 相同但后续互异"的文件）下，第 k 个新文件要跟 k-1 个
/// 代表文件各比一遍——O(k²) 次全文读取，实测几小时都跑不完，而且整个过程
/// 只有一个组的线程在忙，进度条几乎不动。
///
/// 现在拆成两小步：先把组内每个文件**全量 BLAKE3 哈希**并行算一遍（每个文件
/// 只读一次），按哈希预聚类；哈希相同的子组再进入逐字节最终确认。哈希只是
/// 预筛，最终结论仍然是逐字节比较（跟整个模块"不依赖哈希碰撞概率"的原则
/// 一致）；总 I/O 从 O(k²) 降到 O(k)，极端场景从几小时变成几分钟。
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
        // 每个任务需要的是"下标 + 真实路径"这些自己独立的数据（克隆出来，
        // 不能借用 `paths`——线程池的任务要求 `'static`，`paths` 活不了那么
        // 长，见 `run_stage` 里同样的处理方式）。
        let files: Vec<(usize, String)> = idxs.iter().map(|&i| (i, paths[i].clone())).collect();
        let file_count = idxs.len() as u64;
        let tx = tx.clone();
        pool.execute(move || {
            // worker panic 兕底：绝不能因为单个任务 panic 就既不发消息又杀死
            // 工作线程——收集端 `recv()` 是按任务数收的，少一条就永远挂住，
            // UI 上表现为"正在比对内容…"永远转圈。panic 时发空结果（该组
            // 结果缺失，总好过整个流程挂死），panic 本身由全局 panic hook
            // （main.rs 里 install_panic_logger）记录进日志。
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
        // 按时间节流，不是按凑整的计数节流——见模块顶部"进度条为什么之前
        // 看起来一格一格跳"的说明。50ms 大约对应 20fps，肉眼看起来是连续的，
        // 又不会因为"每个文件都上报一次"造成几十万次 channel 发送的开销。
        if received == n_jobs as u64 || last_report.elapsed() >= Duration::from_millis(50) {
            on_progress(HashPhase::Confirm, done, total);
            last_report = Instant::now();
        }
    }
    results
}

/// 对同一个 header 分组里的文件做逐字节确认，返回真正内容一致的子分组
/// （一个 header 分组理论上可能因为哈希碰撞混进内容不同的文件，虽然概率
/// 极低，但既然目的就是"不依赖哈希碰撞概率"，这里老老实实处理这种情况，
/// 不是简单假设"header 哈希一样就全组都一样"）。
///
/// 做法分两小步：
///
/// 1. **全量哈希预聚类**：每个文件全量读一遍算 BLAKE3，按哈希把组内文件
///    先分成若干"疑似同内容"的子组（每个文件只读一次，总 I/O O(k)，
///    不会像以前那样 O(k²) 地反复重读代表文件——见 `run_confirm_stage`
///    顶部的说明）；
/// 2. **逐字节最终确认**：每个疑似同内容的子组内用 [`files_equal`] 逐字节
///    比较。哈希碰撞的文件会在这里被拆开（哈希只预筛，结论不看哈希），
///    返回的分组保证是逐字节确认过的。
///
/// 代表文件如果不太大（见 [`MAX_CACHE_BYTES`]，且全进程缓存总量没超过
/// [`TOTAL_CACHE_BUDGET_BYTES`]）会整份读进内存缓存，避免后续每次比较都
/// 重新从磁盘读一遍代表文件的内容。
fn verify_identical(files: Vec<(usize, String)>) -> Vec<(Vec<usize>, Option<String>)> {
    if files.len() < 2 {
        return Vec::new();
    }

    // ---- 第 1 小步：全量哈希预聚类（每文件只读一次，流式哈希，不缓存内容）----
    // 内容不缓存是刻意的：极端场景一个组里能有几千个 64MB 级的大文件，全部
    // 缓在内存里会把进程吃爆（旧实现只缓存代表文件，也是这个顾虑）；这里
    // 只算哈希，等聚类完知道谁是代表文件了，再由 [`read_if_cacheable`] 按
    // 全局预算缓存代表文件本身。
    let mut hashed: Vec<(usize, String, [u8; 32])> = Vec::with_capacity(files.len());
    for (idx, path) in files {
        let Some(hash) = hash_file_full(&path) else {
            // 读不了（权限不够/被删了/被占用）：跟 header 预筛阶段同一个原则，
            // 直接跳过，不参与任何一组——宁可漏判，也不能瞎猜。
            continue;
        };
        hashed.push((idx, path, hash));
    }

    let mut by_hash: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (order, (_, _, hash)) in hashed.iter().enumerate() {
        by_hash.entry(*hash).or_default().push(order);
    }

    // ---- 第 2 小步：逐字节最终确认（哈希只预筛，结论不看哈希）----
    let mut out: Vec<(Vec<usize>, Option<String>)> = Vec::new();
    // 按哈希排序输出，保证同样输入下结果顺序稳定可复现（HashMap 遍历序是随机的）。
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
        // 组内第一个文件作代表，其余逐个跟它字节比较。哈希相同时这一步
        // 几乎总是全部通过（开销只是把每个文件再读一遍）；真有碰撞的
        // 话会在这里被拆开/剔除，保证结论不依赖哈希。
        if byte_verify_cluster(&cluster) {
            let hex = blake3::Hash::from_bytes(hash).to_hex().to_string();
            out.push((cluster.into_iter().map(|(i, _)| i).collect(), Some(hex)));
        }
    }
    out
}

/// 全量流式读一个文件、算 BLAKE3 哈希。只算哈希，不缓存内容（为什么不做
/// 内容缓存见 `verify_identical` 第 1 小步的说明）。读不了返回 None。
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

/// 对一个"哈希预聚类后的子组"做逐字节确认：组内第一个文件作代表，其余
/// 全部跟它逐字节一致才返回 true（有任何不一致就整组作废——正常情况
/// 不会发生，只有哈希碰撞才会，宁可漏判也不误报"内容相同"）。
fn byte_verify_cluster(cluster: &[(usize, String)]) -> bool {
    if cluster.len() < 2 {
        return false;
    }
    let (_, rep_path) = &cluster[0];
    // 代表文件的内容缓存：这里重新读一次（上面预聚类时的缓存在 `hashed`
    // 里，不跨函数传了，简单优先——预聚类+确认合计每个文件也就读两遍）。
    let rep_cache = read_if_cacheable(rep_path);
    for (_, path) in cluster.iter().skip(1) {
        if !files_equal(path, rep_path, rep_cache.as_deref()) {
            return false;
        }
    }
    true
}

/// 把一个文件读进内存（不超过 [`MAX_CACHE_BYTES`] 且总预算允许时），
/// 给逐字节确认当代表文件缓存用。
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

/// 判断 `path` 和 `rep_path` 两个文件内容是否完全一致。`rep_cache` 是
/// `rep_path` 的内容缓存（如果调用方已经读进内存了的话）——有缓存的话只需要
/// 流式读 `path` 一遍、边读边跟内存比对；没有缓存（代表文件太大）就两边都
/// 流式读、同步分块比较。两种路径都是读到第一个不一样的地方就立刻返回
/// `false`，不用等读完整个文件（前提是文件确实不同——如果两个文件真的完全
/// 一样，这个"提前退出"用不上，该读多少还是得读多少，这是逐字节比较没法
/// 绕开的基本工作量，见模块顶部说明）。
///
/// 注意：**先比较文件长度**——长度都不一样，内容必然不一样，不用碰 I/O。
fn files_equal(path: &str, rep_path: &str, rep_cache: Option<&[u8]>) -> bool {
    // 长度快速排除：两个独立 open 前先比 metadata（绝大多数"大小相同但内容
    // 不同"的文件在这一步就能便宜地排除掉一部分跨文件比较的 I/O）。
    // 两个文件大小必然相同（header 预筛按大小分组过），这里只是防御。
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

/// 尽量把 `buf` 填满：反复 `read`，直到缓冲区满了或者真的读到文件末尾。
/// 单次 `read` 不保证能填满整个缓冲区（取决于操作系统一次给多少），两个独立
/// 文件的单次 `read` 返回的字节数完全可能对不上——如果不做这个"填满"处理，
/// 直接拿两次独立 `read` 的结果去比较，会把"这次操作系统只给了我 4KB、下次
/// 给了 60KB"这种正常情况误判成"内容不一样"，是逐字节比较正确性上必须处理
/// 的一个细节。
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

/// 把一批 `(下标, 文件大小)` 任务丢进线程池并行跑 `f(path, size)`，收集结果。
///
/// 结果通过 `mpsc` 通道收集，不是"把结果数组切片分给各线程各写各的"那种静态
/// 分片——静态分片要求提前知道每个任务耗时差不多才能均衡负载，但这里各文件
/// 大小、位于磁盘哪个位置都不一样，耗时差异可能很大；线程池 + 共享队列 +
/// 通道收集结果是"谁先干完谁去拿下一个任务"，天然做负载均衡。
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
            // worker panic 兜底：panic 时发 None（收集端把这条当"读不到，跳过"
            // 处理——语义与文件打开失败一致），保证收集端按任务数收满，不会
            // 因为少一条消息而永远挂住（那样 UI 会永远转圈）。
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path, size)))
                .unwrap_or(None);
            let _ = tx.send((idx, r));
        });
    }
    drop(tx);

    // 按时间节流上报进度（约 50ms 一次），不是按凑整的计数节流——见模块顶部
    // "进度条为什么之前看起来一格一格跳"的说明；上报的 `done` 是此刻真实
    // 处理到的个数，不会是刻意凑出来的整百整千。
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

/// 读文件里从 `offset` 开始的 `len` 字节，算 BLAKE3 哈希，只取前 8 字节转成
/// `u64` 当分组 key——这一步只是"预筛"，缩小候选范围用，不是最终确认结果，
/// 不需要完整 256 位。真正判定"是不是重复文件"靠的是 [`verify_identical`]
/// 的逐字节比较，不是这里的哈希。
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

/// 线程数：CPU 核心数 × 2（和项目里 `scan.rs` 目录扫描用的公式一致）。
/// 这批工作大部分时间在等磁盘 I/O、不是在算哈希/比较，线程数比核心数多一些
/// 能让"一个线程在等磁盘返回数据"的空隙被别的线程用来干活，不会白白空转。
///
/// 这个公式对 SSD 合适，机械硬盘（HDD）上不一定：fclones 作者自己都说过
/// "磁盘随机访问延迟是主要瓶颈"，机械硬盘上开太多线程意味着磁头在不同文件
/// 之间来回跳着读，物理寻道的开销可能比"多线程重叠等待时间"省下来的还多。
/// 如果发现在机械硬盘上线程数越多反而越慢，先手动把这个公式换成固定的小
/// 数字（比如 2~4）试试，比继续加线程更可能有效果。
fn worker_thread_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_mul(2).max(2)
}

/// 极简线程池，只用标准库、不引入新依赖。设计基本照抄《Rust 程序设计语言》
/// 官方教程"多线程 Web 服务器"那一章的实现——是网上被验证过最多次的 std-only
/// 线程池写法，没有用什么冷门技巧，正确性容易推理。工作线程常驻，整个
/// [`find_duplicates`] 调用期间反复复用来跑预筛/逐字节确认这两个阶段的
/// 任务，不是"来一批任务就创建一批线程、跑完就销毁"（早期版本慢的头号
/// 原因就是这个，见 git 历史/之前几版的说明）。
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
                // 锁只在 recv 时持有（job 在锁外执行），理论上不会被 job 的
                // panic 毒化；用 into_inner 容错 poisoning，不让任何一个
                // worker 的意外 panic 把其它 worker 一起带崩。
                let job = {
                    let guard = receiver.lock().unwrap_or_else(|p| p.into_inner());
                    guard.recv()
                };
                match job {
                    Ok(job) => {
                        // job 的 panic 不允许杀死 worker 线程（否则线程池悄悄
                        // 缩编，极端情况下所有 worker 都死掉、后续任务永远
                        // 没人执行）。panic 本身由全局 panic hook 记录进日志。
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
