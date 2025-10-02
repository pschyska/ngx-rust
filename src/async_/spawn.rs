extern crate std;

use core::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use core::future::Future;

pub use async_task::Task;
use async_task::{Runnable, ScheduleInfo, WithInfo};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nginx_sys::{ngx_event_actions, ngx_event_t, ngx_thread_tid};

use crate::log::ngx_cycle_log;
use crate::ngx_log_debug;

static MAIN_TID: AtomicI64 = AtomicI64::new(-1);

#[inline]
fn on_event_thread() -> bool {
    let main_tid = MAIN_TID.load(Ordering::Relaxed);
    let tid: i64 = unsafe { ngx_thread_tid().into() };
    main_tid == tid
}

extern "C" fn notify_handler(_ev: *mut ngx_event_t) {
    let tid = unsafe { ngx_thread_tid().into() };

    // initialize MAIN_TID on first execution
    let _ = MAIN_TID.compare_exchange(-1, tid, Ordering::Relaxed, Ordering::Relaxed);

    let scheduler = scheduler();
    let mut cnt = 0;
    while let Ok(r) = scheduler.rx.try_recv() {
        r.run();
        cnt += 1;
    }
    ngx_log_debug!(
        ngx_cycle_log().as_ptr(),
        "async: notify_handler processed {cnt} items"
    );
}

fn notify() {
    ngx_log_debug!(ngx_cycle_log().as_ptr(), "async: ngx_notify");
    unsafe {
        ngx_event_actions.notify.expect("ngx_notify")(Some(notify_handler));
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
        // If we are on the event loop thread it's safe to simply run the Runnable, otherwise we
        // enqueue the Runnable and call notify to move it and interrupt epoll
        //
        // If woken_while_running, it indicates that a task has yielded itself to the Scheduler.
        // Force round-trip via queue and notify to limit reentrancy.
        //
        if on_event_thread() && !info.woken_while_running {
            runnable.run();
        } else {
            self.tx.send(runnable).expect("send");
            notify();
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
