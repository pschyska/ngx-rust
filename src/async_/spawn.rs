extern crate std;

use core::ffi::c_int;
use core::sync::atomic::{AtomicI64, Ordering};
use core::{mem, ptr};
use std::sync::OnceLock;

use core::future::Future;

pub use async_task::Task;
use async_task::{Runnable, ScheduleInfo, WithInfo};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nginx_sys::{
    kill, ngx_del_timer, ngx_delete_posted_event, ngx_event_t, ngx_post_event, ngx_posted_events,
    ngx_thread_tid, SIGIO,
};

use crate::log::ngx_cycle_log;
use crate::ngx_log_debug;
use crate::sync::RwLock;

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

        let tid = unsafe { ngx_thread_tid() };
        std::println!("!!! run handler tid={}", tid);
        r.run();
        cnt += 1;
    }
    ngx_log_debug!(
        unsafe { (*ev).log },
        "async: notify_handler processed {cnt} items"
    );
}

fn notify() -> c_int {
    ngx_log_debug!(ngx_cycle_log().as_ptr(), "async: ngx_notify");
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
    event: RwLock<ngx_event_t>,
}

// Safety: mutable access to event is guarded via RwLock
unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

impl Scheduler {
    fn new() -> Self {
        let (tx, rx) = unbounded();
        let mut event: ngx_event_t = unsafe { mem::zeroed() };
        event.handler = Some(async_handler);
        event.log = ngx_cycle_log().as_ptr();
        let event = RwLock::new(event);

        Scheduler { tx, rx, event }
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
            let tid = unsafe { ngx_thread_tid() };
            std::println!("!!! run eager tid={}", tid);
            runnable.run();
        } else {
            self.tx.send(runnable).expect("send");
            {
                let mut event = self.event.write();
                event.log = ngx_cycle_log().as_ptr();

                unsafe {
                    ngx_post_event(&mut *event, ptr::addr_of_mut!(ngx_posted_events));
                }
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

impl Drop for Scheduler {
    fn drop(&mut self) {
        let mut event = self.event.write();
        if event.posted() != 0 {
            unsafe { ngx_delete_posted_event(&mut *event) };
        }

        if event.timer_set() != 0 {
            unsafe { ngx_del_timer(&mut *event) };
        }
    }
}

static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();

fn scheduler() -> &'static Scheduler {
    SCHEDULER.get_or_init(Scheduler::new)
}

fn schedule(runnable: Runnable, info: ScheduleInfo) {
    let scheduler = scheduler();

    let tid = unsafe { ngx_thread_tid() };
    std::println!("!!! schedule tid={}", tid);
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
