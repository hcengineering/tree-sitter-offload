use std::{
    mem::transmute,
    ops::{Deref, DerefMut},
    str,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, LazyLock, RwLock,
    },
    usize,
};

use crossbeam_utils::sync::ShardedLock;
use jni::{
    objects::{JByteArray, JClass, JObject, JObjectArray, JString},
    sys::{jlong, jsize},
    JNIEnv,
};
use tree_sitter::Query;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LanguageId(jlong);

static LANGUAGE_ID_COUNTER: AtomicI64 = AtomicI64::new(0);
pub(crate) static LANGUAGE_REGISTRY: LazyLock<RwLock<LanguageRegistry>> =
    LazyLock::new(|| RwLock::default());

impl LanguageId {
    fn new() -> LanguageId {
        LanguageId(LANGUAGE_ID_COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

pub struct LanguageParserInfo {
    pub(crate) highlights_query: Option<Arc<tree_sitter::Query>>,
}

pub struct Language {
    id: LanguageId,
    name: String,
    ts_language: Arc<tree_sitter::Language>,
    parser_info: ShardedLock<LanguageParserInfo>,
}

impl Language {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn parser_info(&self) -> impl Deref<Target = LanguageParserInfo> + use<'_> {
        self.parser_info.read().unwrap()
    }

    pub(crate) fn parser_info_mut(&self) -> impl DerefMut<Target = LanguageParserInfo> + use<'_> {
        self.parser_info.write().unwrap()
    }
}

#[derive(Default)]
pub struct LanguageRegistry {
    languages: Vec<Language>,
}

impl LanguageRegistry {
    pub fn language(&self, language_id: LanguageId) -> Option<&Language> {
        self.languages.iter().find(|l| l.id == language_id)
    }

    pub fn language_by_name(&self, language_name: &str) -> Option<&Language> {
        self.languages.iter().find(|l| l.name == language_name)
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeLanguageRegistry_nativeRegisterLanguage<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
    language: JObject<'local>,
) -> LanguageId {
    let name = env
        .get_string(&name)
        .expect("valid string from java interface");
    let name = name.into();
    let language_handle = env
        .call_method(&language, "getPtr", "()J", &[])
        .expect("TSLanguage has getPtr method")
        .j()
        .expect("getPtr returns long");
    let ts_language = language_handle as *const tree_sitter::ffi::TSLanguage;
    // SAFETY: TSParser language from java has valid language_handle from linked tree-sitter
    let ts_language = unsafe {
        // Copy language so it can be freed by rust
        let ts_language = tree_sitter::ffi::ts_language_copy(ts_language);
        tree_sitter::Language::from_raw(ts_language)
    };
    let id = LanguageId::new();
    let parser_info = ShardedLock::new(LanguageParserInfo {
        highlights_query: None,
    });

    let mut registry = LANGUAGE_REGISTRY.write().unwrap();
    registry.languages.push(Language {
        id,
        name,
        ts_language: Arc::new(ts_language),
        parser_info,
    });
    id
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeLanguageRegistry_nativeAddHighlightQuery<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    language_id: LanguageId,
    query_data: JByteArray<'local>,
) -> JObjectArray<'local> {
    let Some(ts_language) = LANGUAGE_REGISTRY
        .read()
        .unwrap()
        .language(language_id)
        .map(|l| Arc::clone(&l.ts_language))
    else {
        env.throw_new("java/lang/IllegalArgumentException", "invalid language id")
            .unwrap();
        return JObjectArray::default();
    };
    let query_size = env.get_array_length(&query_data).unwrap() as usize;
    let mut query_buffer = vec![0i8; query_size];
    env.get_byte_array_region(&query_data, 0, &mut query_buffer)
        .expect("array fits the buffer");
    // SAFETY: transmute from &[i8] to &[u8] is valid
    let query_slice = unsafe { transmute(query_buffer.as_slice()) };
    let Ok(query_str) = str::from_utf8(query_slice) else {
        env.throw_new(
            "java/lang/IllegalArgumentException",
            "query data is not valid utf-8",
        )
        .unwrap();
        return JObjectArray::default();
    };
    let query = match Query::new(&ts_language, query_str) {
        Ok(query) => query,
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
            return JObjectArray::default();
        }
    };
    let query = Arc::new(query);
    LANGUAGE_REGISTRY
        .write()
        .unwrap()
        .language(language_id)
        .expect("already checked that language exists")
        .parser_info_mut()
        .highlights_query = Some(Arc::clone(&query));
    let capture_names = query.capture_names();
    let capture_names_array = env
        .new_object_array(
            capture_names.len() as jsize,
            "java/lang/String",
            JString::default(),
        )
        .unwrap();
    let mut index = 0i32;
    for capture_name in capture_names {
        let capture_name = env.new_string(capture_name).unwrap();
        env.set_object_array_element(&capture_names_array, index, capture_name)
            .unwrap();
        index += 1;
    }
    capture_names_array
}
