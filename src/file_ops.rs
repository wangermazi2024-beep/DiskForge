//! 真正会碰用户文件系统的两个操作：删除到回收站、打开系统"属性"对话框。
//! 单独拆一个模块（而不是塞进 ui/tree_list.rs），因为这两个是纯 Win32 调用，
//! 不依赖 egui，之后 compact_tree.rs（扩展名分类/重复文件查找）右键菜单
//! 想要同样的"删除"/"属性"时可以直接复用，不用再抄一遍。

/// 把一个文件/文件夹删除到回收站（不是永久删除）。
///
/// 用 `SHFileOperationW` + `FOF_ALLOWUNDO`，这是 Windows Shell 标准的"移到回收站"
/// 方式——`std::fs::remove_file`/`remove_dir_all` 是直接永久删除，不会经过回收站，
/// 绝对不能用在这里。`FOF_NOCONFIRMATION` 关掉系统自带的二次确认弹窗，是因为
/// 我们在应用自己的 UI 里已经有一个确认弹窗了，两层确认反而啰嗦；
/// `FOF_NOERRORUI` 关掉系统自带的错误弹窗，改成把错误原因返回给调用方，
/// 由应用自己的状态栏/弹窗统一展示，风格和其它报错保持一致。
///
/// `pFrom` 要求是"用 \0 分隔、以两个 \0 结尾"的路径列表，哪怕只删一个文件也要这么拼——
/// 这是 `SHFileOperationW` 从 Win32 API 设计之初就有的老接口约定，不这么拼会读到脏内存。
///
/// 已知局限：`SHFileOperationW` 是比较老的 Shell API，不支持超过 260 字符
/// （`MAX_PATH`）的长路径，也不支持 `\\?\` 长路径前缀（加了反而可能出错，
/// 这一点和 `find_locking_processes` 用的 Restart Manager 不一样，不能照搬
/// 同一个办法）——路径特别深的文件删除可能因此失败。真要支持长路径删除，
/// 得换成更新的 `IFileOperation` COM 接口（微软从 Vista 起推荐的替代方案），
/// 工作量不小，这次没有做，先记录在这里。
#[cfg(windows)]
pub fn delete_to_recycle_bin(path: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FO_DELETE, SHFILEOPSTRUCTW,
    };

    if path.is_empty() {
        return Err("路径为空".to_string());
    }
    // 双 \0 结尾的宽字符缓冲区。
    let mut from: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().collect();
    from.push(0);
    from.push(0);

    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let ret = unsafe { SHFileOperationW(&mut op) };
    crate::applog::log(&format!("[file_ops] 删除到回收站: {path} (ret={ret}, aborted={})", op.fAnyOperationsAborted));
    if ret != 0 {
        return Err(format!("删除失败（错误码 0x{ret:X}）"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("操作被取消".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn delete_to_recycle_bin(_path: &str) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

/// 打开系统原生的"属性"对话框。
///
/// 一开始用的是 `ShellExecuteW` 直接传 `"properties"` 谓词，看起来是最简单的
/// 写法、也是网上最常见的例子，但实测对 C:\TEST 这种普通文件夹会失败，返回值
/// 0x1f（`SE_ERR_NOASSOC`——"找不到关联的应用程序"）。原因是 `ShellExecuteW`
/// 的 `properties` 谓词走的是"按文件扩展名/ProgID 查注册表里登记的静态谓词"
/// 这条路，文件夹本身没有关联的"应用程序"，自然查不到；这也是为什么资源
/// 管理器右键"属性"这个功能，微软官方文档专门强调普通调用方式覆盖不到、
/// 必须用 `ShellExecuteExW` 配 `SEE_MASK_INVOKEIDLIST` 才行——这个标志让
/// Shell 改成通过目标的"快捷菜单处理器"（`IContextMenu`）去调用谓词，
/// 而不是查注册表里的静态关联，跟资源管理器右键菜单走的是同一条路，
/// 文件、文件夹、甚至没有关联程序的文件类型都能正常弹出属性对话框。
#[cfg(windows)]
pub fn open_properties(path: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};

    if path.is_empty() {
        return;
    }
    let verb: Vec<u16> = "properties".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();

    // 结构体里有个 hIcon/hMonitor union 字段，我们完全用不上（没设
    // SEE_MASK_ICON/SEE_MASK_HMONITOR），零初始化整个结构体最省事，不用去抠
    // union 具体叫什么名字——全零对这些字段（要么是数值 0，要么是空指针）
    // 都是合法值，不会有未定义行为。
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_INVOKEIDLIST;
    sei.hwnd = std::ptr::null_mut();
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = std::ptr::null();
    sei.lpDirectory = std::ptr::null();
    sei.nShow = 1; // SW_SHOWNORMAL；SEE_MASK_INVOKEIDLIST 弹的是对话框，这个值基本不影响什么，按惯例填。

    let ok = unsafe { ShellExecuteExW(&mut sei) };
    crate::applog::log(&format!("[file_ops] 打开属性对话框: {path} (ShellExecuteExW={ok})"));
    if ok == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        crate::applog::log(&format!("[file_ops] 打开属性对话框失败: {path} (GetLastError={err})"));
    }
}

#[cfg(not(windows))]
pub fn open_properties(_path: &str) {}

// ============================================================================
// 创建符号链接（去重 / 迁移到其他盘）
// ============================================================================
//
// 只用符号链接一种机制（硬链接/目录联接权衡后特意排除，理由见下面）。
// 命名约定是"内容寻址"：
//   - **文件**：真实数据搬到 `{目标目录}\{文件内容 BLAKE3 哈希}\{原文件名}`。
//     哈希当文件夹名是有意为之——同样内容的文件，不管什么时候、从哪个位置
//     迁移过来，算出来的哈希都一样，天然落到同一个文件夹里。这意味着"以后
//     再遇到同样内容的重复文件，哈希一算，发现这个文件夹已经存在，直接建
//     个链接指过去就行，不用重新复制一遍"——不需要另外维护一张"文件内容
//     去了哪"的索引表，文件夹名字本身就是索引，这是文件名 = 哈希这种设计
//     （Git、Docker 镜像层、好几个去重工具的持久化缓存都是这个思路）的
//     经典好处。
//   - **文件夹**：没有一个单一的"内容哈希"可以算（文件夹是一堆文件的集合，
//     不是一段连续字节流），所以用日期命名：`{目标目录}\{今天日期
//     YYYY-MM-DD}\{原文件夹名}`，日期给的是"这批是哪天迁移的"这个人类可读
//     的归档维度，重名会自动加序号，不会覆盖。
//
// 为什么只用符号链接、不用硬链接/目录联接：硬链接只能同盘、只能文件，
// 目录联接只能目录不能文件——不管选哪个都会出现"这次该用哪种机制"的分支，
// 对用户来说这个选择毫无意义（他们只关心"腾出空间/搬到别的盘"，不关心
// 底层是哪种链接）。符号链接一种机制覆盖文件和文件夹、能跨盘，三个场景
// （去重/迁移/去重+迁移）一套逻辑，没有"这次行为跟上次不一样"的困惑。
// 代价：目标那份数据被误删/误移动，链接会跟着断掉，这个风险目前只在文档里
// 提示，还没有做"防止误删链接目标"的额外保护（比如给目标文件加只读属性），
// 值得作为后续加固项。

use std::io::Read;
use std::path::Path;

/// 算一个文件的完整内容 BLAKE3 哈希（十六进制小写）。用来给内容寻址的目标
/// 文件夹命名——见模块顶部说明。
pub fn hash_file_blake3(path: &str) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(e) => return Err(format!("读取文件失败: {e}")),
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 创建符号链接。`link_path` 是新建的链接本身的路径（这个路径上必须还没有
/// 任何文件/文件夹，符号链接不能建在已存在的东西上面），`target_path` 是
/// 链接指向的真实位置。
///
/// 优先带上 `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE`——Win10 1703+
/// （创意者更新）开了"开发者模式"的话，不用管理员权限也能建符号链接；老
/// 系统不认这个 flag，会返回 `ERROR_INVALID_PARAMETER`，这时候去掉这一位
/// 重试（这次就要求管理员权限了）——这正是 Rust 标准库
/// `std::os::windows::fs::symlink_file` 处理这个新老系统兼容性问题的
/// 同一个办法，照抄它的思路。
#[cfg(windows)]
pub fn create_symlink(link_path: &str, target_path: &str, is_dir: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateSymbolicLinkW, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY,
    };

    let link_w: Vec<u16> = std::ffi::OsStr::new(link_path).encode_wide().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = std::ffi::OsStr::new(target_path).encode_wide().chain(std::iter::once(0)).collect();
    let base_flags: u32 = if is_dir { SYMBOLIC_LINK_FLAG_DIRECTORY } else { 0 };

    unsafe {
        let mut ok = CreateSymbolicLinkW(link_w.as_ptr(), target_w.as_ptr(), base_flags | SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE);
        if ok == 0 && GetLastError() == ERROR_INVALID_PARAMETER {
            // 老系统不认那个 flag（返回参数错误），去掉再试一次——这次没有
            // "免开发者模式"这个便利，需要管理员权限才能成功。
            ok = CreateSymbolicLinkW(link_w.as_ptr(), target_w.as_ptr(), base_flags);
        }
        if ok == 0 {
            let err = GetLastError();
            let hint = if err == 1314 {
                // ERROR_PRIVILEGE_NOT_HELD
                "（没有权限——请在「设置 > 系统 > 开发者选项」里打开开发者模式，或者以管理员身份运行本程序）"
            } else {
                ""
            };
            return Err(format!("创建符号链接失败（错误码 {err}）{hint}"));
        }
    }
    crate::applog::log(&format!("[file_ops] 创建符号链接: {link_path} -> {target_path}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn create_symlink(_link_path: &str, _target_path: &str, _is_dir: bool) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

/// 把 `path` 原地换成指向 `target` 的符号链接——`target` 处已经有真实数据了
/// （不需要再复制一遍），用于重复文件组里"真身"之外的其它副本：整组文件
/// 已经在检测阶段逐字节确认过内容完全一致（见 `dedup.rs`）。
///
/// ## 替换前的最后安全复查（TOCTOU 防护）
///
/// 从"重复文件检测完成"到"用户真的点下创建符号链接"之间可能隔了好几分钟，
/// 这期间某个副本完全可能被用户/其它程序改写过——不复查就把它删掉（进回收
/// 站）的话，新内容虽然可以在回收站找回，但路径上已经是链接了，用户找起来
/// 非常麻烦。这里在删除前用 [`crate::dedup::files_identical`] 做一次逐字节
/// 复查：长度先比（零 I/O 排除绝大多数），再全量比对；发现不一致就中止这
/// 一个副本的处理并报错，原文件一个字节都不会动。
///
/// 删除原文件/文件夹用的是"删到回收站"（`delete_to_recycle_bin_with_retry`），
/// 不是 `std::fs::remove_dir_all`/`remove_file`——这是这次专门修的一个数据
/// 安全问题：`remove_dir_all` 对文件夹是逐个文件/子文件夹分别调用删除，如果
/// 文件夹里某个文件正被占用导致中途失败，前面已经删掉的文件是永久丢失
/// （不经过回收站，找不回来），后面的又没删，文件夹变成不上不下的"半删除"
/// 状态——而当时真实数据已经安全复制到了目标位置，所以这种情况下源文件夹
/// 被破坏是完全没必要的风险。`SHFileOperationW`（`delete_to_recycle_bin`
/// 内部用的）对整个文件夹路径做的是一次"移动到回收站"，本质上是重命名/
/// 移动整个目录项，不需要目录里每一个文件都没被占用就能成功（NTFS 移动
/// 目录只改父目录的元数据，不用逐个打开里面的文件）——就算最终失败，
/// 回收站里也不会留下"半个文件夹"，原文件夹原地不动，等同于这一步什么都
/// 没发生，可以放心重试或者去处理占用后再试。
/// 代价：原始内容不是立刻彻底腾出磁盘空间，而是进了回收站，要用户后续
/// 清空回收站才会真正释放——用这点空间上的延迟换数据安全，这个取舍是
/// 值得的。
///
/// `verify_content`：是否做逐字节复查。重复文件组的副本走 `true`（检测到
/// 操作之间可能隔了几分钟）；`migrate_file_to_symlink`/`migrate_folder_to_symlink`
/// 走 `false`——它们的目标是几秒钟前刚复制出来并做过哈希校验的，复查等于
/// 把巨型文件再读两遍，白付一倍 I/O，TOCTOU 窗口也在亚秒级。
pub fn replace_with_symlink(path: &str, target: &str, is_dir: bool, verify_content: bool) -> Result<(), String> {
    // 文件夹符号链接跳过内容复查：文件夹是"一个位置的集合"，没有单一的
    // 内容可比（迁移文件夹时目标也是刚刚才镜像完的，窗口极小）；文件副本
    // 必须复查（见上面的 TOCTOU 说明）。
    if verify_content && !is_dir && !crate::dedup::files_identical(path, target) {
        let msg = format!(
            "副本 {path} 与真身 {target} 的内容已经不一致（可能在扫描之后被修改过），为防止丢失新内容已中止对这个副本的替换，原文件保留不动"
        );
        crate::applog::log(&format!("[file_ops] 替换符号链接前复查失败: {msg}"));
        return Err(msg);
    }
    delete_to_recycle_bin_with_retry(path)?;
    if let Err(e) = create_symlink(path, target, is_dir) {
        let msg = format!("原文件已删除（在回收站里，可以找回），但创建符号链接失败，真实数据在 {target}，请手动处理: {e}");
        crate::applog::log(&format!("[file_ops] {msg}"));
        return Err(msg);
    }
    Ok(())
}

/// 符号链接目标位置的根目录：`{drive}:\DiskForge`。所有迁移操作统一放在
/// 用户选定分区下的这一个固定目录里，不再让用户随便选任意文件夹——见
/// `mirrored_folder_target_path`/`migrate_file_to_symlink` 上的说明，
/// 目标路径本身就是"分区 + 固定目录 + 镜像原路径/内容哈希"拼出来的，
/// 换成任意目录会破坏这个可预期、可推导的结构。
pub fn diskforge_base_dir(drive_letter: char) -> String {
    format!("{}:\\DiskForge", drive_letter.to_ascii_uppercase())
}

/// 文件夹迁移的目标路径：把源路径的盘符换成一段路径（比如 `C:\Users\Bob\Foo`
/// 变成 `C\Users\Bob\Foo`），拼在 `base_dir`（`{目标盘}:\DiskForge`）后面，
/// 得到形如 `D:\DiskForge\C\Users\Bob\Foo` 的完整路径——盘符本身也保留成
/// 一段路径，是为了避免 `C:\Users\X` 和 `D:\Users\X` 两个不同来源的文件夹
/// 都映射到同一个 `DiskForge\Users\X`，互相冲突。
fn mirrored_folder_target_path(source_path: &str, base_dir: &str) -> Result<String, String> {
    let bytes = source_path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' {
        return Err(format!("无法识别的路径格式（不是标准的盘符路径，暂不支持迁移）: {source_path}"));
    }
    let drive = source_path.chars().next().unwrap().to_ascii_uppercase();
    let rest = source_path[2..].trim_start_matches('\\');
    if rest.is_empty() {
        return Err("不支持直接迁移整个分区根目录".to_string());
    }
    Ok(format!("{}\\{drive}\\{rest}", base_dir.trim_end_matches('\\')))
}

/// 把一个文件"内容寻址"迁移：真实数据搬到 `{base_dir}\{文件内容哈希}\{原
/// 文件名}`，原来的位置换成指向那里的符号链接。返回真实数据最终所在的
/// 完整路径。`base_dir` 是 `diskforge_base_dir()` 拼出来的固定目录。
///
/// 如果这个哈希对应的目标位置已经存在（之前迁移过同样内容的文件），不
/// 重复复制——但会重新校验一次目标内容的哈希，不能因为"文件夹名字对上了"
/// 就假设内容真的一样（万一是哈希碰撞，或者目标文件被手动改过），这是
/// 贯穿这个应用去重逻辑的同一个原则：判定"内容相同"必须有实打实的比对
/// 依据，不能靠命名约定去猜。
///
/// 安全顺序：先复制、再用哈希校验完整性，通过了才删除原文件、最后才建
/// 链接——绝不"先删后建"，复制/校验失败会保留原文件不动、清理掉复制出
/// 一半的残留，最多是这次迁移操作本身失败，不会丢数据、也不会在目标位置
/// 留垃圾。每一步失败都会写日志，方便事后在日志里查到底是哪一步失败的
/// （不只是弹一条状态提示，提示条几秒后会自动消失）。
pub fn migrate_file_to_symlink(source_path: &str, base_dir: &str) -> Result<String, String> {
    let hash = hash_file_blake3(source_path)?;
    let file_name = Path::new(source_path)
        .file_name()
        .ok_or_else(|| "无法解析文件名".to_string())?
        .to_string_lossy()
        .to_string();
    let target_dir = format!("{}\\{hash}", base_dir.trim_end_matches('\\'));
    let target_path = format!("{target_dir}\\{file_name}");

    if Path::new(&target_path).exists() {
        let existing_hash = hash_file_blake3(&target_path)?;
        if existing_hash != hash {
            let msg = format!("目标位置 {target_path} 已存在但内容不一致（理论上不应该发生），为安全起见中止操作，不会覆盖也不会删除原文件");
            crate::applog::log(&format!("[file_ops] 迁移文件失败: {source_path}: {msg}"));
            return Err(msg);
        }
        crate::applog::log(&format!("[file_ops] 内容寻址目标已存在且校验一致，直接复用: {target_path}"));
    } else {
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("创建目标目录失败: {e}"))?;
        if let Err(e) = std::fs::copy(source_path, &target_path) {
            let _ = std::fs::remove_file(&target_path);
            let _ = std::fs::remove_dir(&target_dir); // 刚建的目录这时候必然是空的，删不掉也无所谓（最多留一个空文件夹）
            let msg = format!("复制文件失败: {e}");
            crate::applog::log(&format!("[file_ops] 迁移文件失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
            return Err(msg);
        }
        let copied_hash = hash_file_blake3(&target_path)?;
        if copied_hash != hash {
            let _ = std::fs::remove_file(&target_path);
            let _ = std::fs::remove_dir(&target_dir);
            let msg = "复制后校验失败（内容对不上），已撤销复制，原文件未受影响".to_string();
            crate::applog::log(&format!("[file_ops] 迁移文件校验失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
            return Err(msg);
        }
    }

    // 刚复制+哈希校验完，不做第二次逐字节复查（verify_content=false）。
    if let Err(e) = replace_with_symlink(source_path, &target_path, false, false) {
        crate::applog::log(&format!("[file_ops] 迁移文件：复制+校验成功，但替换符号链接失败: {source_path} -> {target_path}: {e}"));
        return Err(e);
    }
    crate::applog::log(&format!("[file_ops] 迁移文件成功: {source_path} -> {target_path}"));
    Ok(target_path)
}

/// 把一个文件夹迁移：整个文件夹搬到"镜像原路径"的位置（见
/// `mirrored_folder_target_path`），原来的位置换成指向那里的符号链接。
///
/// 目标位置如果已经存在，直接失败中止，不会自动加 `(1)`/`(2)` 这样的序号
/// 后缀——目标路径是"盘符+完整原始路径"拼出来的，正常情况下同一个源文件夹
/// 只会对应唯一一个目标位置，如果这个位置已经存在，大概率意味着之前已经
/// 迁移过、或者出现了什么没预料到的情况，这时候应该让用户自己看一眼、
/// 自己决定怎么处理，而不是静默加个序号"绕过去"掩盖掉这个异常信号。
///
/// **不能用 `std::fs::rename` 做跨盘移动**——这是容易踩的坑：`rename` 在
/// Windows 上就是 `MoveFileExW`，官方文档明确说了"跨卷移动目录"这个操作
/// 不受任何 flag 支持（`MOVEFILE_COPY_ALLOWED` 能让*文件*跨卷时退化成
/// 复制+删除，但明确排除了目录），所以这里手写了递归复制
/// （`copy_dir_recursive`），复制完用文件数+总大小做一次完整性校验（没有
/// 逐文件比对内容——文件夹里文件一多，这个校验成本会失控，退而求其次用
/// "数量和大小都对得上"这个比第一版弱一些但足够拦住"复制中途失败/少复制
/// 了几个文件"的检查），校验通过才删除原文件夹、建链接；复制/校验失败会
/// 清理掉目标位置已经复制出来的残留，不留垃圾。
pub fn migrate_folder_to_symlink(source_path: &str, base_dir: &str) -> Result<String, String> {
    // 复制一个大文件夹可能要花不少时间——先用重命名探测一下这个文件夹本身
    // 有没有被占用，占用了就没必要先复制一遍、到最后一步删除源文件夹时
    // 才发现删不掉，白白浪费时间和磁盘空间。"无法确定"不当成"占用"处理——
    // 见 `check_folder_occupied_by_rename` 上的说明，权限不够/系统保护
    // 目录也会导致"无法确定"，不该因为这个就拦掉本来合法的操作，真的占用
    // 的话后面复制/删除阶段自然会失败并给出具体错误。
    match check_folder_occupied_by_rename(source_path) {
        FolderOccupancy::Free => {}
        FolderOccupancy::Locked => {
            let msg = "文件夹当前被占用（重命名探测：共享/锁冲突），已取消迁移，没有复制任何数据。建议先用右键菜单的\"检测占用\"找到占用的进程，处理完再重试。".to_string();
            crate::applog::log(&format!("[file_ops] 迁移文件夹前置检测发现被占用，已中止: {source_path}"));
            return Err(msg);
        }
        FolderOccupancy::Inconclusive(reason) => {
            crate::applog::log(&format!("[file_ops] 迁移文件夹前置检测结果不确定，继续尝试迁移: {source_path}: {reason}"));
        }
    }

    let target_path = mirrored_folder_target_path(source_path, base_dir)?;
    if Path::new(&target_path).exists() {
        let msg = format!("目标位置 {target_path} 已经存在，可能之前已经迁移过、或者出现了没预料到的情况——为安全起见中止操作，请手动检查这个位置后再重试");
        crate::applog::log(&format!("[file_ops] 迁移文件夹失败: {source_path}: {msg}"));
        return Err(msg);
    }
    // 迁移前先做一次 reparse point 预扫：源文件夹里如果存在符号链接/junction/
    // OneDrive 占位文件这类 reparse 子项，复制阶段会静默跳过它们（跳过是
    // 对的，跳进去了才会出事——死循环/把别处的内容重复计入），但"静默跳过
    // + 原文件夹整体进回收站"组合起来就是数据不完整：数量校验两边同跳恒
    // 通过，用户根本不知道少了东西。所以现在直接在动手之前中止，把决定权
    // 还给用户——这是审计确认过的"静默产出不完整结果"问题，宁可不做，
    // 也不做出一份缺东西的镜像。
    if let Some(offender) = find_reparse_entry(Path::new(source_path), source_path)
        .map_err(|e| format!("预扫源文件夹失败（未做任何改动）: {e}"))? {
        let msg = format!(
            "文件夹里包含符号链接/junction/OneDrive 占位项（{offender}），这类条目无法被安全地复制到别的盘——为避免迁出一份不完整的镜像，已中止迁移，原文件夹未受任何影响。可以先用资源管理器/检测占用处理这些条目后再迁移剩下的部分"
        );
        crate::applog::log(&format!("[file_ops] 迁移文件夹中止（发现 reparse 子项）: {source_path}: {offender}"));
        return Err(msg);
    }
    let dst = Path::new(&target_path);
    let Some(parent) = dst.parent() else {
        return Err(format!("无法解析目标路径的上级目录: {target_path}"));
    };
    std::fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败: {e}"))?;

    let src = Path::new(source_path);
    if let Err(e) = copy_dir_recursive(src, dst) {
        let _ = std::fs::remove_dir_all(dst);
        let msg = format!("复制文件夹失败: {e}");
        crate::applog::log(&format!("[file_ops] 迁移文件夹失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
        return Err(msg);
    }

    let (src_count, src_size) = count_dir(src).map_err(|e| format!("统计源文件夹失败: {e}"))?;
    let (dst_count, dst_size) = count_dir(dst).map_err(|e| format!("统计目标文件夹失败: {e}"))?;
    if src_count != dst_count || src_size != dst_size {
        let _ = std::fs::remove_dir_all(dst);
        let msg = format!(
            "复制后校验失败（源 {src_count} 个文件/{src_size} 字节，目标 {dst_count} 个文件/{dst_size} 字节），已撤销复制，原文件夹未受影响"
        );
        crate::applog::log(&format!("[file_ops] 迁移文件夹校验失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
        return Err(msg);
    }

    if let Err(e) = replace_with_symlink(source_path, &target_path, true, false) {
        crate::applog::log(&format!("[file_ops] 迁移文件夹：复制+校验成功，但替换符号链接失败: {source_path} -> {target_path}: {e}"));
        return Err(e);
    }
    crate::applog::log(&format!("[file_ops] 迁移文件夹成功: {source_path} -> {target_path}"));
    Ok(target_path)
}

/// 迭代式预扫：源文件夹里是否存在任何 reparse point（符号链接/junction/
/// OneDrive 占位文件等）。返回第一个发现的（相对说明用），没有则 Ok(None)。
/// 显式栈遍历（跟本项目其它遍历大树的地方同一套标准）：这个函数跑在后台
/// 线程的默认 2MB 栈上，原生递归在极深的目录树上会栈溢出。
#[cfg(windows)]
fn find_reparse_entry(root: &Path, root_display: &str) -> Result<Option<String>, std::io::Error> {
    use crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT;
    use std::os::windows::fs::MetadataExt;
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let attrs = entry.metadata().map(|m| m.file_attributes()).unwrap_or(0);
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                let name = entry.file_name().to_string_lossy().into_owned();
                let full = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
                return Ok(Some(format!("{root_display}\\{full}")));
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_rel = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
                stack.push((dir.join(entry.file_name()), child_rel));
            }
        }
    }
    Ok(None)
}
#[cfg(not(windows))]
fn find_reparse_entry(_root: &Path, _root_display: &str) -> Result<Option<String>, std::io::Error> {
    Ok(None)
}

/// 递归复制一个文件夹的全部内容（显式栈迭代版，不再原生递归——这个函数
/// 跑在后台线程的默认 2MB 栈上，几千层深的目录树就会栈溢出崩溃，而
/// `export.rs` 早就因为同样的原因改成了显式栈，同一项目里两套标准）。
/// 符号链接/reparse point 类型的子条目会被跳过（不递归进去、也不复制）——
/// 这类特殊条目处理不当容易出问题（比如复制到一个指向自己祖先目录的符号
/// 链接会死循环）；迁移入口现在会在动手前用 [`find_reparse_entry`] 把
/// 含 reparse 子项的文件夹整个拦下，这里是最后一道防线。
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut stack: Vec<(std::path::PathBuf, std::path::PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for entry in std::fs::read_dir(&src_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let dst_path = dst_dir.join(entry.file_name());
            if file_type.is_symlink() {
                continue;
            } else if file_type.is_dir() {
                std::fs::create_dir_all(&dst_path)?;
                stack.push((entry.path(), dst_path));
            } else if file_type.is_file() {
                std::fs::copy(entry.path(), &dst_path)?;
            }
        }
    }
    Ok(())
}

/// 统计一个文件夹递归下来的文件总数 + 总字节数，给 `migrate_folder_to_symlink`
/// 做复制后的完整性校验用。和 `copy_dir_recursive` 一样跳过符号链接；
/// 同样改成显式栈迭代（理由同上：后台线程默认栈只有 2MB）。
fn count_dir(path: &Path) -> std::io::Result<(u64, u64)> {
    let mut count = 0u64;
    let mut size = 0u64;
    let mut stack: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            } else if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                count += 1;
                size += entry.metadata()?.len();
            }
        }
    }
    Ok((count, size))
}

/// 创建符号链接成功后，重新读一次这个位置在磁盘上的最新状态（现在应该是
/// 一个符号链接了），构建一个新的 `Node` 用来在树里原地替换旧节点——旧节点
/// 存的是"迁移前的真实文件/文件夹"的大小/属性，迁移完之后这些数据已经
/// 过时（符号链接本身只占几十到几百字节，不是原来那么大）。
///
/// 时间戳/所有者/颜色偷懒沿用旧节点的值，没有重新去查——符号链接准确的
/// 修改时间对用户不重要，这里优先保证"大小对、类型对（能正确显示紫色 L
/// 徽标）"，不追求逐个字段都精确刷新，真要精确就得走一遍完整的重新扫描，
/// 这里只是"创建成功后的即时反馈"，不是替代重新扫描。
#[cfg(windows)]
pub fn build_refreshed_symlink_node(path: &str, name: &str, is_dir: bool, old: &crate::model::Node) -> crate::model::Node {
    use crate::model::Node;
    // 符号链接自身的"大小"是 CreateSymbolicLinkW 内部写进重解析数据里的
    // 目标路径长度，通常几十到几百字节；用 std::fs::symlink_metadata（不
    // 追踪链接，读链接自己的元数据，不是目标的）取一次，取不到就当 0——
    // 符号链接本来就不占什么实际空间，这个数字只是"看着对"用的，不是关键信息。
    let size = std::fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0);
    // 我们自己刚用 CreateSymbolicLinkW 建出来的，肯定是这个 tag，不需要再
    // 查一遍 Win32 API 确认。
    use crate::fs_attrs::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_SYMLINK};
    let attrs = FILE_ATTRIBUTE_REPARSE_POINT | if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
    let node = if is_dir {
        // 目录符号链接当叶子节点处理，没有子节点——和 scan.rs 扫描到已有的
        // 目录 reparse point 时的处理方式保持一致，不递归进去。
        Node::new_folder_with_meta(name, old.color, Vec::new(), old.modified_ft, old.created_ft, old.accessed_ft, attrs, IO_REPARSE_TAG_SYMLINK, false, old.owner.clone())
    } else {
        Node::new_file_with_meta(name, size, size, old.color, old.modified_ft, old.created_ft, old.accessed_ft, attrs, IO_REPARSE_TAG_SYMLINK, false, old.owner.clone())
    };
    match &old.full_path_override {
        Some(_) => node.with_full_path(path.to_string()),
        None => node,
    }
}

