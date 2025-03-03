use std::{
    collections::HashMap,
    ops::{Deref, Range},
};

use jni::{
    errors::Result as JNIResult,
    objects::{JCharArray, JClass, JObject, JValue},
    sys::{jint, jsize},
    JNIEnv,
};
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Node, QueryCursor};

use crate::{
    jni_utils::throw_exception_from_result,
    language_registry::with_language,
    query::RecodingUtf16TextProvider,
    syntax_snapshot::{
        SyntaxSnapshot, SyntaxSnapshotDesc, SyntaxSnapshotEntryContent, SyntaxSnapshotTreeCursor,
    },
    LanguageId,
};

use super::HighlightToken;

type ParentStackEntry = (LanguageId, usize, Range<usize>);

// Find start byte of minimal token cover of range
// Returns (cover_start_byte, parent_stack, tree_cursor)
fn find_cover_start(
    snapshot: &SyntaxSnapshot,
    byte_start: usize,
) -> (usize, Vec<ParentStackEntry>, SyntaxSnapshotTreeCursor<'_>) {
    let mut tree_cursor = SyntaxSnapshotTreeCursor::walk(snapshot);
    let mut parent_stack = Vec::new();
    loop {
        let node = tree_cursor.node();
        parent_stack.push((
            tree_cursor.language(),
            node.id(),
            node.start_byte()..node.end_byte(),
        ));
        if tree_cursor.goto_first_child_for_byte(byte_start).is_none() {
            break;
        }
    }
    debug_assert_eq!(
        parent_stack.last().map(|(_, node_id, _)| *node_id),
        Some(tree_cursor.node().id())
    );
    let mut cover_start_byte = tree_cursor.node().start_byte();
    while cover_start_byte >= byte_start {
        // Need to extend cover to the left, but
        // there is no node between cover_start and current node
        if tree_cursor.goto_previous_sibling() {
            let node = tree_cursor.node();
            *parent_stack
                .last_mut()
                .expect("has stack entries if has previous sibling") = (
                tree_cursor.language(),
                node.id(),
                node.start_byte()..node.end_byte(),
            );
            cover_start_byte = tree_cursor.node().end_byte();
        } else if tree_cursor.goto_parent() {
            parent_stack.pop();
            cover_start_byte = tree_cursor.node().start_byte();
        } else {
            // start of the file, no nodes before start of range
            cover_start_byte = 0;
            break;
        }
    }
    debug_assert!(cover_start_byte <= byte_start);
    (cover_start_byte, parent_stack, tree_cursor)
}

fn collect_highlights_for_range(
    snapshot: &SyntaxSnapshot,
    text: &[u16],
    byte_range: Range<usize>,
) -> HashMap<Range<usize>, (LanguageId, u16, usize)> {
    let mut query_cursor = QueryCursor::new();
    query_cursor.set_byte_range(byte_range.clone());
    let text_provider = RecodingUtf16TextProvider::new(text);
    let intersecting_entries = snapshot.entries.iter().filter(|entry| {
        entry.byte_range.start <= byte_range.end && entry.byte_range.end >= byte_range.start
    });
    let mut highlights: HashMap<Range<usize>, (LanguageId, u16, usize)> = HashMap::new();
    for entry in intersecting_entries {
        let SyntaxSnapshotEntryContent::Parsed { language, tree } = &entry.content else {
            continue;
        };
        let query = with_language(*language, |language| {
            language.parser_info().highlights_query.clone()
        });
        let Ok(Some(query)) = query else {
            continue;
        };
        let root_node = tree.root_node_with_offset(entry.byte_offset, entry.point_offset);
        let mut captures = query_cursor.captures(&query.0, root_node, &text_provider);
        while let Some((next_match, cidx)) = captures.next() {
            if !query
                .1
                .satisfies_predicates(&mut &text_provider, next_match)
            {
                next_match.remove();
                continue;
            }
            let capture = next_match.captures[*cidx];
            let range = capture.node.start_byte()..capture.node.end_byte();
            let capture_id = capture.index as u16;
            if !query.2.contains(capture_id as usize) {
                continue;
            }
            if let Some((other_language, _, pattern_index)) = highlights.get(&range) {
                if other_language == language && next_match.pattern_index < *pattern_index {
                    continue;
                }
            }
            highlights.insert(range, (*language, capture_id, next_match.pattern_index));
        }
    }
    highlights
}

