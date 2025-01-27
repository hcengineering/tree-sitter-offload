use std::{
    collections::HashMap,
    iter::Peekable,
    ops::{Deref, Range},
    sync::Arc,
    usize,
};

use jni::{
    objects::{JCharArray, JClass, JObject, JValue},
    sys::{jint, jsize},
    JNIEnv,
};
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{
    ffi::{self, TSTree},
    Node, Query, QueryCursor, TextProvider, Tree,
};

use crate::language_registry::{LanguageId, LANGUAGE_REGISTRY};

use super::HighlightToken;

struct RecodingUtf16TextProvider<'a> {
    text: &'a [u16],
    buffer: &'a [u8; 64],
}

struct RecodingUtf16TextProviderIterator<'a, I: Iterator<Item = char>> {
    iterator: Peekable<I>,
    buffer: &'a [u8; 64],
    buffer_written: usize,
}

impl<'a, I: Iterator<Item = char>> Iterator for RecodingUtf16TextProviderIterator<'a, I> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(c) = self.iterator.peek() {
            if self.buffer_written + char::len_utf8(*c) > self.buffer.len() {
                return Some(&self.buffer[..self.buffer_written]);
            } else {
                let _ = self.iterator.next();
            }
        }
        if self.buffer_written > 0 {
            Some(&self.buffer[..self.buffer_written])
        } else {
            None
        }
    }
}

impl<'a> TextProvider<&'a [u8]> for RecodingUtf16TextProvider<'a> {
    type I = RecodingUtf16TextProviderIterator<'a, Box<dyn Iterator<Item = char> + 'a>>;

    fn text(&mut self, node: Node) -> Self::I {
        let start_offset = node.start_byte() / 2;
        let end_offset = node.end_byte() / 2;

        let char_iterator = char::decode_utf16(self.text[start_offset..end_offset].iter().copied())
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER));
        let char_iterator: Box<dyn Iterator<Item = char> + 'a> = Box::new(char_iterator);
        RecodingUtf16TextProviderIterator {
            iterator: char_iterator.peekable(),
            buffer: self.buffer,
            buffer_written: 0,
        }
    }
}