#[cfg(not(windows))]
pub fn build_refreshed_symlink_node(_path: &str, _name: &str, _is_dir: bool, old: &crate::model::Node) -> crate::model::Node {
    old.clone()
}

// ============================================================================
// 文件/文件夹占用检测 + 自动重试
// ============================================================================
//
// 背景：创建符号链接（去重/迁移到其他盘）的第一步通常是"先把原文件删掉"，
// 如果这个文件正被别的程序打开（哪怕只是被资源管理器选中预览、被某个编辑器
// 打开、被杀毒软件正在扫描），删除会直接失败——用户看到一个 Win32 错误码，
// 完全不知道是谁占用的、也不知道该怎么办。这里查了几种解决方案：
//
// 1. **Restart Manager API**（这次实现的）——Windows 官方提供、专门用来回答
//    "这个文件正被哪些进程/服务占用"这个问题的 API，Windows 资源管理器自己
//    删文件弹出"文件正在使用"对话框、Windows Installer 更新前检测占用，
//    用的都是这一套（`RmStartSession` → `RmRegisterResources` →
//    `RmGetList` → `RmEndSession`）。相比让用户自己去翻任务管理器猜，
//    直接告诉用户"被 XX.exe（PID 1234）占用"体验好得多。局限：它依赖
//    Windows 自己维护的"谁打开了这个文件"这份记录，绝大多数场景都能覆盖，
//    但个别驱动级别的极端占用方式可能查不到（下面第 3 点是补充方案）。
// 2. **自动重试**（这次也实现的）——很多占用其实是瞬时的：杀毒软件正在
//    扫描这个文件、索引服务刚碰了一下、资源管理器缩略图缓存正在读——这种
//    "转瞬即逝"的占用，等个几百毫秒重试一次往往就好了，不需要真的去查
//    是谁占用、也不需要用户介入。真正持续被占用（比如文件在 Word 里开着）
//    重试才会真的失败，这时候再查占用进程告诉用户。
// 3. **没有实现、但值得记录的备选方案**：
//    - `NtQuerySystemInformation`（`SystemHandleInformation`）遍历全系统
//      句柄表，逐个进程比对——这是 Sysinternals `handle.exe`/Process Explorer
//      背后的原理，比 Restart Manager 更底层、能查到的场景更全，但这是半
//      官方/未完全文档化的 API（微软没有正式承诺其稳定性），实现复杂度也
//      高得多（要枚举所有进程、对每个句柄查类型再解析成文件路径）。除非
//      发现 Restart Manager 在实际使用中确实有覆盖不到的场景，不建议为了
//      "更全"去换成这个更脆弱的方案。
//    - `MOVEFILE_DELAY_UNTIL_REBOOT`（`MoveFileExW` 的一个标志）——如果确认
//      是被占用、用户也不想等/不方便关掉占用它的程序，可以把删除操作注册成
//      "下次开机时执行"，这是 Windows Installer 处理"正在使用中的系统文件"
//      的经典手段。对我们的场景（用户主动去重/迁移）不算优先级很高的方案，
//      但作为"重试也不行、占用进程又是关键系统服务不方便强制关闭"时的兜底
//      选项，值得以后需要的时候加上。
//    - 直接调用 Restart Manager 的 `RmShutdown`/`RmRestart` 去强制关闭占用
//      该文件的应用——技术上可行（Windows Installer 就是这么干的），但这是
//      "未经用户明确同意就关掉人家正在用的程序"，用户体验和数据安全风险都
//      不小（比如强制关掉一个正在编辑但没保存的 Word 文档），这次没有做，
//      以后如果要做，必须先在 UI 上明确告知用户"将要关闭以下程序"并拿到
//      确认，不能静默执行。

