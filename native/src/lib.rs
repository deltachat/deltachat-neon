#[macro_use]
extern crate neon;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use lazy_static::lazy_static;
use neon::context::Context as NeonContext;
use neon::prelude::*;

use deltachat::{
    constants::Event,
    context::{self, Context},
    job,
};

lazy_static! {
    static ref CALLBACK: RwLock<Option<EventHandler>> = RwLock::new(None);
}

unsafe extern "C" fn callback(
    _ctx: &Context,
    event: Event,
    _data1: libc::uintptr_t,
    data2: libc::uintptr_t,
) -> libc::uintptr_t {
    let data2_string = match event {
        Event::INFO
        | Event::WARNING
        | Event::ERROR
        | Event::SMTP_CONNECTED
        | Event::IMAP_CONNECTED
        | Event::ERROR_NETWORK => {
            let data2 = data2 as *const libc::c_char;
            if data2.is_null() {
                Some("".to_string())
            } else {
                Some(
                    std::ffi::CStr::from_ptr(data2)
                        .to_string_lossy()
                        .to_string(),
                )
            }
        }
        _ => None,
    };

    if let Some(ref cb) = &*CALLBACK.read().unwrap() {
        cb.schedule(move |cx, this, callback| {
            let args: Vec<Handle<JsValue>> = match event {
                Event::INFO
                | Event::WARNING
                | Event::ERROR
                | Event::SMTP_CONNECTED
                | Event::IMAP_CONNECTED
                | Event::ERROR_NETWORK => vec![
                    cx.string(format!("{:?}", event)).upcast(),
                    cx.string(data2_string.unwrap()).upcast(),
                ],
                _ => vec![
                    cx.string(format!("{:?}", event)).upcast(),
                    cx.undefined().upcast(),
                ],
            };

            let _result = callback.call(cx, this, args);
        });
    }

    0 as libc::uintptr_t
}

pub struct ContextWrapper {
    context: Arc<RwLock<Context>>,
    running: Arc<AtomicBool>,
    handles: Option<Handles>,
}

use std::thread::JoinHandle;

struct Handles {
    imap: JoinHandle<()>,
    mvbox: JoinHandle<()>,
    sentbox: JoinHandle<()>,
    smtp: JoinHandle<()>,
}

impl Handles {
    fn new(
        imap: JoinHandle<()>,
        mvbox: JoinHandle<()>,
        sentbox: JoinHandle<()>,
        smtp: JoinHandle<()>,
    ) -> Self {
        Handles {
            imap,
            mvbox,
            sentbox,
            smtp,
        }
    }

    fn join(self) -> std::thread::Result<()> {
        let Handles {
            imap,
            mvbox,
            sentbox,
            smtp,
        } = self;
        imap.join()?;
        mvbox.join()?;
        sentbox.join()?;
        smtp.join()?;
        Ok(())
    }
}

declare_types! {
    pub class JsContext for ContextWrapper {
        init(mut cx) {
            let this = cx.this();

            let ctx = context::dc_context_new(Some(callback), std::ptr::null_mut(), None);

            let f = cx.argument::<JsFunction>(0)?;

            let cb = &mut *CALLBACK.write().unwrap();
            assert!(cb.is_none(), "Only one context supported at the moment");

            *cb = Some(EventHandler::new(this, f));

            Ok(ContextWrapper {
                context: Arc::new(RwLock::new(ctx)),
                running: Arc::new(AtomicBool::new(false)),
                handles: None,
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
            let mut this = cx.this();

            let (context, running) = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);

                ctx.running.store(true, Ordering::Relaxed);
                (ctx.context.clone(), ctx.running.clone())
            };

            // start threads
            let imap_handle = {
                let context = context.clone();
                let running = running.clone();

                std::thread::spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        {
                            let ctx = &context.read().unwrap();
                            job::perform_imap_jobs(ctx);
                            job::perform_imap_fetch(ctx);
                        }
                        while running.load(Ordering::Relaxed) {
                            job::perform_imap_idle(&context.read().unwrap());
                        }
                    }
                })
            };

            let mvbox_handle = {
                let context = context.clone();
                let running = running.clone();

                std::thread::spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        job::perform_mvbox_fetch(&context.read().unwrap());
                        while running.load(Ordering::Relaxed) {
                            job::perform_mvbox_idle(&context.read().unwrap());
                        }
                    }
                })
            };

            let sentbox_handle = {
                let context = context.clone();
                let running = running.clone();

                std::thread::spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        job::perform_sentbox_fetch(&context.read().unwrap());
                        while running.load(Ordering::Relaxed) {
                            job::perform_sentbox_idle(&context.read().unwrap());
                        }
                    }
                })
            };

            let smtp_handle = {
                let context = context.clone();
                let running = running.clone();

                std::thread::spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        job::perform_smtp_jobs(&context.read().unwrap());
                        while running.load(Ordering::Relaxed) {
                            job::perform_smtp_idle(&context.read().unwrap());
                        }
                    }
                })
            };

            {
                let guard = cx.lock();
                let mut ctx = this.borrow_mut(&guard);
                ctx.handles = Some(Handles::new(imap_handle, mvbox_handle, sentbox_handle, smtp_handle));
            }

            Ok(cx.undefined().upcast())
        }

        method disconnect(mut cx) {
            let mut this = cx.this();
            {
                let guard = cx.lock();
                let mut ctx = this.borrow_mut(&guard);

                if ctx.running.compare_and_swap(true, false, Ordering::Relaxed) {
                    {
                        let context = &ctx.context.read().unwrap();
                        // Interrupt
                        job::interrupt_imap_idle(context);
                        job::interrupt_mvbox_idle(context);
                        job::interrupt_sentbox_idle(context);
                        job::interrupt_smtp_idle(context);
                    }

                    if let Some(handles) = ctx.handles.take() {
                        handles.join().unwrap();
                    }

                    CALLBACK.write().unwrap().take();
                }
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
