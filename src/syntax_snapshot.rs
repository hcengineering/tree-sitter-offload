use std::sync::{Arc, LazyLock, Mutex};

use crate::language_registry::{with_language, LanguageId};

mod jni_methods;
pub use jni_methods::SyntaxSnapshotDesc;

#[derive(Default)]
struct ParsersPool {
    pool: Arc<Mutex<Vec<tree_sitter::Parser>>>,
}

static PARSERS_POOL: LazyLock<ParsersPool> = LazyLock::new(ParsersPool::default);

impl ParsersPool {
    fn with_parser<T, F: FnOnce(&mut tree_sitter::Parser) -> T>(&self, func: F) -> T {
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

fn with_parser<T, F: FnOnce(&mut tree_sitter::Parser) -> T>(func: F) -> T {
    PARSERS_POOL.with_parser(func)
}

pub struct SyntaxSnapshot {
    pub(crate) tree: tree_sitter::Tree,
}

impl SyntaxSnapshot {
    fn parse(base_language_id: LanguageId, text: &[u16]) -> Option<Self> {
        let ts_language =
            with_language(base_language_id, |language| language.ts_language()).ok()?;
        let tree = with_parser(|parser| {
            parser.set_language(&ts_language).ok()?;
            parser.parse_utf16(text, None)
        });
        tree.map(|tree| Self { tree })
    }

    fn parse_incremental(
        base_language_id: LanguageId,
        text: &[u16],
        old_snapshot: &SyntaxSnapshot,
    ) -> Option<Self> {
        let ts_language =
            with_language(base_language_id, |language| language.ts_language()).ok()?;
        let tree = with_parser(|parser| {
            parser.set_language(&ts_language).ok()?;
            parser.parse_utf16(text, Some(&old_snapshot.tree))
        });
        tree.map(|tree| Self { tree })
    }

    fn with_edit(&self, edit: &tree_sitter::InputEdit) -> SyntaxSnapshot {
        let mut tree = self.tree.clone();
        tree.edit(edit);
        SyntaxSnapshot { tree }
    }

    fn changed_ranges(&self, other: &Self) -> impl ExactSizeIterator<Item = tree_sitter::Range> {
        self.tree.changed_ranges(&other.tree)
    }
}
