use std::sync::Arc;

use jni::{
    errors::Result as JNIResult,
    objects::{JCharArray, JClass, JObject, JObjectArray},
    sys::{jboolean, jint, jsize},
    JNIEnv,
};
use streaming_iterator::StreamingIterator;
use tree_sitter::QueryCursor;

use crate::{
    jni_utils::{throw_exception_from_result, RangeDesc},
    language_registry::{with_language, Language},
    predicates::AdditionalPredicates,
    query::RecodingUtf16TextProvider,
    syntax_snapshot::SyntaxSnapshotDesc,
};

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

fn collect_ranges<'local, F: FnOnce(&Language) -> Option<Arc<RangesQuery>>>(
    env: &mut JNIEnv<'local>,
    snapshot: JObject<'local>,
    text: JCharArray<'local>,
    start_offset: jint,
    end_offset: jint,
    use_inner: jboolean,
    query_selector: F,
) -> JNIResult<JObjectArray<'local>> {
    let (snapshot, base_language_id) = SyntaxSnapshotDesc::from_java_object(env, snapshot)?;
    let range_desc = RangeDesc::new(env)?;
    let Some(query) = with_language(base_language_id, query_selector)
        .ok()
        .flatten()
    else {
        let obj_array = env.new_object_array(0, &range_desc.class, JObject::null())?;
        return Ok(obj_array);
    };

    let text_length = env.get_array_length(&text)?;
    let mut text_buffer = vec![0u16; text_length as usize];
    env.get_char_array_region(&text, 0, &mut text_buffer)?;

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(((start_offset * 2) as usize)..((end_offset * 2) as usize));
    let text_provider = RecodingUtf16TextProvider::new(&text_buffer);
    let mut text_provider2 = RecodingUtf16TextProvider::new(&text_buffer);
    let mut ranges: Vec<tree_sitter::Range> = Vec::new();
    let mut matches = cursor.matches(&query.query, snapshot.tree.root_node(), text_provider);
    while let Some(query_match) = matches.next() {
        if !query
            .predicates
            .satisfies_predicates(&mut text_provider2, query_match)
        {
            continue;
        }
        let mut start_byte: Option<usize> = None;
        let mut end_byte: Option<usize> = None;
        let mut start_point: Option<tree_sitter::Point> = None;
        let mut end_point: Option<tree_sitter::Point> = None;
        for capture in query_match.captures {
            if capture.index == query.main_capture_id {
                start_byte = start_byte.or(Some(capture.node.start_byte()));
                end_byte = end_byte.or(Some(capture.node.end_byte()));
                start_point = start_point.or(Some(capture.node.start_position()));
                end_point = end_point.or(Some(capture.node.end_position()));
            } else if use_inner != 0 {
                if Some(capture.index) == query.start_capture_id {
                    start_byte = Some(capture.node.end_byte());
                    start_point = Some(capture.node.end_position());
                } else if Some(capture.index) == query.end_capture_id {
                    end_byte = Some(capture.node.start_byte());
                    end_point = Some(capture.node.start_position());
                }
            }
        }
        if let (Some(start_byte), Some(end_byte), Some(start_point), Some(end_point)) =
            (start_byte, end_byte, start_point, end_point)
        {
            ranges.push(tree_sitter::Range {
                start_byte,
                end_byte,
                start_point,
                end_point,
            });
        }
    }
    let ranges_array =
        env.new_object_array(ranges.len() as jsize, &range_desc.class, JObject::null())?;
    for (index, range) in ranges.into_iter().enumerate() {
        let range_obj = range_desc.to_java_object(env, range)?;
        let range_obj = env.auto_local(range_obj);
        env.set_object_array_element(&ranges_array, index as i32, range_obj)?;
    }
    Ok(ranges_array)
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
    let result = collect_ranges(
        &mut env,
        snapshot,
        text,
        start_offset,
        end_offset,
        use_inner,
        |language| language.parser_info().indents_query.clone(),
    );
    throw_exception_from_result(&mut env, result)
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
    let result = collect_ranges(
        &mut env,
        snapshot,
        text,
        start_offset,
        end_offset,
        use_inner,
        |language| language.parser_info().folds_query.clone(),
    );
    throw_exception_from_result(&mut env, result)
}
