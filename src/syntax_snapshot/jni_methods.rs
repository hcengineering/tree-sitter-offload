use once_cell::sync::OnceCell as JOnceLock;

use jni::{
    errors::{Error as JNIError, Result as JNIResult},
    objects::{AutoLocal, JCharArray, JClass, JFieldID, JMethodID, JObject, JValue},
    signature::{Primitive, ReturnType},
    JNIEnv,
};

use crate::{
    jni_utils::{throw_exception_from_result, PointDesc, RangeDesc},
    language_registry::LanguageId,
};

use super::SyntaxSnapshot;

struct SyntaxSnapshotDescInner {
    constructor: JMethodID,
    handle_field: JFieldID,
}

pub struct SyntaxSnapshotDesc<'local> {
    inner: &'static SyntaxSnapshotDescInner,
    class: AutoLocal<'local, JClass<'local>>,
}

static SYNTAX_SNAPSHOT: JOnceLock<SyntaxSnapshotDescInner> = JOnceLock::new();

impl<'local> SyntaxSnapshotDesc<'local> {
    fn from_class(
        env: &mut JNIEnv<'local>,
        class: JClass<'local>,
    ) -> JNIResult<SyntaxSnapshotDesc<'local>> {
        Ok(SyntaxSnapshotDesc {
            inner: SYNTAX_SNAPSHOT.get_or_try_init(|| {
                let constructor = env.get_method_id(&class, "<init>", "(JJ)V")?;
                let handle_field = env.get_field_id(&class, "handle", "J")?;
                Ok::<_, JNIError>(SyntaxSnapshotDescInner {
                    constructor,
                    handle_field,
                })
            })?,
            class: env.auto_local(class),
        })
    }

    fn from_obj_class(
        env: &mut JNIEnv<'local>,
        obj: &JObject<'local>,
    ) -> JNIResult<SyntaxSnapshotDesc<'local>> {
        let class = env.get_object_class(obj)?;
        SyntaxSnapshotDesc::from_class(env, class)
    }

    pub fn to_java_object(
        &self,
        env: &mut JNIEnv<'local>,
        base_language_id: LanguageId,
        snapshot: SyntaxSnapshot,
    ) -> JNIResult<JObject<'local>> {
        let wrapped = Box::new(snapshot);
        let ptr = Box::into_raw(wrapped);
        // SAFETY: constructor is valid and derived from class by construction of self
        unsafe {
            env.new_object_unchecked(
                &self.class,
                self.inner.constructor,
                &[
                    JValue::Long(ptr as i64).as_jni(),
                    JValue::from(base_language_id).as_jni(),
                ],
            )
        }
    }

    fn ref_from_java_object_impl(
        &self,
        env: &mut JNIEnv<'local>,
        snapshot: JObject<'local>,
    ) -> JNIResult<&'local SyntaxSnapshot> {
        let handle = env.get_field_unchecked(
            &snapshot,
            self.inner.handle_field,
            ReturnType::Primitive(Primitive::Long),
        )?;
        let handle = handle.j()? as *mut SyntaxSnapshot;
        // SAFETY: handle is expected to be created from Box raw ptr; handle is not freed for
        // lifetime of snapshot (duration of JNI call)
        let handle = unsafe { handle.as_ref() }
            .ok_or(JNIError::NullPtr("Snapshot handle expected to be non-null"))?;
        Ok(handle)
    }

    pub fn from_java_object(
        env: &mut JNIEnv<'local>,
        snapshot: JObject<'local>,
    ) -> JNIResult<&'local SyntaxSnapshot> {
        SyntaxSnapshotDesc::from_obj_class(env, &snapshot)?.ref_from_java_object_impl(env, snapshot)
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeSyntaxSnapshot_nativeParse<
    'local,
