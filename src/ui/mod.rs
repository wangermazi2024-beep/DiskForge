pub mod compact_tree;
pub mod sidebar;
pub mod startup;
pub mod topbar;
pub mod tree_list;

use crate::model::NodePath;
use crate::search_index::NameIndex;

#[derive(Debug, Clone)]
pub enum TreeAction {
    None,
    Select(NodePath),
    ToggleExpand(NodePath),
    #[allow(dead_code)]
    EnterNode(NodePath),
    RequestDelete { abs_path: NodePath, name: String, full_path: String, is_folder: bool, index_entry: Option<u32> },
    RequestCheckLock { abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    RequestCheckLockGroup { abs_path: NodePath, name: String },
    RequestCreateSymlink { abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    RequestCreateSymlinkGroup { abs_path: NodePath, name: String },
    RequestRescan(usize),
    RequestRemovePartition(usize),
    RequestExtensionBreakdown(usize),
    RequestDuplicateFinder(usize),
}

fn chars_eq_ci(a: char, b: char) -> bool {
    let mut ai = a.to_lowercase();
    let mut bi = b.to_lowercase();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
}

fn contains_ignore_case(name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_chars: Vec<char> = needle.chars().collect();
    let Some(&first) = needle_chars.first() else { return true };
    'outer: for (start, c) in name.char_indices() {
        if !chars_eq_ci(c, first) {
            continue;
        }
        let mut nc = name[start + c.len_utf8()..].chars();
        for &n in &needle_chars[1..] {
            match nc.next() {
                Some(m) if chars_eq_ci(m, n) => {}
                _ => continue 'outer,
            }
        }
        return true;
    }
    false
}

fn fold_lowercase(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

pub enum Matcher {
    Plain(String),
    Regex(regex::Regex),
}

impl Matcher {
    #[allow(dead_code)]
    pub fn build(query: &str, use_regex: bool) -> Result<Self, String> {
        if use_regex {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map(Matcher::Regex)
                .map_err(|e| format!("正则表达式有误：{e}"))
        } else {
            Ok(Matcher::Plain(fold_lowercase(query)))
        }
    }

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
                Err(_) => Matcher::Plain(fold_lowercase(query)),
            }
        } else {
            Matcher::Plain(fold_lowercase(query))
        }
    }

    pub fn is_match(&self, name: &str) -> bool {
        match self {
            Matcher::Plain(q) => contains_ignore_case(name, q),
            Matcher::Regex(re) => re.is_match(name),
        }
    }

    pub fn find_in_index(&self, index: &NameIndex) -> Vec<u32> {
        match self {
            Matcher::Plain(q) => index.find_plain(q),
            Matcher::Regex(re) => index.find_regex(re),
        }
    }
}

pub use crate::model::{SortDir, SortKey, SortState};
