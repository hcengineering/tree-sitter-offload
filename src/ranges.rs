use std::{char, collections::HashMap, ops::Range, sync::Arc, usize};

use jni::{
    errors::Result as JNIResult,
    objects::{AutoLocal, JCharArray, JClass, JMethodID, JObject, JObjectArray, JValue},
    strings::JNIString,
    sys::{jboolean, jint, jsize},
    JNIEnv,
};
use streaming_iterator::StreamingIterator;
use tree_sitter::QueryCursor;

use crate::{
    jni_utils::{throw_exception_from_result, RangeDesc},
    language_registry::with_language,
    predicates::AdditionalPredicates,
    query::RecodingUtf16TextProvider,
    syntax_snapshot::{SyntaxSnapshot, SyntaxSnapshotDesc, SyntaxSnapshotEntryContent},
    Language, LanguageId,
};
use once_cell::sync::OnceCell as JOnceLock;

#[derive(thiserror::Error, Debug)]
pub enum RangesQueryError {
    #[error("required captures not found")]
    NoRequiredCaptures,
    #[error("duplicate captures found")]
    DuplicateCapture,
}

pub struct RangesQuery {
    query: tree_sitter::Query,
    predicates: AdditionalPredicates,
    main_capture_id: u32,
    start_capture_id: Option<u32>,
    end_capture_id: Option<u32>,
}

impl RangesQuery {
    pub fn new(
        query: tree_sitter::Query,
        predicates: AdditionalPredicates,
        main_capture_name: &str,
    ) -> Result<RangesQuery, RangesQueryError> {
        let mut main_capture_id: Option<u32> = None;
        let mut start_capture_id: Option<u32> = None;
        let mut end_capture_id: Option<u32> = None;
        for (idx, capture_name) in query.capture_names().iter().enumerate() {
            if *capture_name == main_capture_name {
                let old_capture_id = main_capture_id.replace(idx as u32);
                if old_capture_id.is_some() {
                    return Err(RangesQueryError::DuplicateCapture);
                }
            } else if *capture_name == "start" {
                let old_capture_id = start_capture_id.replace(idx as u32);
                if old_capture_id.is_some() {
                    return Err(RangesQueryError::DuplicateCapture);
                }
            } else if *capture_name == "end" {
                let old_capture_id = end_capture_id.replace(idx as u32);
                if old_capture_id.is_some() {
                    return Err(RangesQueryError::DuplicateCapture);
                }
            }
        }

        Ok(RangesQuery {
            query,
            predicates,
            main_capture_id: main_capture_id.ok_or(RangesQueryError::NoRequiredCaptures)?,
            start_capture_id,
            end_capture_id,
        })
    }
}