>(
    mut env: JNIEnv<'local>,
    class: JClass<'local>,
    text: JCharArray<'local>,
    base_language_id: LanguageId,
) -> JObject<'local> {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        class: JClass<'local>,
        text: JCharArray<'local>,
        base_language_id: LanguageId,
    ) -> JNIResult<JObject<'local>> {
        let text_length = env.get_array_length(&text)? as usize;
        let mut text_buffer = vec![0u16; text_length];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;
        let Some(snapshot) = SyntaxSnapshot::parse(base_language_id, &text_buffer) else {
            return Ok(JObject::null());
        };
        SyntaxSnapshotDesc::from_class(env, class)?.to_java_object(env, base_language_id, snapshot)
    }
    let result = inner(&mut env, class, text, base_language_id);
    throw_exception_from_result(&mut env, result)
}

static PAIR_METHODS: JOnceLock<PairMethods> = JOnceLock::new();
struct PairMethods {
    constructor: JMethodID,
}

struct PairDesc<'local> {
    methods: &'static PairMethods,
    class: AutoLocal<'local, JClass<'local>>,
}

impl<'local> PairDesc<'local> {
    fn new(env: &mut JNIEnv<'local>) -> JNIResult<PairDesc<'local>> {
        let class = env.find_class("kotlin/Pair")?;
        let class = env.auto_local(class);
        let methods = PAIR_METHODS.get_or_try_init(|| {
            Ok::<_, JNIError>(PairMethods {
                constructor: env.get_method_id(
                    &class,
                    "<init>",
                    "(Ljava/lang/Object;Ljava/lang/Object;)V",
                )?,
            })
        })?;
        Ok(PairDesc { methods, class })
    }

    fn to_java_object(
        &self,
        env: &mut JNIEnv<'local>,
        pair: (JObject<'local>, JObject<'local>),
    ) -> JNIResult<JObject<'local>> {
        // SAFETY: constructor is valid and derived from class by construction of self
        unsafe {
            env.new_object_unchecked(
                &self.class,
                self.methods.constructor,
                &[
                    JValue::Object(&pair.0).as_jni(),
                    JValue::Object(&pair.1).as_jni(),
                ],
            )
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeSyntaxSnapshot_nativeParseWithOld<
    'local,
>(
    mut env: JNIEnv<'local>,
    class: JClass<'local>,
    text: JCharArray<'local>,
    old_snapshot: JObject<'local>,
    edit: JObject<'local>,
) -> JObject<'local> {
    fn inner<'local>(
        env: &mut JNIEnv<'local>,
        class: JClass<'local>,
        text: JCharArray<'local>,
        old_snapshot: JObject<'local>,
        edit: JObject<'local>,
    ) -> JNIResult<JObject<'local>> {
        let desc = SyntaxSnapshotDesc::from_class(env, class)?;
        let old_snapshot = desc.ref_from_java_object_impl(env, old_snapshot)?;
        let text_length = env.get_array_length(&text)? as usize;
        let mut text_buffer = vec![0u16; text_length];
        env.get_char_array_region(&text, 0, &mut text_buffer)?;
        let edit = InputEditMethods::from_java_object(env, &edit)?;
        let Some((snapshot, changed_ranges)) =
            SyntaxSnapshot::parse_incremental(&text_buffer, old_snapshot, edit)
        else {
            return Ok(JObject::null());
        };
        let range_desc = RangeDesc::new(env)?;
        let array = env.new_object_array(
            changed_ranges.len() as i32,
            &range_desc.class,
            JObject::null(),
        )?;
        for (idx, range) in changed_ranges.into_iter().enumerate() {
            let range_obj = range_desc.to_java_object(env, range)?;
            let range_obj = env.auto_local(range_obj);
            env.set_object_array_element(&array, idx as i32, &range_obj)?;
        }
        let pair_desc = PairDesc::new(env)?;
        let snapshot = desc.to_java_object(env, snapshot.base_language(), snapshot)?;
        pair_desc.to_java_object(env, (snapshot, array.into()))
    }
    let result = inner(&mut env, class, text, old_snapshot, edit);
    throw_exception_from_result(&mut env, result)
}