/// 给一个绝对路径加上 `\\?\` 长路径前缀（UNC 网络路径是 `\\?\UNC\...`）。
///
/// Win32 API 默认限制路径不能超过 260 字符（`MAX_PATH`），超过就直接报错/
/// 找不到文件——这是几十年前遗留下来的老限制，不是 Restart Manager 特有的
/// 问题，但会表现成"这个文件明明存在却检测不到占用"，查资料确认了这是个
/// 已知的通用解决办法：加上 `\\?\` 前缀之后，Windows 会跳过传统路径解析，
/// 允许最长到 32767 字符。已经带前缀的路径原样返回，不重复加；已知的相对
/// 路径/非绝对路径场景在这个项目里不会出现（这里处理的都是从磁盘扫描出来
/// 的绝对路径），所以不处理相对路径转换。
fn to_extended_length_path(path: &str) -> String {
    if path.starts_with(r"\\?\") {
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else {
        format!(r"\\?\{path}")
    }
}

/// 占用当前文件/文件夹的一个进程（或服务）。
#[derive(Clone)]
pub struct LockingProcess {
    pub pid: u32,
    /// 服务返回服务的长名称，普通程序返回用户能看懂的程序名（不是可执行文件
    /// 路径），这是 Restart Manager 自己给出的"友好名称"。
    pub app_name: String,
    /// 如果这个"进程"其实是个 Windows 服务，这里是服务的短名字（可以用来
    /// 提示用户"net stop 这个服务名"或者去服务管理器里找）；不是服务就是
    /// `None`。
    pub service_name: Option<String>,
}

/// 查询哪些进程/服务正占用着给定的一批文件（或文件夹——文件夹本身一般不会
/// 被"占用"，但如果调用方想查文件夹下某个具体文件，直接传那个文件路径）。
///
/// 用的是 Windows 官方 Restart Manager API，见上面模块内的说明。查询本身
/// 不会关闭/打断任何进程，纯只读操作，随便调用没有副作用。
#[cfg(windows)]
pub fn find_locking_processes(paths: &[&str]) -> Result<Vec<LockingProcess>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };

    if paths.is_empty() {
        return Ok(Vec::new());
    }

    // Restart Manager 的会话 key 缓冲区。文档里这个长度是
    // `CCH_RM_SESSION_KEY + 1`（微软定义为 32+1），这里多给一些余量，不去
    // 依赖具体拿到的常量值是否精确对得上——缓冲区只嫌大不嫌小，Rm 只会往里
    // 写一个远小于缓冲区长度的 GUID 字符串。
    let mut session_key = [0u16; 64];
    let mut session_handle: u32 = 0;
    let start_ret = unsafe { RmStartSession(&mut session_handle, 0, session_key.as_mut_ptr()) };
    if start_ret != ERROR_SUCCESS {
        return Err(format!("RmStartSession 失败（错误码 {start_ret}）"));
    }
    // 不管中间哪一步失败，只要 session 开成功了就必须调用 RmEndSession 收尾，
    // 用一个"守卫"结构体保证——即使中途用 `?`/`return` 提前退出，Drop 也会
    // 把 session 关掉，不会泄漏 Restart Manager 的会话资源（一个用户会话
    // 同一时间最多只能开 64 个 Restart Manager 会话，用完不关迟早会把这个
    // 上限占满）。
    struct SessionGuard(u32);
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe { RmEndSession(self.0) };
        }
    }
    let _guard = SessionGuard(session_handle);

    // 过滤/规范化路径——这几类问题不处理的话，都会导致"明明没被占用却报错"
    // 或者"一个路径有问题、拖累整批检测全部失败"这种误报，而这份检测结果
    // 直接关系到"敢不敢删除/创建符号链接"，误报比漏报更危险（漏报顶多是没
    // 查出真实占用，误报会让用户误以为文件被占用而不敢操作，或者反过来
    // 误以为没占用其实检测根本没跑起来）：
    //   - 超过 260 字符（`MAX_PATH`）的路径，Win32 API 不加 `\\?\` 前缀的话
    //     会直接找不到文件/报错——这是 Win32 API 的老限制，不是 Restart
    //     Manager 特有的，但会表现成"这个文件检测不到占用"或者拖累整个
    //     `RmRegisterResources` 调用出错。前缀加不加对 Restart Manager
    //     没有副作用，统一加上最省心。
    //   - 路径为空字符串的（理论上不应该出现，但防御一下）直接跳过，不传
    //     给 Win32 API。
    let wide_paths: Vec<Vec<u16>> = paths
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let extended = to_extended_length_path(p);
            std::ffi::OsStr::new(&extended).encode_wide().chain(std::iter::once(0)).collect()
        })
        .collect();
    if wide_paths.is_empty() {
        return Ok(Vec::new());
    }
    let path_ptrs: Vec<*const u16> = wide_paths.iter().map(|p| p.as_ptr()).collect();

    let reg_ret = unsafe {
        RmRegisterResources(
            session_handle, path_ptrs.len() as u32, path_ptrs.as_ptr(),
            0, std::ptr::null(), 0, std::ptr::null(),
        )
    };
    if reg_ret != ERROR_SUCCESS {
        return Err(format!("RmRegisterResources 失败（错误码 {reg_ret}）"));
    }

    // 先用一个小缓冲区问一次"到底有多少个"（`RmGetList` 在缓冲区不够大时
    // 会告诉你实际需要多少），再按这个数字分配足够大的缓冲区正式取一次——
    // 这是 Restart Manager API 的标准用法（MSDN 示例、上面查到的开源实现
    // `LockCheck`/`.NET Matters` 专栏的写法都是这个套路）。
    let mut needed: u32 = 0;
    let mut got: u32 = 0;
    let mut reboot_reasons: u32 = 0;
    let first_ret = unsafe { RmGetList(session_handle, &mut needed, &mut got, std::ptr::null_mut(), &mut reboot_reasons) };
    // ERROR_MORE_DATA（234）是正常情况——第一次问的时候本来就没打算真的
    // 拿到列表，只是为了问一下 `needed` 是多少；`got` 传 0 意味着调用方
    // 提供的缓冲区容量是 0，所以只要注册了资源、有任何进程占用，这里几乎
    // 总会返回 ERROR_MORE_DATA。真的返回 ERROR_SUCCESS 说明没有任何进程
    // 占用（`needed` 会是 0）。
    // ERROR_MORE_DATA 用 windows-sys 的标准定义（以前本地重定义 234）。
    let error_more_data = windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    if first_ret != ERROR_SUCCESS && first_ret != error_more_data {
        return Err(format!("RmGetList（探测数量）失败（错误码 {first_ret}）"));
    }
    if needed == 0 {
        return Ok(Vec::new());
    }

    let mut buf: Vec<RM_PROCESS_INFO> = Vec::with_capacity(needed as usize);
    // `RM_PROCESS_INFO` 全部字段要么是定长数组要么是数值/句柄，全零是合法的
    // 初始状态（不含任何指针/需要析构的字段），`zeroed()` 安全。
    for _ in 0..needed {
        buf.push(unsafe { std::mem::zeroed() });
    }
    got = needed;
    let second_ret = unsafe { RmGetList(session_handle, &mut needed, &mut got, buf.as_mut_ptr(), &mut reboot_reasons) };
    if second_ret != ERROR_SUCCESS {
        return Err(format!("RmGetList（取列表）失败（错误码 {second_ret}）"));
    }

    let mut result = Vec::with_capacity(got as usize);
    for info in buf.iter().take(got as usize) {
        let app_name = String::from_utf16_lossy(&info.strAppName)
            .trim_end_matches('\0').to_string();
        let service_name = String::from_utf16_lossy(&info.strServiceShortName)
            .trim_end_matches('\0').to_string();
        result.push(LockingProcess {
            pid: info.Process.dwProcessId,
            app_name: if app_name.is_empty() { format!("(PID {})", info.Process.dwProcessId) } else { app_name },
            service_name: if service_name.is_empty() { None } else { Some(service_name) },
        });
    }
    Ok(result)
}