fn collect_ranges(
    snapshot: &SyntaxSnapshot,
    query_selector: impl Fn(&Language) -> Option<Arc<RangesQuery>>,
    query_cache: &mut HashMap<LanguageId, Arc<RangesQuery>>,
    text: &[u16],
    byte_range: Range<usize>,
    use_inner: bool,
) -> Vec<((LanguageId, usize), tree_sitter::Range, usize)> {
    let mut ranges = Vec::new();
    let text_provider = RecodingUtf16TextProvider::new(text);
    for entry in &snapshot.entries {
        if byte_range.start >= entry.byte_range.end || byte_range.end <= entry.byte_range.start {
            continue;
        }
        let SyntaxSnapshotEntryContent::Parsed { language, tree } = &entry.content else {
            continue;
        };
        let query = if let Some(query) = query_cache.get(language) {
            query
        } else {
            let Ok(Some(query)) = with_language(*language, |language| query_selector(language))
            else {
                continue;
            };
            query_cache.entry(*language).or_insert(query)
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(entry.byte_range.clone());
        let mut matches = cursor.matches(
            &query.query,
            tree.root_node_with_offset(entry.byte_offset, entry.point_offset),
            &text_provider,
        );
        while let Some(query_match) = matches.next() {
            if !query
                .predicates
                .satisfies_predicates(&mut &text_provider, query_match)
            {
                continue;
            }
            let mut start_byte: Option<usize> = None;
            let mut end_byte: Option<usize> = None;
            let mut next_byte: Option<usize> = None;
            let mut start_point: Option<tree_sitter::Point> = None;
            let mut end_point: Option<tree_sitter::Point> = None;
            let nodes = query_match.nodes_for_capture_index(query.main_capture_id);
            for node in nodes {
                if start_byte.is_none_or(|b| node.start_byte() < b) {
                    start_byte = Some(node.start_byte());
                    start_point = Some(node.start_position());
                }
                if end_byte.is_none_or(|b| node.end_byte() > b) {
                    end_byte = Some(node.end_byte());
                    end_point = Some(node.end_position());
                }
                if let Some(next_node) = node.next_sibling() {
                    if next_byte.is_none_or(|b| next_node.start_byte() > b) {
                        next_byte = Some(next_node.start_byte())
                    }
                } else {
                    next_byte = Some(node.end_byte())
                }
            }
            let use_inner = use_inner
                || query
                    .query
                    .property_settings(query_match.pattern_index)
                    .iter()
                    .any(|p| p.key.as_ref() == "range.inner");
            for capture in query_match.captures {
                if Some(capture.index) == query.start_capture_id {
                    if use_inner {
                        start_byte = Some(capture.node.end_byte());
                        start_point = Some(capture.node.end_position());
                    } else {
                        start_byte = Some(capture.node.start_byte());
                        start_point = Some(capture.node.start_position());
                    }
                } else if Some(capture.index) == query.end_capture_id {
                    if use_inner {
                        end_byte = Some(capture.node.start_byte());
                        end_point = Some(capture.node.start_position());
                        next_byte = Some(capture.node.start_byte());
                    } else {
                        end_byte = Some(capture.node.end_byte());
                        end_point = Some(capture.node.end_position());
                        if let Some(next_node) = capture.node.next_sibling() {
                            next_byte = Some(next_node.start_byte())
                        } else {
                            next_byte = Some(capture.node.end_byte())
                        }
                    }
                }
            }
            if let (
                Some(start_byte),
                Some(end_byte),
                Some(start_point),
                Some(end_point),
                Some(next_byte),
            ) = (start_byte, end_byte, start_point, end_point, next_byte)
            {
                ranges.push((
                    (*language, query_match.pattern_index),
                    tree_sitter::Range {
                        start_byte,
                        end_byte,
                        start_point,
                        end_point,
                    },
                    next_byte,
                ));
            }
        }
    }
    ranges
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeRangesProvider_nativeGetIndentRanges<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    snapshot: JObject<'local>,
    text: JCharArray<'local>,
    start_offset: jint,
    end_offset: jint,
    use_inner: jboolean,
) -> JObjectArray<'local> {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        snapshot: JObject<'local>,
        text: JCharArray<'local>,
        start_offset: jint,
        end_offset: jint,
        use_inner: jboolean,
    ) -> JNIResult<JObjectArray<'local>> {
        let snapshot = SyntaxSnapshotDesc::from_java_object(env, snapshot)?;
        let range_desc = RangeDesc::new(env)?;
        let text_length = env.get_array_length(&text)?;
        let mut text_buffer = vec![0u16; text_length as usize];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;

        let use_inner = use_inner != 0;
        let mut query_cache = HashMap::new();
        let ranges = collect_ranges(
            snapshot,
            |l| l.parser_info().indents_query.clone(),
            &mut query_cache,
            &text_buffer,
            ((start_offset * 2) as usize)..((end_offset * 2) as usize),
            use_inner,
        );

        let ranges_array =
            env.new_object_array(ranges.len() as jsize, &range_desc.class, JObject::null())?;
        for (index, (_, range, _)) in ranges.into_iter().enumerate() {
            let range_obj = range_desc.to_java_object(env, range)?;
            let range_obj = env.auto_local(range_obj);
            env.set_object_array_element(&ranges_array, index as i32, range_obj)?;
        }
        Ok(ranges_array)
    }
    let result = inner(
        &mut env,
        snapshot,
        text,
        start_offset,
        end_offset,
        use_inner,
    );
    throw_exception_from_result(&mut env, result)
}

static FOLD_RANGE_CONSTRUCTOR: JOnceLock<JMethodID> = JOnceLock::new();

struct FoldRangeDesc<'local> {
    constructor: JMethodID,
    class: AutoLocal<'local, JClass<'local>>,
    range_desc: RangeDesc<'local>,
}

