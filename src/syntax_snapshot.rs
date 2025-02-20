use std::{
    borrow::Cow,
    collections::BinaryHeap,
    ops::Range,
    sync::{Arc, LazyLock, Mutex},
};

use crate::{
    injections::InjectionMatch,
    language_registry::{with_language, with_unknown_language, LanguageId, UnknownLanguage},
};

mod jni_methods;
pub use jni_methods::SyntaxSnapshotDesc;
use tree_sitter as ts;

#[derive(Default)]
struct ParsersPool {
    pool: Arc<Mutex<Vec<ts::Parser>>>,
}

static PARSERS_POOL: LazyLock<ParsersPool> = LazyLock::new(ParsersPool::default);

impl ParsersPool {
    fn with_parser<T, F: FnOnce(&mut ts::Parser) -> T>(&self, func: F) -> T {
        let mut parser = {
            let mut guard = self.pool.lock().unwrap();
            guard.pop().unwrap_or_default()
        };
        let result = func(&mut parser);
        parser.reset();
        let mut guard = self.pool.lock().unwrap();
        guard.push(parser);
        result
    }
}

fn with_parser<T, F: FnOnce(&mut ts::Parser) -> T>(func: F) -> T {
    PARSERS_POOL.with_parser(func)
}

#[derive(Debug, PartialEq, Eq)]
enum ParseCommandLanguage {
    Known(LanguageId),
    Unknown(UnknownLanguage),
}

#[derive(Debug, PartialEq, Eq)]
struct ParseCommand {
    depth: usize,
    language: ParseCommandLanguage,
    included_ranges: Vec<ts::Range>,
    byte_range: std::ops::Range<usize>,
    byte_offset: usize,
    point_offset: ts::Point,
}

impl ParseCommand {
    fn source_language(&self) -> Cow<'_, UnknownLanguage> {
        match &self.language {
            ParseCommandLanguage::Known(language_id) => {
                let language_name: Box<str> =
                    with_language(*language_id, |language| language.name().into())
                        .unwrap_or_else(|_| format!("Language({language_id:?})").into());
                Cow::Owned(UnknownLanguage::LanguageName(language_name))
            }
            ParseCommandLanguage::Unknown(unknown_language) => Cow::Borrowed(unknown_language),
        }
    }
    fn language_id(&self) -> Option<LanguageId> {
        match self.language {
            ParseCommandLanguage::Known(language_id) => Some(language_id),
            ParseCommandLanguage::Unknown(_) => None,
        }
    }

    fn from_injection(injection: InjectionMatch, depth: usize) -> Self {
        let language = with_unknown_language(&injection.language, |language| {
            ParseCommandLanguage::Known(language.id())
        })
        .unwrap_or(ParseCommandLanguage::Unknown(injection.language));
        let injection_start = injection
            .included_ranges
            .first()
            .expect("injection always has at least one range");
        let byte_offset = injection_start.start_byte;
        let point_offset = injection_start.start_point;
        Self {
            depth,
            language,
            included_ranges: injection.included_ranges,
            byte_range: injection.enclosing_byte_range,
            byte_offset,
            point_offset,
        }
    }
}

impl PartialOrd for ParseCommand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParseCommand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        Ord::cmp(&self.depth, &other.depth)
            .then_with(|| Ord::cmp(&self.byte_range.start, &other.byte_range.start))
            .then_with(|| Ord::cmp(&self.byte_range.end, &other.byte_range.end))
            .reverse()
    }
}

pub struct SyntaxSnapshot {
    pub(crate) entries: Vec<SyntaxSnapshotEntry>,
}