#[cfg(not(windows))]
pub fn find_locking_processes(_paths: &[&str]) -> Result<Vec<LockingProcess>, String> {
    Ok(Vec::new())
}

/// "重命名探测"文件夹占用检测的结论。不是简单的"占用/没占用"两种，因为
/// 重命名失败并不总是意味着"被占用"——见 `check_folder_occupied_by_rename`
/// 上的详细说明。
#[derive(Clone)]
pub enum FolderOccupancy {
    /// 确定没有被占用——重命名成功过（并且已经改回原名）。
    Free,
    /// 比较有把握地判定被占用——错误码明确是共享冲突/锁冲突。
    Locked,
    /// 没法确定——重命名失败，但错误码不是共享冲突/锁冲突（最常见的是
    /// 拒绝访问），可能是权限不够，也可能是系统保护目录，也可能确实被占用。
    Inconclusive(String),
}

/// 用"重命名探测"判断一个文件夹是否被占用——比对文件夹里成千上万个文件
/// 逐个查 Restart Manager 准得多、也快得多：Windows 底层重命名一个目录
/// 是纯粹的目录项元数据操作，不需要目录里的文件都没被打开就能成功（这也是
/// 为什么"删除原文件夹"改用"移到回收站"而不是逐文件删除的同一个原理，
/// 见 `replace_with_symlink` 上的说明）——只要文件夹本身（或者它内部某个
/// 不允许改动目录结构的东西）真的被别的进程占着，重命名就会失败；反过来，
/// 重命名成功就能确定这一刻没有东西占着它，立刻改回原名，就跟没发生过一样。
///
/// 重命名失败不一定就是"被占用"——也可能是权限不够（没有管理员权限），
/// 或者这是个系统保护的特殊目录（比如 `C:\Users`、`C:\Windows` 本身，
/// Windows 出于系统完整性考虑，即使有管理员权限往往也不允许重命名这几个
/// "已知文件夹"，但这不代表它们"被占用"）。这两种情况在 Win32 层面很可能
/// 报同一个错误码（`ERROR_ACCESS_DENIED`），没办法百分之百区分，所以只有
/// 错误码明确是 `ERROR_SHARING_VIOLATION`(32)/`ERROR_LOCK_VIOLATION`(33)
/// 这种"共享/锁冲突"专用错误码时，才判定为确定被占用；其它失败原因一律
/// 归类成"无法确定"，如实告诉用户，不武断地报告"占用"或"没占用"——这正是
/// 这次要修的问题：以前用"查文件夹里最多 2000 个文件"的方式，对 `C:\Users`
/// 这种大文件夹经常产生"没查到占用"的假阴性结论。
#[cfg(windows)]
pub fn check_folder_occupied_by_rename(path: &str) -> FolderOccupancy {
    let p = Path::new(path);
    let Some(parent) = p.parent() else {
        return FolderOccupancy::Inconclusive("无法解析上级目录".to_string());
    };
    let Some(file_name) = p.file_name() else {
        return FolderOccupancy::Inconclusive("无法解析文件夹名".to_string());
    };
    let file_name = file_name.to_string_lossy().to_string();
    // 临时名字里带个随机数（用当前时间的纳秒数凑数，不需要真正密码学级别的
    // 随机），避免连续检测、或者恰好已经有同名残留文件时撞车。
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!("{file_name}.diskforge_lockcheck_{nonce:08x}"));

    match std::fs::rename(p, &temp_path) {
        Ok(()) => {
            // 立刻改回来——这一步理论上不该失败（刚成功地把它从原名挪到了
            // 临时名，说明这一刻这个位置是完全可控的），万一真的失败了（比如
            // 极端时机下正好有别的进程冒出来抢占了原路径），也要把这个异常
            // 情况大声记下来，不能悄悄放过——文件夹现在顶着一个奇怪的临时
            // 名字，用户找不到自己的文件夹会很困惑，得让他知道去哪找。
            if let Err(e) = std::fs::rename(&temp_path, p) {
                let msg = format!(
                    "重命名探测完成后，改回原名失败！文件夹当前的实际路径是: {}，请手动把它改回「{file_name}」: {e}",
                    temp_path.display(),
                );
                crate::applog::log(&format!("[file_ops] {msg}"));
                return FolderOccupancy::Inconclusive(msg);
            }
            FolderOccupancy::Free
        }
        Err(e) => match e.raw_os_error() {
            Some(32) | Some(33) => FolderOccupancy::Locked,
            _ => FolderOccupancy::Inconclusive(format!(
                "重命名探测失败（{e}），可能是权限不足或者这是系统保护的特殊目录，也可能确实被占用，无法进一步确定"
            )),
        },
    }
}

