use std::ops::Range;

use jni::{
    errors::Result as JNIResult,
    objects::{AutoLocal, JCharArray, JClass, JMethodID, JObject, JObjectArray, JValue},
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
    syntax_snapshot::{SyntaxSnapshot, SyntaxSnapshotDesc},
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
    query: &RangesQuery,
    text: &[u16],
    byte_range: Range<usize>,
    use_inner: bool,
) -> Vec<(usize, tree_sitter::Range)> {
    let mut ranges = Vec::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(byte_range);
    let text_provider = RecodingUtf16TextProvider::new(text);
    let tree = snapshot.main_tree();
    let mut matches = cursor.matches(&query.query, tree.root_node(), &text_provider);
    while let Some(query_match) = matches.next() {
        if !query
            .predicates
            .satisfies_predicates(&mut &text_provider, query_match)
        {
            continue;
        }
        let mut start_byte: Option<usize> = None;
        let mut end_byte: Option<usize> = None;
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
        }
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
                } else {
                    end_byte = Some(capture.node.end_byte());
                    end_point = Some(capture.node.end_position());
                }
            }
        }
        if let (Some(start_byte), Some(end_byte), Some(start_point), Some(end_point)) =
            (start_byte, end_byte, start_point, end_point)
        {
            ranges.push((
                query_match.pattern_index,
                tree_sitter::Range {
                    start_byte,
                    end_byte,
                    start_point,
                    end_point,
                },
            ));
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
        let Some(query) = with_language(snapshot.base_language(), |language| {
            language.parser_info().indents_query.clone()
        })
        .ok()
        .flatten() else {
            let obj_array = env.new_object_array(0, &range_desc.class, JObject::null())?;
            return Ok(obj_array);
        };
        let text_length = env.get_array_length(&text)?;
        let mut text_buffer = vec![0u16; text_length as usize];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;

        let use_inner = use_inner != 0;
        let ranges = collect_ranges(
            snapshot,
            &query,
            &text_buffer,
            ((start_offset * 2) as usize)..((end_offset * 2) as usize),
            use_inner,
        );

        let ranges_array =
            env.new_object_array(ranges.len() as jsize, &range_desc.class, JObject::null())?;
        for (index, (_, range)) in ranges.into_iter().enumerate() {
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
        collapsed_text: Option<&str>,
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
        let Some(query) = with_language(snapshot.base_language(), |language| {
            language.parser_info().folds_query.clone()
        })
        .ok()
        .flatten() else {
            let obj_array = env.new_object_array(0, &fold_range_desc.class, JObject::null())?;
            return Ok(obj_array);
        };
        let text_length = env.get_array_length(&text)?;
        let mut text_buffer = vec![0u16; text_length as usize];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;

        let use_inner = use_inner != 0;
        let ranges = collect_ranges(
            snapshot,
            &query,
            &text_buffer,
            ((start_offset * 2) as usize)..((end_offset * 2) as usize),
            use_inner,
        );

        let ranges_array = env.new_object_array(
            ranges.len() as jsize,
            &fold_range_desc.class,
            JObject::null(),
        )?;
        for (index, (pattern_id, range)) in ranges.into_iter().enumerate() {
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