#[derive(Debug, Clone)]
pub(crate) enum SyntaxSnapshotEntryContent {
    Parsed {
        language: LanguageId,
        tree: ts::Tree,
    },
    Unparsed(#[allow(dead_code)] UnknownLanguage),
}

#[derive(Debug, Clone)]
pub struct SyntaxSnapshotEntry {
    pub(crate) depth: usize,
    pub(crate) content: SyntaxSnapshotEntryContent,
    pub(crate) byte_range: Range<usize>,
    pub(crate) byte_offset: usize,
    pub(crate) point_offset: ts::Point,
}

impl SyntaxSnapshotEntry {
    fn new_unparsed(parse_command: &ParseCommand) -> Self {
        Self {
            depth: parse_command.depth,
            content: SyntaxSnapshotEntryContent::Unparsed(
                parse_command.source_language().into_owned(),
            ),
            byte_range: parse_command.byte_range.clone(),
            byte_offset: parse_command.byte_offset,
            point_offset: parse_command.point_offset,
        }
    }
}

fn sub_point(point1: &ts::Point, point2: &ts::Point) -> ts::Point {
    if point1.row == point2.row {
        ts::Point {
            row: 0,
            column: point1.column.saturating_sub(point2.column),
        }
    } else {
        ts::Point {
            row: point1.row.saturating_sub(point2.row),
            column: point1.column,
        }
    }
}

impl SyntaxSnapshot {
    pub fn base_language(&self) -> LanguageId {
        match &self
            .entries
            .first()
            .expect("there is always a main entry")
            .content
        {
            SyntaxSnapshotEntryContent::Parsed { language, .. } => *language,
            SyntaxSnapshotEntryContent::Unparsed(_) => unreachable!(),
        }
    }

    pub fn main_tree(&self) -> &ts::Tree {
        match &self
            .entries
            .first()
            .expect("there is always a main entry")
            .content
        {
            SyntaxSnapshotEntryContent::Parsed { language: _, tree } => tree,
            SyntaxSnapshotEntryContent::Unparsed(_) => unreachable!(),
        }
    }

    fn parse(base_language_id: LanguageId, text: &[u16]) -> Option<Self> {
        let mut entries: Vec<SyntaxSnapshotEntry> = Vec::new();
        let mut parse_queue: BinaryHeap<ParseCommand> = BinaryHeap::new();
        parse_queue.push(ParseCommand {
            depth: 0,
            language: ParseCommandLanguage::Known(base_language_id),
            byte_range: 0..text.len() * 2,
            included_ranges: Vec::new(),
            byte_offset: 0,
            point_offset: ts::Point::default(),
        });
        while let Some(parse_command) = parse_queue.pop() {
            let Some(language_id) = parse_command.language_id() else {
                entries.push(SyntaxSnapshotEntry::new_unparsed(&parse_command));
                continue;
            };
            let (ts_language, injections_query) = with_language(language_id, |language| {
                (
                    language.ts_language(),
                    language.parser_info().injections_query.clone(),
                )
            })
            .ok()?;
            let mut included_ranges = parse_command.included_ranges.clone();
            for range in &mut included_ranges {
                range.start_byte -= parse_command.byte_offset;
                range.start_point = sub_point(&range.start_point, &parse_command.point_offset);
                range.end_byte -= parse_command.byte_offset;
                range.end_point = sub_point(&range.end_point, &parse_command.point_offset);
            }
            let tree = with_parser(|parser| {
                parser.set_language(&ts_language).ok()?;
                parser.set_included_ranges(&included_ranges).ok()?;
                let text_slice =
                    &text[(parse_command.byte_range.start / 2)..(parse_command.byte_range.end / 2)];
                parser.parse_utf16(text_slice, None)
            });
            let Some(tree) = tree else {
                entries.push(SyntaxSnapshotEntry::new_unparsed(&parse_command));
                continue;
            };
            if let Some(injections_query) = injections_query {
                let node = tree
                    .root_node_with_offset(parse_command.byte_offset, parse_command.point_offset);
                let injections = injections_query.collect_injections(
                    node,
                    text,
                    &[parse_command.byte_range.clone()],
                );
                parse_queue.extend(injections.into_iter().map(|injection| {
                    ParseCommand::from_injection(injection, parse_command.depth + 1)
                }));
            }

            let entry = SyntaxSnapshotEntry {
                depth: parse_command.depth,
                content: SyntaxSnapshotEntryContent::Parsed {
                    language: language_id,
                    tree,
                },
                byte_range: parse_command.byte_range,
                byte_offset: parse_command.byte_offset,
                point_offset: parse_command.point_offset,
            };
            entries.push(entry);
        }
        if !entries.is_empty()
            && matches!(
                entries.first(),
                Some(SyntaxSnapshotEntry {
                    content: SyntaxSnapshotEntryContent::Parsed { .. },
                    ..
                })
            )
        {
            Some(SyntaxSnapshot { entries })
        } else {
            None
        }
    }

