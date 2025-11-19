extern crate std;

use core::ffi::c_int;
use core::sync::atomic::{AtomicI64, Ordering};
use core::{mem, ptr};
use std::sync::OnceLock;

use core::future::Future;

use alloc::boxed::Box;
pub use async_task::Task;
use async_task::{Runnable, ScheduleInfo, WithInfo};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nginx_sys::{kill, ngx_event_t, ngx_post_event, ngx_posted_next_events, ngx_thread_tid, SIGIO};

use crate::log::ngx_cycle_log;
use crate::ngx_log_debug;

static MAIN_TID: AtomicI64 = AtomicI64::new(-1);

#[inline]
fn on_event_thread() -> bool {
    let main_tid = MAIN_TID.load(Ordering::Relaxed);
    let tid: i64 = unsafe { ngx_thread_tid().into() };
    main_tid == tid
}

extern "C" fn async_handler(ev: *mut ngx_event_t) {
    // initialize MAIN_TID on first execution
    let tid = unsafe { ngx_thread_tid().into() };
    let _ = MAIN_TID.compare_exchange(-1, tid, Ordering::Relaxed, Ordering::Relaxed);
    let scheduler = scheduler();
    let mut cnt = 0;
    while let Ok(r) = scheduler.rx.try_recv() {
        r.run();
        cnt += 1;
    }
    ngx_log_debug!(
        unsafe { (*ev).log },
        "async: processed {cnt} items"
    );

    unsafe {
        drop(Box::from_raw(ev));
    }
}

fn notify() -> c_int {
    ngx_log_debug!(ngx_cycle_log().as_ptr(), "async: notify via SIGIO");
    unsafe {
        kill(
            std::process::id().try_into().unwrap(),
            SIGIO.try_into().unwrap(),
        )
    }
}

struct Scheduler {
    rx: Receiver<Runnable>,
    tx: Sender<Runnable>,
}

impl Scheduler {
    fn new() -> Self {
        let (tx, rx) = unbounded();
        Scheduler { tx, rx }
    }

    fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
        let oet = on_event_thread();
        // If we are on the event loop thread it's safe to simply run the Runnable, otherwise we
        // enqueue the Runnable, post our event, and SIGIO to interrupt epoll. The event handler
        // then runs the Runnable on the event loop thread.
        //
        // If woken_while_running, it indicates that a task has yielded itself to the Scheduler.
        // Force round-trip via queue to limit reentrancy (skipping SIGIO).
        if oet && !info.woken_while_running {
            runnable.run();
        } else {
            self.tx.send(runnable).expect("send");
            unsafe {
                let event: *mut ngx_event_t = Box::into_raw(Box::new(mem::zeroed()));
                (*event).handler = Some(async_handler);
                (*event).log = ngx_cycle_log().as_ptr();
                ngx_post_event(event, ptr::addr_of_mut!(ngx_posted_next_events));
            }

            if !oet {
                let rc = notify();
                if rc != 0 {
                    panic!("kill: {rc}")
                }
            }
        }
    }
}

static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();

fn scheduler() -> &'static Scheduler {
    SCHEDULER.get_or_init(Scheduler::new)
}

fn schedule(runnable: Runnable, info: ScheduleInfo) {
    let scheduler = scheduler();
    scheduler.schedule(runnable, info);
}

/// Creates a new task running on the NGINX event loop.
pub fn spawn<F, T>(future: F) -> Task<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    ngx_log_debug!(ngx_cycle_log().as_ptr(), "async: spawning new task");
    let (runnable, task) = unsafe { async_task::spawn_unchecked(future, WithInfo(schedule)) };
    runnable.schedule();
    task
}
