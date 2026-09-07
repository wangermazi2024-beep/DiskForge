//! 搜索框的匹配规则：普通子串包含（大小写不敏感），或者正则表达式（同样大小写
//! 不敏感）。原本定义在 `ui/mod.rs` 里（bin crate），现在挪到 lib crate 是因为
//! `search_index.rs` 的扁平索引并行搜索（`FlatIndex::search`）也需要用同一套
//! 匹配规则——`ui` 模块本身依赖 `egui`，且只属于 bin target（`main.rs` 里的
//! `mod ui;` 是私有的、bin-only 的），lib crate（`search_index.rs` 所在的地方）
//! 没法引用它。`Matcher` 本身只依赖 `regex`，不涉及任何 GUI 绘制逻辑，挪到
//! lib crate 没有额外代价，还让"查找"、"搜索文件"（`ui/tree_list.rs`，走 bin）
//! 和 `search_index.rs`（走 lib）三处彻底共用同一份匹配实现，不会出现"改了一处、
//! 另一处行为悄悄不一致"的问题。
//!
//! `ui/mod.rs` 里保留一个 `pub use crate::matcher::Matcher;` 做转发，
//! 所有原来 `crate::ui::Matcher` 的调用点不用改。

#[derive(Clone)]
pub enum Matcher {
    Plain(String),
    Regex(regex::Regex),
}

impl Matcher {
    /// `query` 为空字符串时不应该调用这个函数（调用方在外面用
    /// `!query.trim().is_empty()` 判断"是否处于搜索模式"），这里不重复检查。
    pub fn build(query: &str, use_regex: bool) -> Result<Self, String> {
        if use_regex {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map(Matcher::Regex)
                .map_err(|e| format!("正则表达式有误：{e}"))
        } else {
            Ok(Matcher::Plain(query.to_lowercase()))
        }
    }

    /// "查找"悬浮窗用：不需要用户开任何开关，自动识别通配符——查询词里带
    /// `*`（匹配任意长度的任意字符）或 `?`（匹配单个任意字符）就自动按
    /// 通配符模式匹配整个名字（比如 `*.pid` 只匹配以 `.pid` 结尾的文件），
    /// 没有这两个字符就还是最简单的大小写不敏感包含匹配。这个函数是
    /// 不会失败的（通配符转出来的正则由 `regex::escape` 逐段拼接、`*`/`?`
    /// 换成 `.*`/`.`，构造上保证一定能编译成功），不像 `build()` 那样要处理
    /// 用户手写正则可能写错的情况——"查找"就是要简单，不给用户暴露"正则
    /// 语法错误"这种需要额外 UI 展示的复杂状态。
    pub fn build_auto(query: &str) -> Self {
        if query.contains('*') || query.contains('?') {
            let mut pattern = String::from("(?i)^");
            for ch in query.chars() {
                match ch {
                    '*' => pattern.push_str(".*"),
                    '?' => pattern.push('.'),
                    c => pattern.push_str(&regex::escape(&c.to_string())),
                }
            }
            pattern.push('$');
            match regex::Regex::new(&pattern) {
                Ok(re) => Matcher::Regex(re),
                // 理论上到不了这里（上面的构造方式保证一定合法），万一真的
                // 出了没预料到的问题，退化成普通包含匹配兜底，不让"查找"
                // 直接失效。
                Err(_) => Matcher::Plain(query.to_lowercase()),
            }
        } else {
            Matcher::Plain(query.to_lowercase())
        }
    }

    pub fn is_match(&self, name: &str) -> bool {
        match self {
            // 旧实现是 `name.to_lowercase().contains(q)`：每比较一次就对 `name`
            // 做一次 `to_lowercase()`，也就是一次堆分配 + 拷贝。百万级文件、
            // 每次搜索都要比较全部条目的场景下，这个分配本身就是"查找偶尔卡一下"
            // 的主因之一——换成不分配的大小写不敏感子串匹配（`ascii_ci_contains`），
            // 单次比较的开销从"分配+拷贝+contains"降到"纯扫描"，配合
            // `search_index.rs` 的并行扫描，两者叠加是这次优化的核心。
            //
            // 注意：`q` 在 `build`/`build_auto` 里已经用 `to_lowercase()` 转过
            // 一次，且只转一次（构造 `Matcher` 时，不是每次匹配时），所以这里
            // 拿到的 `q` 已经是小写，只需要把 `name` 的每个字符按需转成小写
            // 再比较，不需要真的分配一份转换后的新字符串。
            Matcher::Plain(q) => ascii_ci_contains(name, q),
            Matcher::Regex(re) => re.is_match(name),
        }
    }
}

/// 大小写不敏感的子串包含判断，不做任何堆分配。
///
/// 文件名/路径绝大多数场景是 ASCII 为主（含少量中文等非 ASCII 字符也没关系——
/// 非 ASCII 字符按字节直接比较，中文本身没有"大小写"概念，直接按原字节比较
/// 是正确的；只有 ASCII 字母才需要做大小写折叠）。这个假设和标准库
/// `str::eq_ignore_ascii_case` 的语义完全一致，覆盖了文件名匹配的绝大多数
/// 真实场景，同时避免了 Unicode 大小写折叠那一套更复杂、更慢的规则。
///
/// 实现上按字节滑动窗口比较，`O(name.len() * needle.len())` 最坏情况下并不比
/// `to_lowercase()+contains` 更"高级"，但没有中间分配——对文件名这种通常几十
/// 字节长的短字符串，分配本身的固定开销（不是复杂度）才是大头，去掉分配比
/// 优化算法复杂度收益更直接。
fn ascii_ci_contains(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    'outer: for start in 0..=(h.len() - n.len()) {
        for i in 0..n.len() {
            // `n[i]` 已经保证是小写（构造 `Matcher` 时转换过一次）；`h[start+i]`
            // 现场折叠成小写再比较，避免分配整份转换后的字符串。
            if h[start + i].to_ascii_lowercase() != n[i] {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_case_insensitive_contains() {
        let m = Matcher::build("readme", false).unwrap();
        assert!(m.is_match("README.md"));
        assert!(m.is_match("my_ReadMe_file.txt"));
        assert!(!m.is_match("license.txt"));
    }

    #[test]
    fn plain_empty_query_matches_everything() {
        let m = Matcher::build("", false).unwrap();
        assert!(m.is_match("anything.exe"));
    }

    #[test]
    fn wildcard_suffix() {
        let m = Matcher::build_auto("*.pid");
        assert!(m.is_match("server.pid"));
        assert!(!m.is_match("server.pid.old"));
    }

    #[test]
    fn wildcard_single_char() {
        let m = Matcher::build_auto("a?c");
        assert!(m.is_match("abc"));
        assert!(!m.is_match("ac"));
    }

    #[test]
    fn non_ascii_name_matches_by_raw_bytes() {
        let m = Matcher::build("报告", false).unwrap();
        assert!(m.is_match("2024年度报告.docx"));
        assert!(!m.is_match("2024年度总结.docx"));
    }
}
