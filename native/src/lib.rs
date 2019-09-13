#[macro_use]
extern crate neon;

use std::sync::{Arc, RwLock};

use neon::context::Context as NeonContext;
use neon::prelude::*;

use deltachat::{
    constants::Event,
    context::{self, Context},
    job,
};

unsafe extern "C" fn callback(
    _ctx: &Context,
    event: Event,
    _data1: libc::uintptr_t,
    data2: libc::uintptr_t,
) -> libc::uintptr_t {
    match event {
        Event::INFO | Event::WARNING | Event::ERROR => {
            println!("{:?}: {}", event, unsafe {
                std::ffi::CStr::from_ptr(data2 as *const _)
                    .to_str()
                    .unwrap()
            });
        }
        _ => println!("callback ({:?})", event),
    }

    0 as libc::uintptr_t
}

pub struct ContextWrapper {
    context: Arc<RwLock<Context>>,
    running: bool,
    cb: Arc<RwLock<Option<EventHandler>>>,
}

declare_types! {
    pub class JsContext for ContextWrapper {
        init(mut cx) {
            let this = cx.this();

            let ctx = context::dc_context_new(Some(callback), std::ptr::null_mut(), None);

            let f = cx.argument::<JsFunction>(0)?;
            let cb = EventHandler::new(this, f);

            Ok(ContextWrapper {
                context: Arc::new(RwLock::new(ctx)),
                running: false,
                cb: Arc::new(RwLock::new(Some(cb))),
            })
        }

        method open(mut cx) {
            let this = cx.this();

            let dbfile = cx.argument::<JsString>(0)?.value();
            let blobdir = match cx.argument_opt(1) {
                Some(v) => Some(v.downcast::<JsString>().or_throw(&mut cx)?.value()),
                None => None,
            };

            let opened = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                unsafe {
                    context::dc_open(&ctx.context.clone().read().unwrap(), &dbfile, blobdir.as_ref().map(|s| s.as_str()))
                }
            };

            assert!(opened, "Failed to open {} - {:?}", dbfile, blobdir);

            Ok(cx.undefined().upcast())
        }

        method connect(mut cx) {
            let this = cx.this();

            // start threads
            let (context, cb) = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                (ctx.context.clone(), ctx.cb.clone())
            };

            std::thread::spawn(move || while let Some(cb) = &*cb.clone().read().unwrap() {
                let context = context.clone();

                cb.schedule(move |cx, this, callback| {
                    let args : Vec<Handle<JsValue>> = vec![cx.string("number").upcast()];
                    let _result = callback.call(cx, this, args);

                    job::perform_imap_jobs(&context.read().unwrap());
                    job::perform_imap_fetch(&context.read().unwrap());
                    std::thread::sleep(std::time::Duration::new(1, 0));
                });
            });

            Ok(cx.undefined().upcast())
        }

        method disconnect(mut cx) {
            let this = cx.this();
            {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                *ctx.cb.write().unwrap() = None;
            }

            Ok(cx.undefined().upcast())
        }
    }
}

// Export the class
register_module!(mut m, {
    m.export_class::<JsContext>("Context")?;
    Ok(())
});
