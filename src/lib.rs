use std::{ffi::c_void, sync::OnceLock};

use jni::{sys::jint, JavaVM};

mod highlighting_lexer;
mod language_registry;

pub(crate) static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

extern "system" {
    // Linked from tree-sitter-ng, registers native methods for it
    fn tree_sitter_ng_JNI_OnLoad(vm: *mut jni::sys::JavaVM, reserved: *const c_void) -> jint;
}

#[no_mangle]
pub extern "system" fn JNI_OnLoad(vm: JavaVM, reserved: *const c_void) -> jint {
    let val = unsafe { tree_sitter_ng_JNI_OnLoad(vm.get_java_vm_pointer(), reserved) };

    JAVA_VM.set(vm).unwrap();

    jni::sys::JNI_VERSION_1_2.max(val)
}