pub fn highlight_tokens_cover(
    snapshot: &SyntaxSnapshot,
    text: &[u16],
    range: Range<usize>,
) -> (usize, Vec<HighlightToken>) {
    let (byte_start, parent_stack, mut tree_cursor) = find_cover_start(snapshot, range.start * 2);
    let byte_end = range.end * 2;

    let highlights = collect_highlights_for_range(snapshot, text, byte_start..byte_end);

    let mut highlight_stack: Vec<(LanguageId, usize, u16)> = parent_stack
        .into_iter()
        .filter_map(|(language_id, node_id, range)| {
            highlights
                .get(&range)
                .and_then(|(h_language_id, capture_id, _)| {
                    if language_id == *h_language_id {
                        Some((language_id, node_id, *capture_id))
                    } else {
                        None
                    }
                })
        })
        .collect();

    let mut highlight_tokens: Vec<HighlightToken> = Vec::new();
    let token_from_node =
        |node: Node<'_>, language_id: LanguageId, highlight_stack: &[(LanguageId, usize, u16)]| {
            HighlightToken {
                language_id,
                kind_id: node.kind_id(),
                capture_id: highlight_stack
                    .last()
                    .and_then(|(lang, _, capture_id)| {
                        if *lang == language_id {
                            Some(*capture_id)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(u16::MAX),
                length: ((node.end_byte() - node.start_byte()) / 2) as u32,
            }
        };
    let token_from_node_subrange =
        |range: Range<usize>,
         language_id: LanguageId,
         highlight_stack: &[(LanguageId, usize, u16)]| HighlightToken {
            language_id,
            kind_id: u16::MAX,
            capture_id: highlight_stack
                .last()
                .and_then(|(lang, _, capture_id)| {
                    if *lang == language_id {
                        Some(*capture_id)
                    } else {
                        None
                    }
                })
                .unwrap_or(u16::MAX),
            length: ((range.end - range.start) / 2) as u32,
        };

    let mut byte_current = byte_start;
    while byte_current < byte_end {
        let node = tree_cursor.node();
        let node_id = node.id();
        debug_assert!(byte_current >= node.start_byte());
        if byte_current < node.end_byte() {
            if tree_cursor.goto_first_child() {
                if tree_cursor.node().start_byte() > byte_current {
                    highlight_tokens.push(token_from_node_subrange(
                        byte_current..tree_cursor.node().start_byte(),
                        tree_cursor.language(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().start_byte();
                }
                let node = tree_cursor.node();
                let node_id = node.id();
                let range = node.start_byte()..node.end_byte();
                if let Some((lang, capture_id, _)) = highlights.get(&range).copied() {
                    if tree_cursor.language() == lang {
                        highlight_stack.push((lang, node_id, capture_id));
                    }
                }
            } else {
                if byte_current < node.start_byte() {
                    highlight_tokens.push(token_from_node_subrange(
                        byte_current..node.start_byte(),
                        tree_cursor.language(),
                        &highlight_stack,
                    ));
                }
                highlight_tokens.push(token_from_node(
                    node,
                    tree_cursor.language(),
                    &highlight_stack,
                ));
                byte_current = node.end_byte();
            }
        } else {
            if let Some((lang, highlight_node_id, _)) = highlight_stack.last() {
                if node_id == *highlight_node_id && tree_cursor.language() == *lang {
                    highlight_stack.pop();
                }
            }
            if tree_cursor.goto_next_sibling() {
                if tree_cursor.node().start_byte() > byte_current {
                    highlight_tokens.push(token_from_node_subrange(
                        byte_current..tree_cursor.node().start_byte(),
                        tree_cursor.language(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().start_byte();
                }
                let node = tree_cursor.node();
                let node_id = node.id();
                let range = node.start_byte()..node.end_byte();
                if let Some((lang, capture_id, _)) = highlights.get(&range).copied() {
                    if tree_cursor.language() == lang {
                        highlight_stack.push((lang, node_id, capture_id));
                    }
                }
            } else if tree_cursor.goto_parent() {
                if tree_cursor.node().end_byte() > byte_current {
                    highlight_tokens.push(token_from_node_subrange(
                        byte_current..tree_cursor.node().end_byte(),
                        tree_cursor.language(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().end_byte();
                }
            } else {
                break;
            }
        }
    }
    (byte_start / 2, highlight_tokens)
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeHighlightLexer_nativeCollectHighlights<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    snapshot: JObject<'local>,
    text: JCharArray<'local>,
    start_offset: jint,
    end_offset: jint,
) -> JObject<'local> {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        snapshot: JObject<'local>,
        text: JCharArray<'local>,
        start_offset: jint,
        end_offset: jint,
    ) -> JNIResult<JObject<'local>> {
        let snapshot = SyntaxSnapshotDesc::from_java_object(env, snapshot)?;
        let text_length = env.get_array_length(&text)?;
        let mut text_buffer = vec![0u16; text_length as usize];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;

        let (start_offset, tokens) = highlight_tokens_cover(
            snapshot,
            &text_buffer,
            (start_offset as usize)..(end_offset as usize),
        );
        let token_lengths = env.new_int_array(tokens.len() as i32)?;
        let token_node_kinds = env.new_short_array(tokens.len() as i32)?;
        let token_capture_ids = env.new_short_array(tokens.len() as i32)?;
        let token_languages = env.new_long_array(tokens.len() as i32)?;
        const CHUNK_SIZE: usize = 2048;
        let mut token_lengths_buf: Vec<i32> = Vec::with_capacity(CHUNK_SIZE);
        let mut token_node_kinds_buf: Vec<i16> = Vec::with_capacity(CHUNK_SIZE);
        let mut token_capture_ids_buf: Vec<i16> = Vec::with_capacity(CHUNK_SIZE);
        let mut token_languages_buf: Vec<i64> = Vec::with_capacity(CHUNK_SIZE);
        for (slice_idx, tokens_slice) in tokens.chunks(CHUNK_SIZE).enumerate() {
            for token in tokens_slice {
                token_lengths_buf.push(token.length as i32);
                token_node_kinds_buf.push(token.kind_id as i16);
                token_capture_ids_buf.push(token.capture_id as i16);
                token_languages_buf.push(token.language_id.into());
            }
            env.set_int_array_region(
                &token_lengths,
                (slice_idx * CHUNK_SIZE) as jsize,
                &token_lengths_buf,
            )?;
            env.set_short_array_region(
                &token_node_kinds,
                (slice_idx * CHUNK_SIZE) as jsize,
                &token_node_kinds_buf,
            )?;
            env.set_short_array_region(
                &token_capture_ids,
                (slice_idx * CHUNK_SIZE) as jsize,
                &token_capture_ids_buf,
            )?;
            env.set_long_array_region(
                &token_languages,
                (slice_idx * CHUNK_SIZE) as jsize,
                &token_languages_buf,
            )?;
            token_lengths_buf.clear();
            token_node_kinds_buf.clear();
            token_capture_ids_buf.clear();
            token_languages_buf.clear();
        }
        let tokens_obj = env.new_object(
            "com/hulylabs/treesitter/rusty/TreeSitterNativeHighlightLexer$Tokens",
            "(I[I[S[S[J)V",
            &[
                JValue::Int(start_offset as i32),
                JValue::Object(token_lengths.deref()),
                JValue::Object(token_node_kinds.deref()),
                JValue::Object(token_capture_ids.deref()),
                JValue::Object(token_languages.deref()),
            ],
        )?;

        Ok(tokens_obj)
    }
    let result = inner(&mut env, snapshot, text, start_offset, end_offset);
    throw_exception_from_result(&mut env, result)
}
