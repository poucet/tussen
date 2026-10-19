//! Precise wakeups for the router thread.
//!
//! macOS coalesces timers for ordinary threads: measured on this Mac, a
//! 0.5–20 ms `sleep`/`park_timeout`/channel `recv_timeout` overshoots by
//! **3.0 ms on average and up to 10 ms** — far too coarse for a delay line
//! that should land within a millisecond. Marking the thread with the Mach
//! time-constraint policy (what audio threads use) brings the same
//! measurement to **0.02 ms mean, 0.08 ms max**. The request is modest — a
//! 1 ms period with 0.1 ms of computation, preemptible — and the router is
//! idle almost all the time; if it ever ran hot the kernel would demote it
//! rather than let it starve anything. Other platforms' timers are
//! precise enough as they are, so this is a no-op there.

/// Ask for precise timer wakeups on the calling thread. Returns whether
/// the request was granted (always `true` off macOS).
pub fn precise_wakeups() -> bool {
    imp::precise_wakeups()
}

#[cfg(target_os = "macos")]
mod imp {
    #[repr(C)]
    struct TimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }
    #[repr(C)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }
    const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    extern "C" {
        fn mach_thread_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        static mach_task_self_: u32;
        fn thread_policy_set(thread: u32, flavor: u32, policy: *const TimeConstraintPolicy, count: u32) -> i32;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
    }

    pub fn precise_wakeups() -> bool {
        // SAFETY: plain Mach calls on the current thread with a correctly
        // sized, initialised policy struct; the thread port is released.
        unsafe {
            let mut tb = TimebaseInfo { numer: 0, denom: 0 };
            if mach_timebase_info(&mut tb) != 0 || tb.numer == 0 {
                return false;
            }
            let abs = |ns: u64| (ns * tb.denom as u64 / tb.numer as u64) as u32;
            let policy = TimeConstraintPolicy {
                period: abs(1_000_000),
                computation: abs(100_000),
                constraint: abs(1_000_000),
                preemptible: 1,
            };
            let thread = mach_thread_self();
            let rc = thread_policy_set(thread, THREAD_TIME_CONSTRAINT_POLICY, &policy, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
            mach_port_deallocate(mach_task_self_, thread);
            rc == 0
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn precise_wakeups() -> bool {
        true
    }
}
