use jni::{
    errors::{Error as JNIError, Result as JNIResult},
    JNIEnv,
};

pub fn throw_exception_from_result<'local, T: Default>(
    env: &mut JNIEnv<'local>,
    result: JNIResult<T>,
) -> T {
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