impl<'local> FoldRangeDesc<'local> {
    fn new(env: &mut JNIEnv<'local>) -> JNIResult<FoldRangeDesc<'local>> {
        let range_desc = RangeDesc::new(env)?;
        let class = env.find_class("com/hulylabs/treesitter/language/FoldRange")?;
        let constructor = *FOLD_RANGE_CONSTRUCTOR.get_or_try_init(|| {
            env.get_method_id(
                &class,
                "<init>",
                "(Lcom/hulylabs/treesitter/language/Range;Ljava/lang/String;Z)V",
            )
        })?;

        Ok(FoldRangeDesc {
            constructor,
            class: env.auto_local(class),
            range_desc,
        })
    }

    fn to_java_object(
        &self,
        env: &mut JNIEnv<'local>,
        range: tree_sitter::Range,
        collapsed_text: Option<impl Into<JNIString>>,
        collapsed_by_default: bool,
    ) -> JNIResult<JObject<'local>> {
        let range_obj = self.range_desc.to_java_object(env, range)?;
        let range_obj = env.auto_local(range_obj);
        let collapsed_text: JObject = if let Some(collapsed_text) = collapsed_text {
            env.new_string(collapsed_text)?.into()
        } else {
            JObject::null()
        };
        let collapsed_text = env.auto_local(collapsed_text);
        unsafe {
            env.new_object_unchecked(
                &self.class,
                self.constructor,
                &[
                    JValue::Object(&range_obj).as_jni(),
                    JValue::Object(&collapsed_text).as_jni(),
                    JValue::from(collapsed_by_default).as_jni(),
                ],
            )
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeRangesProvider_nativeGetFoldRanges<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    snapshot: JObject<'local>,
    text: JCharArray<'local>,
    start_offset: jint,
    end_offset: jint,
    use_inner: jboolean,
) -> JObjectArray<'local> {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        snapshot: JObject<'local>,
        text: JCharArray<'local>,
        start_offset: jint,
        end_offset: jint,
        use_inner: jboolean,
    ) -> JNIResult<JObjectArray<'local>> {
        let snapshot = SyntaxSnapshotDesc::from_java_object(env, snapshot)?;
        let fold_range_desc = FoldRangeDesc::new(env)?;
        let text_length = env.get_array_length(&text)?;
        let mut text_buffer = vec![0u16; text_length as usize];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;

        let use_inner = use_inner != 0;
        let mut query_cache = HashMap::new();
        let ranges = collect_ranges(
            snapshot,
            |l| l.parser_info().folds_query.clone(),
            &mut query_cache,
            &text_buffer,
            ((start_offset * 2) as usize)..((end_offset * 2) as usize),
            use_inner,
        );
        let mut combined_ranges: Vec<(usize, tree_sitter::Range, bool, Option<&str>, usize)> =
            Vec::new();
        let mut last_combined_idx: HashMap<usize, usize> = HashMap::new();
        'outer: for ((language_id, pattern_id), range, next_byte) in ranges {
            let query = query_cache
                .get(&language_id)
                .expect("query exists in cache if returned from collect_ranges");
            let mut collapsed_text = None;
            let mut collapsed_by_default = false;
            let properties = query.query.property_settings(pattern_id as usize);
            for property in properties {
                if property.key.as_ref() == "fold.text" {
                    collapsed_text = property.value.as_ref().map(|t| t.as_ref());
                }
                if property.key.as_ref() == "fold.collapsed" {
                    collapsed_by_default = true;
                }
                if property.key.as_ref() == "fold.combined-lines" {
                    if let Some((_, last_range, _, _, last_next_byte)) = last_combined_idx
                        .get(&pattern_id)
                        .and_then(|idx| combined_ranges.get_mut(*idx))
                    {
                        if *last_next_byte == range.start_byte
                            && range.start_point.column == last_range.start_point.column
                            && (last_range.end_point.row + 1 == range.start_point.row
                                || last_range.end_point.row == range.start_point.row)
                        {
                            last_range.end_byte = range.end_byte;
                            last_range.end_point = range.end_point;
                            *last_next_byte = next_byte;
                            continue 'outer;
                        }
                    }
                    last_combined_idx.insert(pattern_id, combined_ranges.len());
                }
            }
            combined_ranges.push((
                pattern_id,
                range,
                collapsed_by_default,
                collapsed_text,
                next_byte,
            ));
        }
        let ranges_array = env.new_object_array(
            combined_ranges.len() as jsize,
            &fold_range_desc.class,
            JObject::null(),
        )?;
        for (index, (_, mut range, collapsed_by_default, collapsed_text, _)) in
            combined_ranges.into_iter().enumerate()
        {
            // Some nodes may include newline at the end, but folds should not end with newline
            if text_buffer[range.end_byte / 2 - 1] == '\n' as u16 {
                range.end_byte -= 1;
                range.end_point.row -= 1;
                let line_end_offset = range.end_byte / 2 - 1;
                let mut offset = line_end_offset;
                let line_start_offset = loop {
                    let new_offset = offset.saturating_sub(1);
                    if text_buffer[new_offset] == ('\n' as u16) || new_offset == 0 {
                        break offset;
                    }
                    offset = new_offset;
                };
                range.end_point.column = char::decode_utf16(
                    text_buffer[line_start_offset..line_start_offset]
                        .iter()
                        .copied(),
                )
                .count();
            }
            let obj =
                fold_range_desc.to_java_object(env, range, collapsed_text, collapsed_by_default)?;
            let obj = env.auto_local(obj);
            env.set_object_array_element(&ranges_array, index as i32, obj)?;
        }

        Ok(ranges_array)
    }
    let result = inner(
        &mut env,
        snapshot,
        text,
        start_offset,
        end_offset,
        use_inner,
    );
    throw_exception_from_result(&mut env, result)
}
