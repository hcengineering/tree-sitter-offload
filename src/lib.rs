use std::ffi::c_void;

use jni::{sys::jint, JavaVM};

mod highlighting_lexer;
mod injections;
pub mod jni_utils;
mod language_registry;
mod predicates;
mod query;
mod ranges;
mod syntax_snapshot;

pub use injections::InjectionQuery;
pub use language_registry::{with_language, with_language_by_name, Language, LanguageId};
pub use ranges::RangesQuery;

unsafe extern "system" {
    // Linked from tree-sitter-ng, registers native methods for it
    fn tree_sitter_ng_JNI_OnLoad(vm: *mut jni::sys::JavaVM, reserved: *const c_void) -> jint;
}

/// # Safety
/// Function is called from already unsafe JNI context
#[no_mangle]
pub unsafe extern "system" fn JNI_OnLoad(vm: JavaVM, reserved: *const c_void) -> jint {
    let val = unsafe { tree_sitter_ng_JNI_OnLoad(vm.get_java_vm_pointer(), reserved) };

    jni::sys::JNI_VERSION_1_2.max(val)
}