#[no_mangle]
pub extern "system" fn Java_com_hulylabs_treesitter_rusty_TreeSitterNativeSyntaxSnapshot_nativeDestroy<
    'local,
>(
    mut _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: i64,
) {
    let ptr = handle as *mut SyntaxSnapshot;
    // SAFETY: handle is created from Box::into_raw, called by java GC when no other reference to
    // it exists
    std::mem::drop(unsafe { Box::from_raw(ptr) });
}

static INPUT_EDIT_METHODS: JOnceLock<InputEditMethods> = JOnceLock::new();

struct InputEditMethods {
    start_offset: JMethodID,
    old_end_offset: JMethodID,
    new_end_offset: JMethodID,
    start_point: JMethodID,
    old_end_point: JMethodID,
    new_end_point: JMethodID,
}

impl InputEditMethods {
    fn from_obj_class<'local>(
        env: &mut JNIEnv<'local>,
        obj: &JObject<'local>,
    ) -> JNIResult<&'static InputEditMethods> {
        let class = env.auto_local(env.get_object_class(obj)?);
        const OFFSET_GETTER_SIG: &str = "()I";
        const POINT_GETTER_SIG: &str = "()Lcom/hulylabs/treesitter/language/Point;";
        INPUT_EDIT_METHODS.get_or_try_init(|| {
            Ok(InputEditMethods {
                start_offset: env.get_method_id(&class, "getStartOffset", OFFSET_GETTER_SIG)?,
                old_end_offset: env.get_method_id(&class, "getOldEndOffset", OFFSET_GETTER_SIG)?,
                new_end_offset: env.get_method_id(&class, "getNewEndOffset", OFFSET_GETTER_SIG)?,
                start_point: env.get_method_id(&class, "getStartPoint", POINT_GETTER_SIG)?,
                old_end_point: env.get_method_id(&class, "getOldEndPoint", POINT_GETTER_SIG)?,
                new_end_point: env.get_method_id(&class, "getNewEndPoint", POINT_GETTER_SIG)?,
            })
        })
    }

    fn call_offset_method<'local>(
        &self,
        env: &mut JNIEnv<'local>,
        obj: &JObject<'local>,
        method_id: JMethodID,
    ) -> JNIResult<usize> {
        // SAFETY: method_id is valid and derived from class by construction of self
        Ok((unsafe {
            env.call_method_unchecked(obj, method_id, ReturnType::Primitive(Primitive::Int), &[])
        })?
        .i()? as usize
            * 2)
    }

    fn call_point_method<'local>(
        &self,
        env: &mut JNIEnv<'local>,
        obj: &JObject<'local>,
        method_id: JMethodID,
    ) -> JNIResult<tree_sitter::Point> {
        // SAFETY: method_id is valid and derived from class by construction of self
        let point_obj = unsafe {
            env.call_method_unchecked(obj, method_id, ReturnType::Object, &[])?
                .l()?
        };
        PointDesc::from_java_object(env, &point_obj)
    }

    pub fn from_java_object<'local>(
        env: &mut JNIEnv<'local>,
        edit: &JObject<'local>,
    ) -> JNIResult<tree_sitter::InputEdit> {
        let desc = InputEditMethods::from_obj_class(env, edit)?;
        let start_byte = desc.call_offset_method(env, edit, desc.start_offset)?;
        let old_end_byte = desc.call_offset_method(env, edit, desc.old_end_offset)?;
        let new_end_byte = desc.call_offset_method(env, edit, desc.new_end_offset)?;
        let start_position = desc.call_point_method(env, edit, desc.start_point)?;
        let old_end_position = desc.call_point_method(env, edit, desc.old_end_point)?;
        let new_end_position = desc.call_point_method(env, edit, desc.new_end_point)?;
        Ok(tree_sitter::InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        })
    }
}
