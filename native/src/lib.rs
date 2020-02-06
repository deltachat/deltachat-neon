#[macro_use]
extern crate neon;

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use neon::context::Context as NeonContext;
use neon::prelude::*;

use deltachat::{
    config, configure, contact,
    context::{self, Context},
    job, Event,
};

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
            let cb: Box<deltachat::context::ContextCallback> = if let Some(f) = cx.argument_opt(1) {
                let f = f.downcast::<JsFunction>().or_throw(&mut cx)?;
                let cb = EventHandler::new(&cx, this, f);

                Box::new(move |_ctx, event| {
                    // TODO: handle events
                    cb.schedule_with(move |cx, this, callback| {
                        let event_name = match event {
                            Event::Info(_) => "Info".into(),
                            Event::Warning(_) => "Warning".into(),
                            Event::Error(_) => "Error".into(),
                            Event::SmtpConnected(_) => "Smtp Connected".into(),
                            Event::ImapConnected(_) => "Imap Connected".into(),
                            Event::ErrorNetwork(_) => "Error Network".into(),
                            _ => format!("{:?}", event),
                        };

                        let text = match event {
                            Event::Info(ref txt)
                                | Event::Warning(ref txt)
                                | Event::Error(ref txt)
                                | Event::SmtpConnected(ref txt)
                                | Event::ImapConnected(ref txt)
                                | Event::ErrorNetwork(ref txt) => cx.string(txt).upcast(),
                            _ => cx.undefined().upcast(),
                        };
                        let args: Vec<Handle<JsValue>> = vec![cx.string(event_name).upcast(), text];
                        let _result = callback.call(cx, this, args);
                    });
                })
            } else {
                Box::new(|_, _| {})
            };


            let dbfile = cx.argument::<JsString>(0)?.value();
            let dir = std::path::Path::new(&dbfile);

            std::fs::create_dir_all(&dir)
                .map_err(|err| format!("Failed to create directory {:?}", err)).unwrap();

            let ctx = context::Context::new(cb, "desktop".into(), dir.join("db.sqlite").to_path_buf())
                .expect("failed to create context");

            Ok(ContextWrapper {
                context: Arc::new(RwLock::new(ctx)),
                running: Arc::new(AtomicBool::new(false)),
                handles: None,
            })
        }


        method configure(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            configure::configure(&context.read().unwrap());

            Ok(cx.undefined().upcast())
        }

        method getInfo(mut cx) {
            let this = cx.this();

            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };

            let res = context.read().unwrap().get_info();

            let info = JsObject::new(&mut cx);

            for (key, value) in res.into_iter() {
                let value = cx.string(value);
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
                            job::perform_inbox_jobs(ctx);
                            job::perform_inbox_fetch(ctx);
                        }
                        while running.load(Ordering::Relaxed) {
                            job::perform_inbox_idle(&context.read().unwrap());
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
                        job::interrupt_inbox_idle(context);
                        job::interrupt_mvbox_idle(context);
                        job::interrupt_sentbox_idle(context);
                        job::interrupt_smtp_idle(context);
                    }

                    if let Some(handles) = ctx.handles.take() {
                        handles.join().unwrap();
                    }
                }
            }
            Ok(cx.undefined().upcast())
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
            let ctx = context.read().unwrap();
            let blobdir = ctx.get_blobdir().to_str().unwrap_or_default();

            Ok(cx.string(blobdir).upcast())
        }

        method createContact(mut cx) {
            let this = cx.this();
            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };
            let ctx = context.read().unwrap();

            let name = cx.argument::<JsString>(0)?.value();
            let addr = cx.argument::<JsString>(1)?.value();
            let contact = contact::Contact::create(&ctx, name, addr).unwrap();

            Ok(cx.number(contact).upcast())
        }

        method lookupContactIdByAddr(mut cx) {
            let this = cx.this();
            let context = {
                let guard = cx.lock();
                let ctx = this.borrow(&guard);
                ctx.context.clone()
            };
            let ctx = context.read().unwrap();

            let addr = cx.argument::<JsString>(0)?.value();
            let contact = contact::Contact::lookup_id_by_addr(&ctx, addr);

            Ok(cx.number(contact).upcast())
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
    m.export_class::<JsContext>("Contact")?;
    m.export_function("maybeValidAddr", maybe_valid_addr)?;

    Ok(())
});
