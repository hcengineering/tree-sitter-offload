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
    errors::Error as JNIError,
    objects::{JByteArray, JClass, JObject, JObjectArray, JString, JValueGen},
    sys::{jlong, jsize},
    JNIEnv,
};
use tree_sitter::Query;

use crate::predicates::{AdditionalPredicates, PREDICATE_PARSER};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LanguageId(jlong);

impl From<jlong> for LanguageId {
    fn from(value: jlong) -> Self {
        Self(value)
    }
}

impl<O> From<LanguageId> for JValueGen<O> {
    fn from(value: LanguageId) -> Self {
        JValueGen::Long(value.0)
    }
}

static LANGUAGE_ID_COUNTER: AtomicI64 = AtomicI64::new(0);
static LANGUAGE_REGISTRY: LazyLock<RwLock<LanguageRegistry>> = LazyLock::new(|| RwLock::default());

impl LanguageId {
    fn new() -> LanguageId {
        LanguageId(LANGUAGE_ID_COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

pub struct LanguageParserInfo {
    pub(crate) highlights_query: Option<Arc<(tree_sitter::Query, AdditionalPredicates)>>,
    pub(crate) folds_query: Option<Arc<(tree_sitter::Query, AdditionalPredicates)>>,
    pub(crate) indents_query: Option<Arc<(tree_sitter::Query, AdditionalPredicates)>>,
    pub(crate) injections_query: Option<Arc<(tree_sitter::Query, AdditionalPredicates)>>,
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

    pub fn ts_language(&self) -> Arc<tree_sitter::Language> {
        Arc::clone(&self.ts_language.clone())
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
        folds_query: None,
        indents_query: None,
        injections_query: None,
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

#[derive(thiserror::Error, Debug)]
pub enum LanguageError {
    #[error("unknown language")]
    InvalidLanguageId,
}

pub fn with_language<T>(
    language_id: LanguageId,
    f: impl FnOnce(&Language) -> T,
) -> Result<T, LanguageError> {
    let registry = LANGUAGE_REGISTRY.read().unwrap();
    let language = registry
        .language(language_id)
        .ok_or(LanguageError::InvalidLanguageId)?;
    Ok(f(language))
}

#[derive(thiserror::Error, Debug)]
pub enum QueryParseError {
    #[error(transparent)]
    InvalideLanguage(#[from] LanguageError),
    #[error(transparent)]
    InvalidEncoding(#[from] str::Utf8Error),
    #[error("tree-sitter parse error: {0}")]
    TreeSitterError(#[from] tree_sitter::QueryError),
    #[error("jni error: {0}")]
    JNIError(#[from] JNIError),
}

fn parse_query<'local>(
    env: &mut JNIEnv<'local>,
    language: &tree_sitter::Language,
    query_data: JByteArray<'local>,
) -> Result<(Query, AdditionalPredicates), QueryParseError> {
    let query_size = env.get_array_length(&query_data)? as usize;
    let mut query_buffer = vec![0i8; query_size];
    env.get_byte_array_region(&query_data, 0, &mut query_buffer)?;
    // SAFETY: transmute from &[i8] to &[u8] is valid
    let query_slice = unsafe { transmute(query_buffer.as_slice()) };
    let query_str = str::from_utf8(query_slice)?;
    let query = Query::new(language, query_str)?;
    let additional_predicates =
        PREDICATE_PARSER.with(|parser| AdditionalPredicates::parse(&query, query_str, parser))?;
    Ok((query, additional_predicates))
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
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        language_id: LanguageId,
        query_data: JByteArray<'local>,
    ) -> Result<JObjectArray<'local>, QueryParseError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())?;
        let query = parse_query(env, &ts_language, query_data)?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().highlights_query = Some(Arc::clone(&query));
        })?;
        let capture_names = query.0.capture_names();
        let capture_names_array = env.new_object_array(
            capture_names.len() as jsize,
            "java/lang/String",
            JString::default(),
        )?;
        let mut index = 0i32;
        for capture_name in capture_names {
            let capture_name = env.new_string(capture_name)?;
            env.set_object_array_element(&capture_names_array, index, &capture_name)?;
            env.delete_local_ref(capture_name)?;
            index += 1;
        }
        Ok(capture_names_array)
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(captures) => captures,
        Err(QueryParseError::JNIError(JNIError::JavaException)) => JObjectArray::default(),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
            JObjectArray::default()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeLanguageRegistry_nativeAddFoldQuery<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    language_id: LanguageId,
    query_data: JByteArray<'local>,
) {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        language_id: LanguageId,
        query_data: JByteArray<'local>,
    ) -> Result<(), QueryParseError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())?;
        let query = parse_query(env, &ts_language, query_data)?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().folds_query = Some(Arc::clone(&query));
        })?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(QueryParseError::JNIError(JNIError::JavaException)) => (),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeLanguageRegistry_nativeAddIndentQuery<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    language_id: LanguageId,
    query_data: JByteArray<'local>,
) {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        language_id: LanguageId,
        query_data: JByteArray<'local>,
    ) -> Result<(), QueryParseError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())?;
        let query = parse_query(env, &ts_language, query_data)?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().indents_query = Some(Arc::clone(&query));
        })?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(QueryParseError::JNIError(JNIError::JavaException)) => (),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeLanguageRegistry_nativeAddInjectionQuery<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    language_id: LanguageId,
    query_data: JByteArray<'local>,
) {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        language_id: LanguageId,
        query_data: JByteArray<'local>,
    ) -> Result<(), QueryParseError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())?;
        let query = parse_query(env, &ts_language, query_data)?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().injections_query = Some(Arc::clone(&query));
        })?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(QueryParseError::JNIError(JNIError::JavaException)) => (),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
        }
    }
}
