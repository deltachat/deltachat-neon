#[macro_use]
extern crate neon;

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use lazy_static::lazy_static;
use neon::context::Context as NeonContext;
use neon::prelude::*;

use deltachat::{
    config, configure,
    constants::Event,
    contact,
    context::{self, Context},
    job,
};

lazy_static! {
    static ref CALLBACK: RwLock<Option<EventHandler>> = RwLock::new(None);
}

struct OpenTask {
    context: Arc<RwLock<Context>>,
    dbfile: String,
    blobdir: Option<String>,
}

impl OpenTask {
    pub fn new(context: Arc<RwLock<Context>>, dbfile: String, blobdir: Option<String>) -> Self {
        OpenTask {
            context,
            dbfile,
            blobdir,
        }
    }
}

impl Task for OpenTask {
    type Output = ();
    type Error = String;
    type JsEvent = JsUndefined;

    fn perform(&self) -> Result<Self::Output, Self::Error> {
        let dir = std::path::Path::new(&self.dbfile);

        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("Failed to create directory {:?}", err))?;

        let dbfile = dir.join("db.sqlite");

        let opened = unsafe {
            let lock = self.context.read().unwrap();
            context::dc_open(
                &lock,
                dbfile.to_str().unwrap(),
                self.blobdir.as_ref().map(|s| s.as_str()),
            )
        };

        if !opened {
            Err("Failed to open database".to_string())
        } else {
            Ok(())
        }
    }

    fn complete(
        self,
        mut cx: TaskContext,
        result: Result<Self::Output, Self::Error>,
    ) -> JsResult<Self::JsEvent> {
        match result {
            Ok(_) => Ok(cx.undefined()),
            Err(err) => cx.throw_error(err),
        }
    }
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

            if let Some(f) = cx.argument_opt(0) {
                let cb = &mut *CALLBACK.write().unwrap();
                assert!(cb.is_none(), "Only one context supported at the moment");

                *cb = Some(EventHandler::new(this, f.downcast::<JsFunction>().or_throw(&mut cx)?));
            }

            Ok(ContextWrapper {
                context: Arc::new(RwLock::new(ctx)),
                running: Arc::new(AtomicBool::new(false)),
                handles: None,
            })
        }

        method open(mut cx) {
            let this = cx.this();

            let dbfile = cx.argument::<JsString>(0)?.value();
            let blobdir = None;

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let cb = OpenTask::new(context, dbfile, blobdir);
            cb.schedule(cx.argument::<JsFunction>(1)?);

            Ok(cx.undefined().upcast())
        }

        method configure(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            unsafe { configure::configure(&context.read().unwrap()) };

            Ok(cx.undefined().upcast())
        }

        method getInfo(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let res = unsafe {
                let c_str = context::dc_get_info(&context.read().unwrap());
                std::ffi::CStr::from_ptr(c_str).to_str().unwrap()
            };

            let info = JsObject::new(&mut cx);

            for line in res.lines() {
                let mut parts = line.split_terminator('=');
                let key = parts.next().unwrap_or_default();
                let value = cx.string(parts.next().unwrap_or_default());

                info.set(&mut cx, key, value)?;
            }


            Ok(info.upcast())
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

        method close(mut cx) {
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
                }

                CALLBACK.write().unwrap().take();
            }
            Ok(cx.undefined().upcast())
        }

        method isOpen(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let res = unsafe { context::dc_is_open(&context.read().unwrap()) };

            Ok(cx.boolean(res == 1).upcast())
        }

        method isConfigured(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let res = configure::dc_is_configured(&context.read().unwrap());

            Ok(cx.boolean(res).upcast())
        }

        method getConfig(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let key = config::Config::from_str(&cx.argument::<JsString>(0)?.value()).expect("invalid key");
            let res = context.read().unwrap().get_config(key).expect("Faile to get config");

            Ok(cx.string(res).upcast())
        }

        method getBlobdir(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let blobdir = unsafe {
                let ptr = context.read().unwrap().get_blobdir();
                assert!(!ptr.is_null());
                std::ffi::CStr::from_ptr(ptr).to_str().unwrap()
            };

            Ok(cx.string(blobdir).upcast())
        }
    }
}

fn maybe_valid_addr(mut cx: FunctionContext) -> JsResult<JsBoolean> {
    let addr = match cx.argument_opt(0) {
        Some(v) => match v.downcast::<JsString>() {
            Ok(v) => v.value(),
            Err(_) => return Ok(cx.boolean(false)),
        },
        None => return Ok(cx.boolean(false)),
    };

    let res = contact::may_be_valid_addr(&addr);

    Ok(cx.boolean(res))
}

// Export the class
register_module!(mut m, {
    m.export_class::<JsContext>("Context")?;
    m.export_function("maybeValidAddr", maybe_valid_addr)?;

    Ok(())
});