#[cfg(not(windows))]
pub fn check_folder_occupied_by_rename(_path: &str) -> FolderOccupancy {
    FolderOccupancy::Inconclusive("当前平台不支持这项检测".to_string())
}

/// 把一批占用进程格式化成一行给用户看的文字，比如
/// "被 QQ.exe（PID 1234）、Everything.exe（PID 5678）占用"。
pub fn describe_locking_processes(procs: &[LockingProcess]) -> String {
    if procs.is_empty() {
        return String::new();
    }
    let names: Vec<String> = procs
        .iter()
        .map(|p| match &p.service_name {
            Some(svc) => format!("{}（服务 {svc}，PID {}）", p.app_name, p.pid),
            None => format!("{}（PID {}）", p.app_name, p.pid),
        })
        .collect();
    format!("被 {} 占用", names.join("、"))
}

/// 删除到回收站，遇到失败自动重试几次再放弃——很多占用是瞬时的（杀毒软件
/// 扫描、索引服务、缩略图缓存），没必要第一次失败就直接向用户报错。重试
/// 全部失败之后，尝试查一下到底是被谁占用的，把这个信息附加进错误消息里，
/// 而不是甩给用户一个看不懂的错误码——这是这次的主要目的：不只是"检测到
/// 占用"，而是直接告诉用户"是谁占用的"，方便用户去手动关掉。
pub fn delete_to_recycle_bin_with_retry(path: &str) -> Result<(), String> {
    const RETRIES: u32 = 3;
    let mut last_err = String::new();
    for attempt in 0..=RETRIES {
        match delete_to_recycle_bin(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt < RETRIES {
                    // 退避时间逐次拉长（300ms、600ms、900ms），给瞬时占用
                    // 更充分的时间自己解除，又不至于让用户等太久。
                    std::thread::sleep(std::time::Duration::from_millis(300 * (attempt as u64 + 1)));
                }
            }
        }
    }
    match find_locking_processes(&[path]) {
        Ok(procs) if !procs.is_empty() => {
            Err(format!("{last_err}；{}", describe_locking_processes(&procs)))
        }
        _ => Err(last_err), // 查不到占用进程（可能不是"占用"导致的失败，是别的原因），保留原始错误信息
    }
}