pub fn highlight_tokens_cover(
    tree: &Tree,
    query: &Query,
    text: &[u16],
    range: Range<usize>,
) -> (usize, Vec<HighlightToken>) {
    let mut query_cursor = QueryCursor::new();
    let byte_start = range.start * 2;
    let byte_end = range.end * 2;
    query_cursor.set_byte_range(byte_start..byte_end);
    let root = tree.root_node();
    let text_buffer = Box::new([0u8; 64]);
    let text_provider = RecodingUtf16TextProvider {
        text,
        buffer: &text_buffer,
    };
    let mut captures = query_cursor.captures(query, root, text_provider);
    let mut highlights: HashMap<usize, u16> = HashMap::new();
    while let Some((next_match, cidx)) = captures.next() {
        let capture = next_match.captures[*cidx];
        let node_id = capture.node.id();
        let capture_id = capture.index as u16;
        highlights.insert(node_id, capture_id);
    }

    let mut highlight_stack: Vec<(usize, u16)> = Vec::new();
    let mut highlight_tokens: Vec<HighlightToken> = Vec::new();
    let token_from_node = |node: Node<'_>, highlight_stack: &[(usize, u16)]| HighlightToken {
        kind_id: node.kind_id(),
        capture_id: highlight_stack
            .last()
            .map(|(_, capture_id)| *capture_id)
            .unwrap_or(u16::MAX),
        length: ((node.end_byte() - node.start_byte()) / 2) as u32,
    };
    let token_from_node_subrange =
        |node: Node<'_>, range: Range<usize>, highlight_stack: &[(usize, u16)]| HighlightToken {
            kind_id: node.kind_id(),
            capture_id: highlight_stack
                .last()
                .map(|(_, capture_id)| *capture_id)
                .unwrap_or(u16::MAX),
            length: ((range.end - range.start) / 2) as u32,
        };
    let mut tree_cursor = root.walk();
    loop {
        let node_id = tree_cursor.node().id();
        if let Some(capture_id) = highlights.get(&node_id).copied() {
            highlight_stack.push((node_id, capture_id));
        }
        if tree_cursor.goto_first_child_for_byte(byte_start).is_none() {
            break;
        }
    }
    let actual_byte_start = tree_cursor.node().start_byte();
    let mut byte_current = actual_byte_start;
    while byte_current < byte_end {
        let node = tree_cursor.node();
        let node_id = node.id();
        if byte_current < node.end_byte() {
            if tree_cursor.goto_first_child() {
                if tree_cursor.node().start_byte() > byte_current {
                    highlight_tokens.push(token_from_node_subrange(
                        node,
                        byte_current..tree_cursor.node().start_byte(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().start_byte();
                }
                let node = tree_cursor.node();
                let node_id = node.id();
                if let Some(capture_id) = highlights.get(&node_id).copied() {
                    highlight_stack.push((node_id, capture_id));
                }
            } else {
                highlight_tokens.push(token_from_node(node, &highlight_stack));
                byte_current = node.end_byte();
            }
        } else {
            if let Some((highlight_id, _)) = highlight_stack.last() {
                if node_id == *highlight_id {
                    highlight_stack.pop();
                }
            }
            if tree_cursor.goto_next_sibling() {
                if tree_cursor.node().start_byte() > byte_current {
                    let parent = tree_cursor
                        .node()
                        .parent()
                        .expect("common parent with a sibling");
                    highlight_tokens.push(token_from_node_subrange(
                        parent,
                        byte_current..tree_cursor.node().start_byte(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().start_byte();
                }
                let node = tree_cursor.node();
                let node_id = node.id();
                if let Some(capture_id) = highlights.get(&node_id).copied() {
                    highlight_stack.push((node_id, capture_id));
                }
            } else if tree_cursor.goto_parent() {
                if tree_cursor.node().end_byte() > byte_current {
                    highlight_tokens.push(token_from_node_subrange(
                        tree_cursor.node(),
                        byte_current..tree_cursor.node().end_byte(),
                        &highlight_stack,
                    ));
                    byte_current = tree_cursor.node().end_byte();
                }
            } else {
                break;
            }
        }
    }
    (actual_byte_start / 2, highlight_tokens)
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeHighlightLexer_nativeCollectHighlights<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    language_id: LanguageId,
    tree: JObject<'local>,
    text: JCharArray<'local>,
    start_offset: jint,
    end_offset: jint,
) -> JObject<'local> {
    let tree_ptr = env
        .call_method(&tree, "getPtr", "()J", &[])
        .unwrap()
        .j()
        .unwrap();
    let tree_ptr = tree_ptr as *mut TSTree;
    let copied_tree = unsafe { ffi::ts_tree_copy(tree_ptr) };
    let tree = unsafe { Tree::from_raw(copied_tree) };
    let text_length = env.get_array_length(&text).unwrap();
    let mut text_buffer = vec![0u16; text_length as usize];
    env.get_char_array_region(&text, 0, &mut text_buffer)
        .unwrap();
    let query = {
        let registry = LANGUAGE_REGISTRY.read().unwrap();
        let Some(language) = registry.language(language_id) else {
            return JObject::null();
        };
        let query = language
            .parser_info()
            .highlights_query
            .as_ref()
            .map(Arc::clone);
        query
    };
    let Some(query) = query else {
        return JObject::null();
    };

    let (start_offset, tokens) = highlight_tokens_cover(
        &tree,
        &query,
        &text_buffer,
        (start_offset as usize)..(end_offset as usize),
    );
    let token_lengths = env.new_int_array(tokens.len() as i32).unwrap();
    let token_node_kinds = env.new_short_array(tokens.len() as i32).unwrap();
    let token_capture_ids = env.new_short_array(tokens.len() as i32).unwrap();
    const CHUNK_SIZE: usize = 2048;
    let mut token_lengths_buf: Vec<i32> = Vec::with_capacity(CHUNK_SIZE);
    let mut token_node_kinds_buf: Vec<i16> = Vec::with_capacity(CHUNK_SIZE);
    let mut token_capture_ids_buf: Vec<i16> = Vec::with_capacity(CHUNK_SIZE);
    for (slice_idx, tokens_slice) in tokens.chunks(CHUNK_SIZE).enumerate() {
        for token in tokens_slice {
            token_lengths_buf.push(token.length as i32);
            token_node_kinds_buf.push(token.kind_id as i16);
            token_capture_ids_buf.push(token.capture_id as i16);
        }
        env.set_int_array_region(
            &token_lengths,
            (slice_idx * CHUNK_SIZE) as jsize,
            &token_lengths_buf,
        )
        .unwrap();
        env.set_short_array_region(
            &token_node_kinds,
            (slice_idx * CHUNK_SIZE) as jsize,
            &token_node_kinds_buf,
        )
        .unwrap();
        env.set_short_array_region(
            &token_capture_ids,
            (slice_idx * CHUNK_SIZE) as jsize,
            &token_capture_ids_buf,
        )
        .unwrap();
        token_lengths_buf.clear();
        token_node_kinds_buf.clear();
        token_capture_ids_buf.clear();
    }
    let tokens_obj = env
        .new_object(
            "com/hulylabs/treesitter/rusty/TreeSitterNativeHighlightLexer$Tokens",
            "(I[I[S[S)V",
            &[
                JValue::Int(start_offset as i32),
                JValue::Object(token_lengths.deref()),
                JValue::Object(token_node_kinds.deref()),
                JValue::Object(token_capture_ids.deref()),
            ],
        )
        .unwrap();

    tokens_obj
}
