#![allow(unused)]

use anyhow::anyhow;
use jni::objects::{JMethodID, JObject, JValue, JValueOwned};
use std::{borrow::Cow, marker::PhantomData};

use crate::{Error, Result};

/*
// jni-rs associates a lifetime with the JMethodID type which effectively makes them uncacheable
// which is really their main purpose. As a workaround we have our own wrapper JMethodID that
// steals the inner jmethodID from jni-rs, removes the lifetime and also implements Send + Sync
// so that the method IDs can be shared between threads (which the JNI spec allows)
//
// Ref: https://github.com/jni-rs/jni-rs/issues/270

#[derive(Clone, Copy)]
pub struct JMethodID {
    id: jni::sys::jmethodID
}
unsafe impl Send for JMethodID {}
unsafe impl Sync for JMethodID {}

impl JMethodID {
    /// Shorthand way of casting back to jni-rs type
    pub fn id<'a>(self) -> jni::objects::JMethodID<'a> {
        jni::objects::JMethodID::from(self.id)
    }
}

impl From<jni::objects::JMethodID<'_>> for JMethodID {
    fn from(id: jni::objects::JMethodID) -> Self {
        Self {
            id: id.into_inner()
        }
    }
}
*/

// Considering that jni-rs will convert exceptions into an Err() but notably does _not_
// clear any pending exception we are careful to use our own method call wrappers that
// will also clear exceptions. Since this behaviour is conceptually like we are wrapping
// calls in an implicit try {} these functions have a 'try_' prefix.
//
// Note: the jni-rs crate only provides general call_method or call_method_unchecked
// functions which internally match on the return type to determine the right jni Call*Method
// function. Since we are providing making our own wrappers, actually I think it could be
// nicer if jni-rs gave a thinner jni-rs binding that could avoid that dynamic matching
//

// TODO: also support extracting info from the Java exception to include in the mapped
// Rust error.
macro_rules! catch_jni_exception {
    ($env:expr, $result:expr) => {
        match $result {
            Err(jni::errors::Error::JavaException) => {
                $env.exception_describe()?;
                $env.exception_clear()?;
                Err(Error::Other(anyhow!("JNI: Exception")))
            }
            Err(err) => Err(Error::from(err)),
            Ok(ok) => Ok(ok),
        }
    };
}

macro_rules! call_primitive_method_with_exception_check {
    ( $env:expr, $obj:expr, $method:expr, $prim_type:tt, $value_type:tt, $args:expr) => {
        if let JValueOwned::$value_type(value) = catch_jni_exception!(
            $env,
            $env.call_method_unchecked(
                $obj,
                $method,
                jni::signature::ReturnType::Primitive(jni::signature::Primitive::$prim_type),
                $args
            )
        )? {
            Ok(value)
        } else {
            Err(Error::Other(anyhow!("JNI: unexpected return type")))
        }
    };
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_bool_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<bool> {
    unsafe {
        match call_primitive_method_with_exception_check!(env, obj, method_id, Boolean, Bool, args) {
            Ok(status) => Ok(status == jni::sys::JNI_TRUE),
            Err(err) => Err(err),
        }
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_int_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<jni::sys::jint> {
    unsafe {
        call_primitive_method_with_exception_check!(env, obj, method_id, Int, Int, args)
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_long_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<jni::sys::jlong> {
    unsafe {
        call_primitive_method_with_exception_check!(env, obj, method_id, Long, Long, args)
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_float_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<jni::sys::jfloat> {
    unsafe {
        call_primitive_method_with_exception_check!(env, obj, method_id, Float, Float, args)
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_void_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<()> {
    unsafe {
        if let JValueOwned::Void = catch_jni_exception!(
            env,
            env.call_method_unchecked(
                obj,
                method_id,
                jni::signature::ReturnType::Primitive(jni::signature::Primitive::Void),
                args
            )
        )? {
            Ok(())
        } else {
            Err(Error::Other(anyhow!("JNI: unexpected return type")))
        }
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_string_method(
    env: &mut jni::JNIEnv, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<Option<String>> {
    unsafe {
        if let JValueOwned::Object(obj) = catch_jni_exception!(
            env,
            env.call_method_unchecked(obj, method_id, jni::signature::ReturnType::Object, args)
        )? {
            if obj.is_null() {
                return Ok(None);
            }
            let jstring = jni::objects::JString::from(obj);
            let js = env.get_string(&jstring)?;
            let s = js.to_str().map_err(|err| {
                let lossy_s = js.to_string_lossy().to_string();
                Error::Other(anyhow!(
                    "JNI: invalid utf8 for returned String: {:?}, lossy = {}",
                    err,
                    lossy_s
                ))
            })?;
            Ok(Some(s.to_string()))
        } else {
            Err(Error::Other(anyhow!("JNI: unexpected return type")))
        }
    }
}

// FIXME: this isn't necessarily safe because we don't cross check that the
// arguments are consistent with the signature
pub fn try_call_object_method<'local>(
    env: &mut jni::JNIEnv<'local>, obj: &JObject, method_id: JMethodID, args: &[jni::sys::jvalue],
) -> Result<JObject<'local>> {
    unsafe {
        if let JValueOwned::Object(obj) = catch_jni_exception!(
            env,
            env.call_method_unchecked(obj, method_id, jni::signature::ReturnType::Object, args)
        )? {
            Ok(obj)
        } else {
            Err(Error::Other(anyhow!("JNI: unexpected return type")))
        }
    }
}

/// Lets us pass a handle to a Rust allocation via JNI (as a jlong)
#[repr(transparent)]
//#[derive(Clone, Copy, Debug)]
#[derive(Debug)]
pub struct JHandle<T: ?Sized + Sync> {
    pub handle: jni::sys::jlong,
    //pub ptr: *const std::ffi::c_void,
    pub phantom: PhantomData<T>,
}

impl<T: ?Sized + Sync> JHandle<T> {
    pub fn as_jni(&self) -> jni::sys::jvalue {
        jni::sys::jvalue { j: self.handle }
    }
}
unsafe impl<T: ?Sized + Sync> Send for JHandle<T> {}
impl<T: ?Sized + Sync> Default for JHandle<T> {
    fn default() -> Self {
        Self {
            handle: 0 as jni::sys::jlong,
            //ptr: 0 as *const std::ffi::c_void,
            phantom: PhantomData,
        }
    }
}

impl<T: ?Sized + Sync> From<JHandle<T>> for jni::sys::jlong {
    fn from(handle: JHandle<T>) -> Self {
        handle.handle
    }
}

impl<'a, T: ?Sized + Sync> From<JHandle<T>> for JValueOwned<'a> {
    fn from(handle: JHandle<T>) -> Self {
        JValueOwned::Long(handle.handle)
    }
}

// For some reason #[derive(Clone, Copy)] isn't working!?
impl<T: ?Sized + Sync> Clone for JHandle<T> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle,
            phantom: PhantomData,
        }
    }
}
impl<T: ?Sized + Sync> Copy for JHandle<T> {}

pub trait IntoJHandle: Sync + Sized {
    unsafe fn into_weak_handle(this: Self) -> JHandle<Self>;
    unsafe fn clone_from_weak_handle(handle: JHandle<Self>) -> Option<Self>;
    unsafe fn drop_weak_handle(handle: JHandle<Self>);
}
