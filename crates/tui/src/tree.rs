use std::collections::BTreeSet;

use torrential_core::{FileRisk, TorrentFile};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Folder { end: usize },
    File(usize),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Node {
    pub depth: u16,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    All,
    Some,
    None,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Tree {
    pub files: Vec<TorrentFile>,
    pub nodes: Vec<Node>,
    pub picked: Vec<bool>,
    pub folded: BTreeSet<usize>,
    pub cursor: usize,
    pub scroll: usize,
}

#[derive(Default)]
struct Dir {
    name: String,
    children: Vec<Entry>,
}

enum Entry {
    Dir(Dir),
    File(usize),
}

impl Dir {
    fn insert(&mut self, parts: &[&str], index: usize) {
        match parts {
            [] => {}
            [_file] => self.children.push(Entry::File(index)),
            [dir, rest @ ..] => {
                let existing = self.children.iter_mut().find_map(|c| match c {
                    Entry::Dir(d) if d.name == *dir => Some(d),
                    _ => None,
                });
                match existing {
                    Some(d) => d.insert(rest, index),
                    None => {
                        let mut d = Dir { name: (*dir).to_string(), children: Vec::new() };
                        d.insert(rest, index);
                        self.children.push(Entry::Dir(d));
                    }
                }
            }
        }
    }

    fn flatten(self, depth: u16, files: &[TorrentFile], nodes: &mut Vec<Node>) {
        let at = nodes.len();
        nodes.push(Node { depth, name: self.name, kind: NodeKind::Folder { end: 0 } });
        for child in self.children {
            match child {
                Entry::Dir(d) => d.flatten(depth + 1, files, nodes),
                Entry::File(i) => {
                    nodes.push(Node { depth: depth + 1, name: leaf(&files[i].path), kind: NodeKind::File(i) })
                }
            }
        }
        nodes[at].kind = NodeKind::Folder { end: nodes.len() };
    }
}

fn leaf(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

impl Tree {
    pub fn new(name: &str, files: Vec<TorrentFile>) -> Self {
        let mut nodes = Vec::new();
        if let [only] = files.as_slice()
            && !only.path.contains('/')
        {
            nodes.push(Node { depth: 0, name: only.path.clone(), kind: NodeKind::File(0) });
        } else {
            let mut root = Dir { name: name.to_string(), children: Vec::new() };
            for (i, f) in files.iter().enumerate() {
                root.insert(&f.path.split('/').collect::<Vec<_>>(), i);
            }
            root.flatten(0, &files, &mut nodes);
        }
        let picked = files.iter().map(|f| f.risk.is_none()).collect();
        Self { files, nodes, picked, folded: BTreeSet::new(), cursor: 0, scroll: 0 }
    }

    fn subtree(&self, node: usize) -> std::ops::Range<usize> {
        match self.nodes[node].kind {
            NodeKind::Folder { end } => node + 1..end,
            NodeKind::File(_) => node..node + 1,
        }
    }

    fn files_under(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.subtree(node).filter_map(|n| match self.nodes[n].kind {
            NodeKind::File(i) => Some(i),
            NodeKind::Folder { .. } => None,
        })
    }

    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut n = 0;
        while n < self.nodes.len() {
            out.push(n);
            n = match self.nodes[n].kind {
                NodeKind::Folder { end } if self.folded.contains(&n) => end,
                _ => n + 1,
            };
        }
        out
    }

    pub fn check(&self, node: usize) -> Check {
        let (mut picked, mut total) = (0, 0);
        for i in self.files_under(node) {
            total += 1;
            picked += usize::from(self.picked[i]);
        }
        match picked {
            0 => Check::None,
            p if p == total => Check::All,
            _ => Check::Some,
        }
    }

    pub fn is_risky(&self, file: usize) -> bool {
        self.files[file].risk == Some(FileRisk::Executable)
    }

    pub fn toggle(&mut self) {
        match self.nodes[self.cursor].kind {
            NodeKind::File(i) => self.picked[i] = !self.picked[i],
            NodeKind::Folder { .. } => {
                let safe: Vec<usize> = self.files_under(self.cursor).filter(|&i| !self.is_risky(i)).collect();
                let all = safe.iter().all(|&i| self.picked[i]);
                for i in safe {
                    self.picked[i] = !all;
                }
            }
        }
    }

    pub fn toggle_all(&mut self) {
        let any = self.picked.iter().any(|p| *p);
        for i in 0..self.files.len() {
            self.picked[i] = !any && !self.is_risky(i);
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let visible = self.visible();
        let at = visible.iter().position(|&n| n == self.cursor).unwrap_or(0);
        let to = at.saturating_add_signed(delta).min(visible.len().saturating_sub(1));
        self.cursor = visible[to];
    }

    pub fn fold(&mut self) {
        let unfolded_folder =
            matches!(self.nodes[self.cursor].kind, NodeKind::Folder { .. }) && !self.folded.contains(&self.cursor);
        if unfolded_folder {
            self.folded.insert(self.cursor);
        } else if let Some(parent) = self.parent(self.cursor) {
            self.cursor = parent;
        }
    }

    pub fn unfold(&mut self) {
        self.folded.remove(&self.cursor);
    }

    fn parent(&self, node: usize) -> Option<usize> {
        let depth = self.nodes[node].depth;
        (0..node).rev().find(|&n| self.nodes[n].depth < depth)
    }

    pub fn picked_count(&self) -> usize {
        self.picked.iter().filter(|p| **p).count()
    }

    pub fn picked_size(&self) -> u64 {
        self.files.iter().zip(&self.picked).filter(|(_, p)| **p).map(|(f, _)| f.size).sum()
    }

    pub fn risky_left_out(&self) -> usize {
        (0..self.files.len()).filter(|&i| self.is_risky(i) && !self.picked[i]).count()
    }

    pub fn selection(&self) -> Vec<String> {
        self.files.iter().zip(&self.picked).filter(|(_, p)| **p).map(|(f, _)| f.path.clone()).collect()
    }

    pub fn settle(&mut self, rows: usize) {
        let visible = self.visible();
        let at = visible.iter().position(|&n| n == self.cursor).unwrap_or(0);
        if at < self.scroll {
            self.scroll = at;
        } else if rows > 0 && at >= self.scroll + rows {
            self.scroll = at + 1 - rows;
        }
        self.scroll = self.scroll.min(visible.len().saturating_sub(rows.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64, risky: bool) -> TorrentFile {
        TorrentFile {
            index: 0,
            path: path.into(),
            size,
            selected: false,
            verified: 0,
            risk: risky.then_some(FileRisk::Executable),
        }
    }

    fn dvd() -> Tree {
        Tree::new(
            "dvd",
            vec![
                file("Night.mp4", 500, false),
                file("extras/meta.xml", 4, false),
                file("Night.srt", 50, false),
                file("extras/setup.exe", 2, true),
            ],
        )
    }

    fn outline(tree: &Tree) -> Vec<String> {
        tree.visible()
            .iter()
            .map(|&n| format!("{}{}", "  ".repeat(tree.nodes[n].depth as usize), tree.nodes[n].name))
            .collect()
    }

    #[test]
    fn folders_gather_their_files_in_first_seen_order() {
        assert_eq!(outline(&dvd()), ["dvd", "  Night.mp4", "  extras", "    meta.xml", "    setup.exe", "  Night.srt"]);
    }

    #[test]
    fn a_single_file_torrent_has_no_root_folder() {
        let tree = Tree::new("arch.iso", vec![file("arch.iso", 9, false)]);
        assert_eq!(outline(&tree), ["arch.iso"]);
    }

    #[test]
    fn risky_files_start_unpicked_and_folders_leave_them_alone() {
        let mut tree = dvd();
        assert_eq!((tree.picked_count(), tree.picked_size(), tree.risky_left_out()), (3, 554, 1));
        assert_eq!(tree.check(0), Check::Some);
        tree.cursor = 2;
        tree.toggle();
        assert_eq!(tree.picked, [true, false, true, false]);
        tree.toggle();
        assert_eq!(tree.picked, [true, true, true, false]);
        tree.cursor = 4;
        tree.toggle();
        assert_eq!(tree.check(0), Check::All);
        assert_eq!(tree.selection(), ["Night.mp4", "extras/meta.xml", "Night.srt", "extras/setup.exe"]);
    }

    #[test]
    fn select_all_or_none() {
        let mut tree = dvd();
        tree.toggle_all();
        assert_eq!(tree.picked_count(), 0);
        tree.toggle_all();
        assert_eq!(tree.selection(), ["Night.mp4", "extras/meta.xml", "Night.srt"]);
    }

    #[test]
    fn folding_hides_a_subtree_and_h_climbs_to_the_parent() {
        let mut tree = dvd();
        tree.cursor = 3;
        tree.fold();
        assert_eq!(tree.cursor, 2);
        tree.fold();
        assert_eq!(outline(&tree), ["dvd", "  Night.mp4", "  extras", "  Night.srt"]);
        tree.move_by(1);
        assert_eq!(tree.nodes[tree.cursor].name, "Night.srt");
        tree.move_by(-1);
        tree.unfold();
        assert_eq!(tree.visible().len(), 6);
    }

    #[test]
    fn settling_keeps_the_cursor_in_view() {
        let mut tree = dvd();
        tree.cursor = 5;
        tree.settle(3);
        assert_eq!(tree.scroll, 3);
        tree.cursor = 1;
        tree.settle(3);
        assert_eq!(tree.scroll, 1);
    }
}
