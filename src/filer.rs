use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: BTreeSet<PathBuf>,
    pub visible: Vec<Node>,
    pub selected: usize,
    pub filter: String,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root,
            expanded: BTreeSet::new(),
            visible: Vec::new(),
            selected: 0,
            filter: String::new(),
        };
        tree.rebuild();
        tree
    }

    pub fn rebuild(&mut self) {
        let mut out = Vec::new();
        collect(&self.root, 0, &self.expanded, &mut out);
        if !self.filter.is_empty() {
            let needle = self.filter.to_lowercase();
            out.retain(|n| n.is_dir || n.name.to_lowercase().contains(&needle));
        }
        self.visible = out;
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.visible.get(self.selected)
    }

    pub fn move_by(&mut self, delta: i64) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as i64;
        let next = (self.selected as i64 + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }

    pub fn top(&mut self) {
        self.selected = 0;
    }

    pub fn bottom(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
    }

    pub fn toggle(&mut self) {
        if let Some(node) = self.selected_node().cloned() {
            if node.is_dir {
                if !self.expanded.remove(&node.path) {
                    self.expanded.insert(node.path);
                }
                self.rebuild();
            }
        }
    }

    pub fn expand(&mut self) {
        if let Some(node) = self.selected_node().cloned() {
            if node.is_dir && !node.expanded {
                self.expanded.insert(node.path);
                self.rebuild();
            }
        }
    }

    /// Collapse the selected dir, or jump to the parent entry.
    pub fn collapse(&mut self) {
        if let Some(node) = self.selected_node().cloned() {
            if node.is_dir && node.expanded {
                self.expanded.remove(&node.path);
                self.rebuild();
            } else if node.depth > 0 {
                let mut i = self.selected;
                while i > 0 {
                    i -= 1;
                    if self.visible[i].depth < node.depth {
                        self.selected = i;
                        break;
                    }
                }
            }
        }
    }
}

fn collect(dir: &Path, depth: usize, expanded: &BTreeSet<PathBuf>, out: &mut Vec<Node>) {
    let mut entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        (!is_dir, e.file_name().to_string_lossy().to_lowercase())
    });
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_expanded = is_dir && expanded.contains(&path);
        out.push(Node {
            path: path.clone(),
            name,
            depth,
            is_dir,
            expanded: is_expanded,
        });
        if is_expanded {
            collect(&path, depth + 1, expanded, out);
        }
    }
}

/// Read-only preview: first ~200 lines of a text file, binary detection.
pub fn preview(path: &Path) -> Vec<String> {
    let mut buf = Vec::new();
    match fs::File::open(path) {
        Ok(mut f) => {
            let _ = f.by_ref().take(64 * 1024).read_to_end(&mut buf);
        }
        Err(e) => return vec![format!("<cannot open: {e}>")],
    }
    if buf.contains(&0) {
        return vec!["<binary file>".to_string()];
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .take(200)
        .map(|s| s.to_string())
        .collect()
}
