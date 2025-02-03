use jni::{
    errors::{Error as JNIError, Result as JNIResult},
    objects::{AutoLocal, JClass, JMethodID, JObject, JValue},
    signature::{Primitive, ReturnType},
    JNIEnv,
};
use once_cell::sync::OnceCell as JOnceLock;

pub fn throw_exception_from_result<T: Default>(env: &mut JNIEnv<'_>, result: JNIResult<T>) -> T {
    match result {
        Ok(val) => val,
        Err(JNIError::JavaException) => Default::default(),
        Err(err) => {
            env.throw_new(
                "java/lang/RuntimeException",
                format!("Error from JNI: {err}"),
            )
            .unwrap();
            Default::default()
        }
    }
}

static POINT_METHODS: JOnceLock<PointMethods> = JOnceLock::new();

struct PointMethods {
    constructor: JMethodID,
    row: JMethodID,
    column: JMethodID,
}

pub struct PointDesc<'local> {
    methods: &'static PointMethods,
    pub class: AutoLocal<'local, JClass<'local>>,
}

impl<'local> PointDesc<'local> {
    fn new(env: &mut JNIEnv<'local>) -> JNIResult<PointDesc<'local>> {
        let class = env.find_class("com/hulylabs/treesitter/language/Point")?;
        PointDesc::from_class(env, class)
    }

    fn from_class(env: &mut JNIEnv<'local>, class: JClass<'local>) -> JNIResult<PointDesc<'local>> {
        let methods = POINT_METHODS.get_or_try_init(|| {
            Ok::<_, JNIError>(PointMethods {
                constructor: env.get_method_id(&class, "<init>", "(II)V")?,
                row: env.get_method_id(&class, "getRow", "()I")?,
                column: env.get_method_id(&class, "getColumn", "()I")?,
            })
        })?;
        Ok(PointDesc {
            methods,
            class: env.auto_local(class),
        })
    }

    fn from_obj_class(
        env: &mut JNIEnv<'local>,
        obj: &JObject<'local>,
    ) -> JNIResult<PointDesc<'local>> {
        let class = env.get_object_class(obj)?;
        Self::from_class(env, class)
    }

    pub fn to_java_object(
        &self,
        env: &mut JNIEnv<'local>,
        point: &tree_sitter::Point,
    ) -> JNIResult<JObject<'local>> {
        // SAFETY: constructor is valid and derived from class by construction of self
        unsafe {
            env.new_object_unchecked(
                &self.class,
                self.methods.constructor,
                &[
                    JValue::Int(point.row as i32).as_jni(),
                    JValue::Int(point.column as i32 / 2).as_jni(),
                ],
            )
        }
    }

    pub fn from_java_object(
        env: &mut JNIEnv<'local>,
        point: &JObject<'local>,
    ) -> JNIResult<tree_sitter::Point> {
        let desc = Self::from_obj_class(env, point)?;
        Ok(tree_sitter::Point {
            // SAFETY: method_id is valid and derived from class by construction of desc
            row: unsafe {
                env.call_method_unchecked(
                    point,
                    desc.methods.row,
                    ReturnType::Primitive(Primitive::Int),
                    &[],
                )
            }?
            .i()? as usize,
            // SAFETY: method_id is valid and derived from class by construction of desc
            column: (unsafe {
                env.call_method_unchecked(
                    point,
                    desc.methods.column,
                    ReturnType::Primitive(Primitive::Int),
                    &[],
                )
            }?
            .i()? as usize)
                * 2,
        })
    }
}

static RANGE_CONSTRUCTOR: JOnceLock<JMethodID> = JOnceLock::new();

pub struct RangeDesc<'local> {
    constructor: JMethodID,
    pub class: AutoLocal<'local, JClass<'local>>,
    point_desc: PointDesc<'local>,
}

impl<'local> RangeDesc<'local> {
    pub fn new(env: &mut JNIEnv<'local>) -> JNIResult<RangeDesc<'local>> {
        let class = env.find_class("com/hulylabs/treesitter/language/Range")?;
        let constructor = *RANGE_CONSTRUCTOR.get_or_try_init(|| {
            env.get_method_id(
                &class,
                "<init>",
                "(IILcom/hulylabs/treesitter/language/Point;Lcom/hulylabs/treesitter/language/Point;)V",
            )
        })?;
        Ok(RangeDesc {
            constructor,
            class: env.auto_local(class),
            point_desc: PointDesc::new(env)?,
        })
    }

    pub fn to_java_object(
        &self,
        env: &mut JNIEnv<'local>,
        range: tree_sitter::Range,
    ) -> JNIResult<JObject<'local>> {
        let start_point = self.point_desc.to_java_object(env, &range.start_point)?;
        let start_point = env.auto_local(start_point);
        let end_point = self.point_desc.to_java_object(env, &range.end_point)?;
        let end_point = env.auto_local(end_point);
        // SAFETY: constructor is valid and derived from class by construction of self
        unsafe {
            env.new_object_unchecked(
                &self.class,
                self.constructor,
                &[
                    JValue::Int((range.start_byte / 2) as i32).as_jni(),
                    JValue::Int((range.end_byte / 2) as i32).as_jni(),
                    JValue::Object(&start_point).as_jni(),
                    JValue::Object(&end_point).as_jni(),
                ],
            )
        }
    }
}