// ============================================================================
// 结束占用进程 / 停止占用服务
// ============================================================================
//
// "检测占用"查出来结果之后，光告诉用户"被谁占用"还不够方便——新手用户看到
// 一个进程名/服务名，未必知道该怎么去关掉它（服务尤其是，很多人不知道
// 服务管理器在哪）。这里加两个"一键处理"的操作，查完直接能点，处理完就
// 能回去继续删除/创建符号链接，不用中途切出去开任务管理器/服务管理器。
//
// 两个操作都是有一定风险的（结束进程可能导致未保存的工作丢失；停止服务
// 可能影响系统其它依赖这个服务的功能），调用方（UI 层）必须在真正调用前
// 给用户一个明确的二次确认，不能做成"点了列表里的按钮就立刻执行"——这两个
// 函数本身只管"执行"，确认逻辑是 UI 层的责任。

/// 结束一个进程（`TerminateProcess`，相当于任务管理器里的"结束任务"——
/// 是强制终止，不会给进程留时间做清理/保存工作，这也是为什么调用前必须
/// 让用户明确确认）。
#[cfg(windows)]
pub fn terminate_process(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    if pid == 0 {
        return Err("无效的 PID".to_string());
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("打开进程失败（错误码 {err}，常见原因是权限不够——试试以管理员身份运行）"));
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("结束进程失败（错误码 {err}）"));
        }
    }
    crate::applog::log(&format!("[file_ops] 已结束进程 PID {pid}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn terminate_process(_pid: u32) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

/// 停止一个 Windows 服务（`ControlService` + `SERVICE_CONTROL_STOP`，相当于
/// 服务管理器里右键"停止"）。`service_name` 要用服务的短名字（`sc query`/
/// 服务属性里的"服务名称"，不是显示名称）——`find_locking_processes` 返回的
/// `LockingProcess.service_name` 就是这个格式，直接传进来就行。
///
/// 只发停止请求、不等待/轮询服务真正停下来——`ControlService` 本身是异步的
/// （发出请求后服务需要时间处理），调用方如果需要"确认已经停了"，应该稍等
/// 一下再重新调用"检测占用"验证，而不是指望这个函数返回时服务已经停妥。
#[cfg(windows)]
pub fn stop_service(service_name: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
        SERVICE_CONTROL_STOP, SERVICE_STATUS, SERVICE_STOP,
    };

    if service_name.is_empty() {
        return Err("服务名为空".to_string());
    }
    let name_wide: Vec<u16> = std::ffi::OsStr::new(service_name).encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("打开服务控制管理器失败（错误码 {err}，常见原因是权限不够——试试以管理员身份运行）"));
        }
        let svc = OpenServiceW(scm, name_wide.as_ptr(), SERVICE_STOP);
        if svc.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            CloseServiceHandle(scm);
            return Err(format!("打开服务失败（错误码 {err}）"));
        }
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let ok = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        if ok == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            // 1051 = ERROR_DEPENDENT_SERVICES_RUNNING：有别的服务依赖这个服务，
            // 得先停依赖它的服务——这个场景给个专门的提示，比甩一个错误码有用。
            if err == 1051 {
                return Err("有其它服务依赖这个服务，需要先停掉那些服务（可以打开系统自带的\"服务\"管理器，找到这个服务，在\"依存关系\"标签页里看依赖它的服务有哪些）".to_string());
            }
            return Err(format!("停止服务失败（错误码 {err}）"));
        }
    }
    crate::applog::log(&format!("[file_ops] 已发送停止请求给服务: {service_name}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn stop_service(_service_name: &str) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}