    fn parse_incremental(
        text: &[u16],
        old_snapshot: &SyntaxSnapshot,
        edit: ts::InputEdit,
    ) -> Option<(Self, Vec<ts::Range>)> {
        let base_language_id = old_snapshot.base_language();
        let mut entries: Vec<SyntaxSnapshotEntry> = Vec::new();
        let mut parse_queue: BinaryHeap<ParseCommand> = BinaryHeap::new();
        let mut changed_ranges: Vec<ts::Range> = Vec::new();
        changed_ranges.push(ts::Range {
            start_byte: edit.start_byte,
            end_byte: edit.new_end_byte,
            start_point: edit.start_position,
            end_point: edit.new_end_position,
        });
        parse_queue.push(ParseCommand {
            depth: 0,
            language: ParseCommandLanguage::Known(base_language_id),
            byte_range: 0..text.len() * 2,
            included_ranges: Vec::new(),
            byte_offset: 0,
            point_offset: ts::Point::default(),
        });
        while let Some(parse_command) = parse_queue.pop() {
            let Some(language_id) = parse_command.language_id() else {
                entries.push(SyntaxSnapshotEntry::new_unparsed(&parse_command));
                continue;
            };
            let (ts_language, injections_query) = with_language(language_id, |language| {
                (
                    language.ts_language(),
                    language.parser_info().injections_query.clone(),
                )
            })
            .ok()?;
            let mut old_tree: Option<ts::Tree> = None;
            if parse_command.depth == 0 {
                let old_entry = &old_snapshot.entries[0];
                if old_entry.byte_range.end >= edit.old_end_byte
                    && (old_entry.byte_range.end - edit.old_end_byte) + edit.new_end_byte
                        == parse_command.byte_range.end
                {
                    old_tree = if let SyntaxSnapshotEntryContent::Parsed { language, tree } =
                        &old_entry.content
                    {
                        if *language == language_id {
                            let mut tree = tree.clone();
                            tree.edit(&edit);
                            Some(tree)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                }
            }
            let mut included_ranges = parse_command.included_ranges.clone();
            for range in &mut included_ranges {
                range.start_byte -= parse_command.byte_offset;
                range.start_point = sub_point(&range.start_point, &parse_command.point_offset);
                range.end_byte -= parse_command.byte_offset;
                range.end_point = sub_point(&range.end_point, &parse_command.point_offset);
            }
            let tree = with_parser(|parser| {
                parser.set_language(&ts_language).ok()?;
                parser.set_included_ranges(&included_ranges).ok()?;
                let text_slice =
                    &text[(parse_command.byte_range.start / 2)..(parse_command.byte_range.end / 2)];
                parser.parse_utf16(text_slice, old_tree.as_ref())
            });
            let Some(tree) = tree else {
                entries.push(SyntaxSnapshotEntry::new_unparsed(&parse_command));
                continue;
            };
            if let Some(old_tree) = old_tree {
                let new_changed_ranges = old_tree.changed_ranges(&tree);
                changed_ranges.extend(new_changed_ranges);
            } else {
                changed_ranges.extend(included_ranges);
            }
            if let Some(injections_query) = injections_query {
                let node = tree
                    .root_node_with_offset(parse_command.byte_offset, parse_command.point_offset);
                let injections = injections_query.collect_injections(
                    node,
                    text,
                    &[parse_command.byte_range.clone()],
                );
                parse_queue.extend(injections.into_iter().map(|injection| {
                    ParseCommand::from_injection(injection, parse_command.depth + 1)
                }));
            }

            let entry = SyntaxSnapshotEntry {
                depth: parse_command.depth,
                content: SyntaxSnapshotEntryContent::Parsed {
                    language: language_id,
                    tree,
                },
                byte_range: parse_command.byte_range,
                byte_offset: parse_command.byte_offset,
                point_offset: parse_command.point_offset,
            };
            entries.push(entry);
        }
        if !entries.is_empty()
            && matches!(
                entries.first(),
                Some(SyntaxSnapshotEntry {
                    content: SyntaxSnapshotEntryContent::Parsed { .. },
                    ..
                })
            )
        {
            Some((SyntaxSnapshot { entries }, changed_ranges))
        } else {
            None
        }
    }
}

pub struct SyntaxSnapshotTreeCursor<'cursor> {
    snapshot: &'cursor SyntaxSnapshot,
    entry_stack: Vec<(usize, ts::TreeCursor<'cursor>)>,
}

impl<'cursor> SyntaxSnapshotTreeCursor<'cursor> {
    pub fn walk(snapshot: &'cursor SyntaxSnapshot) -> Self {
        let main_tree = snapshot.main_tree();
        let tree_cursor = main_tree.walk();
        Self {
            snapshot,
            entry_stack: vec![(0, tree_cursor)],
        }
    }

    pub fn language(&self) -> LanguageId {
        let (entry_idx, _cursor) = self.entry_stack.last().expect("stack is never empty");
        let entry = &self.snapshot.entries[*entry_idx];
        if let SyntaxSnapshotEntryContent::Parsed { language, tree: _ } = &entry.content {
            *language
        } else {
            unreachable!("unparsed entries do not appear on stack")
        }
    }

    pub fn node(&self) -> ts::Node<'cursor> {
        let (_entry_idx, cursor) = self.entry_stack.last().expect("stack is never empty");
        cursor.node()
    }

    pub fn goto_first_child_for_byte(&mut self, index: usize) -> Option<usize> {
        let (entry_idx, cursor) = self.entry_stack.last_mut().expect("stack is never empty");
        let entry = &self.snapshot.entries[*entry_idx];
        if index < entry.byte_range.start || index >= entry.byte_range.end {
            return None;
        }
        if let Some(child) = cursor.goto_first_child_for_byte(index) {
            return Some(child);
        } else {
            let node_range = cursor.node().byte_range();
            let candidate_entry = self.snapshot.entries.iter().enumerate().find(|(_, e)| {
                e.depth == entry.depth + 1
                    && e.byte_range.start >= node_range.start
                    && e.byte_range.end <= node_range.end
                    && index < entry.byte_range.end
            });
            if let Some((idx, entry)) = candidate_entry {
                if let SyntaxSnapshotEntryContent::Parsed { language: _, tree } = &entry.content {
                    let new_root =
                        tree.root_node_with_offset(entry.byte_offset, entry.point_offset);
                    let tree_cursor = new_root.walk();
                    self.entry_stack.push((idx, tree_cursor));
                    return Some(0);
                }
            }
        }
        None
    }

    pub fn goto_first_child(&mut self) -> bool {
        let (entry_idx, cursor) = self.entry_stack.last_mut().expect("stack is never empty");
        if cursor.goto_first_child() {
            return true;
        }
        let node_range = cursor.node().byte_range();
        let entry = &self.snapshot.entries[*entry_idx];
        let candidate_entry = self.snapshot.entries.iter().enumerate().find(|(_, e)| {
            e.depth == entry.depth + 1
                && e.byte_range.start >= node_range.start
                && e.byte_range.end <= node_range.end
        });
        if let Some((idx, entry)) = candidate_entry {
            if let SyntaxSnapshotEntryContent::Parsed { language: _, tree } = &entry.content {
                let new_root = tree.root_node_with_offset(entry.byte_offset, entry.point_offset);
                let tree_cursor = new_root.walk();
                self.entry_stack.push((idx, tree_cursor));
                return true;
            }
        }
        false
    }

    pub fn goto_previous_sibling(&mut self) -> bool {
        let (_entry_idx, cursor) = self.entry_stack.last_mut().expect("stack is never empty");
        cursor.goto_previous_sibling()
    }

    pub fn goto_next_sibling(&mut self) -> bool {
        let (_entry_idx, cursor) = self.entry_stack.last_mut().expect("stack is never empty");
        cursor.goto_next_sibling()
    }

    pub fn goto_parent(&mut self) -> bool {
        let (_entry_idx, cursor) = self.entry_stack.last_mut().expect("stack is never empty");
        if cursor.goto_parent() {
            return true;
        }
        if self.entry_stack.len() > 1 {
            let _ = self.entry_stack.pop();
            return true;
        }
        false
    }
}
