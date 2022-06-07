use log::{debug, trace};

#[cfg(target_os="android")]
fn configure_tokio_builder_for_android_jvm(builder: &mut tokio::runtime::Builder) {
    debug!("create_tokio_runtime_builder_android");

    let ctx = ndk_context::android_context();
    let jvm_ptr = ctx.vm();
    let thread_attach_jvm = unsafe { jni::JavaVM::from_raw(jvm_ptr.cast()).unwrap() };
    let thread_detach_jvm = unsafe { jni::JavaVM::from_raw(jvm_ptr.cast()).unwrap() };

    // To seamlessly allow us to call into Java via JNI within async rust code we need to ensure
    // that all Tokio Runtime threads get associated with a JNIEnv...

    builder.on_thread_start(move || {
        let thread_id = std::thread::current().id();
        debug!("JVM: Attaching tokio thread ({thread_id:?})");
        thread_attach_jvm.attach_current_thread_permanently().unwrap();
    });
    builder.on_thread_stop(move || {
        thread_detach_jvm.detach_current_thread();
        let thread_id = std::thread::current().id();
        debug!("JVM: Detached tokio thread ({thread_id:?}");
    });
}

fn create_tokio_runtime_builder_default() -> tokio::runtime::Builder {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_time();

    builder
}

fn create_tokio_runtime_builder() -> tokio::runtime::Builder {
    let mut builder = create_tokio_runtime_builder_default();

    #[cfg(target_os="android")]
    configure_tokio_builder_for_android_jvm(&mut builder);

    builder
}

pub fn build_tokio_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    create_tokio_runtime_builder().build()
}