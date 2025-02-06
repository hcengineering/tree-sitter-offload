use std::{
    borrow::Cow,
    mem::transmute,
    ops::{Deref, DerefMut},
    str,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, LazyLock, RwLock,
    },
};

use bit_set::BitSet;
use crossbeam_utils::sync::ShardedLock;
use jni::{
    errors::Error as JNIError,
    objects::{JByteArray, JClass, JObject, JObjectArray, JString, JValueGen},
    sys::{jlong, jsize},
    JNIEnv,
};
use tree_sitter::Query;

use crate::{
    injections::InjectionQueryError,
    predicates::{AdditionalPredicates, PREDICATE_PARSER},
    ranges::RangesQueryError,
    InjectionQuery, RangesQuery,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LanguageId(jlong);

impl From<jlong> for LanguageId {
    fn from(value: jlong) -> Self {
        Self(value)
    }
}

impl From<LanguageId> for jlong {
    fn from(value: LanguageId) -> Self {
        value.0
    }
}

impl<O> From<LanguageId> for JValueGen<O> {
    fn from(value: LanguageId) -> Self {
        JValueGen::Long(value.0)
    }
}

static LANGUAGE_ID_COUNTER: AtomicI64 = AtomicI64::new(0);
static LANGUAGE_REGISTRY: LazyLock<RwLock<LanguageRegistry>> = LazyLock::new(RwLock::default);

impl LanguageId {
    pub const UNKNOWN: LanguageId = LanguageId(-1);
    fn new() -> LanguageId {
        LanguageId(LANGUAGE_ID_COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownLanguage {
    LanguageName(Box<str>),
    LanguageMimetype(Box<str>),
}

pub struct LanguageParserInfo {
    pub(crate) highlights_query: Option<Arc<(tree_sitter::Query, AdditionalPredicates, BitSet)>>,
    pub(crate) folds_query: Option<Arc<RangesQuery>>,
    pub(crate) indents_query: Option<Arc<RangesQuery>>,
    pub(crate) injections_query: Option<Arc<InjectionQuery>>,
}

pub struct Language {
    id: LanguageId,
    name: Box<str>,
    ts_language: Arc<tree_sitter::Language>,
    parser_info: ShardedLock<LanguageParserInfo>,
}

impl Language {
    pub fn id(&self) -> LanguageId {
        self.id
    }

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
        self.languages
            .iter()
            .find(|l| l.name.deref() == language_name)
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
    let name: Cow<'_, str> = (&name).into();
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
        name: name.into(),
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

pub fn with_language_by_name<T>(
    language_name: impl AsRef<str>,
    f: impl FnOnce(&Language) -> T,
) -> Result<T, LanguageError> {
    let registry = LANGUAGE_REGISTRY.read().unwrap();
    let language = registry
        .language_by_name(language_name.as_ref())
        .ok_or(LanguageError::InvalidLanguageId)?;
    Ok(f(language))
}

pub fn with_unknown_language<T>(
    language: &UnknownLanguage,
    f: impl FnOnce(&Language) -> T,
) -> Result<T, LanguageError> {
    if let UnknownLanguage::LanguageName(name) = language {
        with_language_by_name(name, f)
    } else {
        Err(LanguageError::InvalidLanguageId)
    }
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
    let query_slice = unsafe { transmute::<&[i8], &[u8]>(query_buffer.as_slice()) };
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
        let (query, predicates) = parse_query(env, &ts_language, query_data)?;
        let capture_names = query.capture_names();
        let mut capture_mask = BitSet::with_capacity(capture_names.len());
        for (idx, capture_name) in capture_names.iter().enumerate() {
            if !capture_name.starts_with('_') {
                capture_mask.insert(idx);
            }
        }
        let query = Arc::new((query, predicates, capture_mask));
        with_language(language_id, |language| {
            language.parser_info_mut().highlights_query = Some(Arc::clone(&query));
        })?;
        let capture_names = query.0.capture_names();
        let capture_names_array = env.new_object_array(
            capture_names.len() as jsize,
            "java/lang/String",
            JString::default(),
        )?;
        for (index, capture_name) in capture_names.iter().enumerate() {
            let capture_name = env.new_string(capture_name)?;
            env.set_object_array_element(&capture_names_array, index as i32, &capture_name)?;
            env.delete_local_ref(capture_name)?;
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

#[derive(thiserror::Error, Debug)]
enum AddRangesQueryError {
    #[error(transparent)]
    ParseError(#[from] QueryParseError),
    #[error(transparent)]
    RangesError(#[from] RangesQueryError),
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
    ) -> Result<(), AddRangesQueryError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())
            .map_err(QueryParseError::from)?;
        let (query, predicates) = parse_query(env, &ts_language, query_data)?;
        let query = RangesQuery::new(query, predicates, "fold")?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().folds_query = Some(query);
        })
        .map_err(QueryParseError::from)?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(AddRangesQueryError::ParseError(QueryParseError::JNIError(
            JNIError::JavaException,
        ))) => (),
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
    ) -> Result<(), AddRangesQueryError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())
            .map_err(QueryParseError::from)?;
        let (query, predicates) = parse_query(env, &ts_language, query_data)?;
        let query = RangesQuery::new(query, predicates, "indent")?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().indents_query = Some(query);
        })
        .map_err(QueryParseError::from)?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(AddRangesQueryError::ParseError(QueryParseError::JNIError(
            JNIError::JavaException,
        ))) => (),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum AddInjectionQueryError {
    #[error(transparent)]
    ParseError(#[from] QueryParseError),
    #[error(transparent)]
    InjectionError(#[from] InjectionQueryError),
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
    ) -> Result<(), AddInjectionQueryError> {
        let ts_language = with_language(language_id, |language| language.ts_language.clone())
            .map_err(QueryParseError::from)?;
        let (query, predicates) = parse_query(env, &ts_language, query_data)?;
        let query = InjectionQuery::new(query, predicates)?;
        let query = Arc::new(query);
        with_language(language_id, |language| {
            language.parser_info_mut().injections_query = Some(Arc::clone(&query));
        })
        .map_err(QueryParseError::from)?;
        Ok(())
    }
    let result = inner(&mut env, language_id, query_data);
    match result {
        Ok(()) => (),
        Err(AddInjectionQueryError::ParseError(QueryParseError::JNIError(
            JNIError::JavaException,
        ))) => (),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Failed to parse query: {err}"),
            )
            .unwrap();
        }
    }
}
