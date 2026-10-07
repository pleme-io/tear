#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::matrix::Band;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskReading {
    pub priority: i32,
    pub context_switches: u64,
    pub cpu_ns: u64,
    pub resident_bytes: u64,
    pub threads: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadReading {
    pub name: String,
    pub current: i32,
    pub base: i32,
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::mem::{MaybeUninit, size_of};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::{TaskReading, ThreadReading};
    use crate::matrix::Band;

    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    const QOS_CLASS_DEFAULT: u32 = 0x15;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    const PROC_PIDLISTTHREADS: i32 = 6;

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, relative: i32) -> i32;
    }

    pub fn apply_thread_band(band: Band) -> io::Result<()> {
        let class = match band {
            Band::Interactive => QOS_CLASS_USER_INTERACTIVE,
            Band::Default => QOS_CLASS_DEFAULT,
            Band::Background => QOS_CLASS_BACKGROUND,
        };
        let rc = unsafe { pthread_set_qos_class_self_np(class, 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    }

    pub fn set_process_background(on: bool) -> io::Result<()> {
        set_background_of(0, on)
    }

    pub fn set_background_of(pid: i32, on: bool) -> io::Result<()> {
        let value = if on { libc::PRIO_DARWIN_BG } else { 0 };
        let who =
            libc::id_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let rc = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, who, value) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn task(pid: i32) -> Option<TaskReading> {
        let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = size_of::<libc::proc_taskinfo>() as i32;
        let rc = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if rc != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(TaskReading {
            priority: info.pti_priority,
            context_switches: u64::try_from(info.pti_csw).unwrap_or(0),
            cpu_ns: info.pti_total_user + info.pti_total_system,
            resident_bytes: info.pti_resident_size,
            threads: u32::try_from(info.pti_threadnum).unwrap_or(0),
        })
    }

    pub fn started_at(pid: i32) -> Option<u64> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        let rc = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if rc != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn threads(pid: i32) -> Vec<ThreadReading> {
        let mut ids = vec![0u64; 1024];
        let bytes = i32::try_from(ids.len() * 8).unwrap_or(i32::MAX);
        let rc = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDLISTTHREADS,
                0,
                ids.as_mut_ptr().cast::<c_void>(),
                bytes,
            )
        };
        let Ok(n) = usize::try_from(rc) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for id in &ids[..(n / 8).min(ids.len())] {
            let mut ti = MaybeUninit::<libc::proc_threadinfo>::zeroed();
            let size = size_of::<libc::proc_threadinfo>() as i32;
            let r = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTHREADINFO,
                    *id,
                    ti.as_mut_ptr().cast::<c_void>(),
                    size,
                )
            };
            if r != size {
                continue;
            }
            let ti = unsafe { ti.assume_init() };
            let name: Vec<u8> = ti
                .pth_name
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| c.to_ne_bytes()[0])
                .collect();
            out.push(ThreadReading {
                name: String::from_utf8_lossy(&name).into_owned(),
                current: ti.pth_curpri,
                base: ti.pth_priority,
            });
        }
        out
    }

    #[repr(C)]
    struct SourceContext {
        version: isize,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
        equal: Option<extern "C" fn(*const c_void, *const c_void) -> u8>,
        hash: Option<extern "C" fn(*const c_void) -> usize>,
        schedule: Option<extern "C" fn(*const c_void, *mut c_void, *const c_void)>,
        cancel: Option<extern "C" fn(*const c_void, *mut c_void, *const c_void)>,
        perform: Option<extern "C" fn(*const c_void)>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopSourceCreate(
            alloc: *const c_void,
            order: isize,
            ctx: *mut SourceContext,
        ) -> *mut c_void;
        fn CFRunLoopAddSource(rl: *mut c_void, src: *mut c_void, mode: *const c_void);
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: *mut c_void);
        fn CFRunLoopSourceSignal(src: *mut c_void);
        fn CFRunLoopWakeUp(rl: *mut c_void);
        fn CFRelease(cf: *const c_void);
    }

    struct Loop {
        origin: Instant,
        stamp: AtomicU64,
        pending: AtomicBool,
        armed: AtomicBool,
        lat: std::sync::Mutex<Vec<u64>>,
        total: usize,
        count: AtomicUsize,
    }

    extern "C" fn perform(info: *const c_void) {
        let st = unsafe { &*info.cast::<Loop>() };
        let now = u64::try_from(st.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if !st.pending.swap(false, Ordering::SeqCst) {
            return;
        }
        let sent = st.stamp.load(Ordering::SeqCst);
        if let Ok(mut v) = st.lat.lock() {
            v.push(now.saturating_sub(sent));
        }
        let c = st.count.fetch_add(1, Ordering::SeqCst) + 1;
        if c >= st.total {
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
        } else {
            st.armed.store(true, Ordering::SeqCst);
        }
    }

    pub fn run_loop_wakes(
        total: usize,
        gap: impl Fn(usize) -> Duration,
        band: Band,
    ) -> io::Result<Vec<u64>> {
        let st = Arc::new(Loop {
            origin: Instant::now(),
            stamp: AtomicU64::new(0),
            pending: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            lat: std::sync::Mutex::new(Vec::with_capacity(total)),
            total,
            count: AtomicUsize::new(0),
        });
        let rl_p = Arc::new(AtomicPtr::<c_void>::new(std::ptr::null_mut()));
        let src_p = Arc::new(AtomicPtr::<c_void>::new(std::ptr::null_mut()));
        let (st2, rl2, src2) = (Arc::clone(&st), Arc::clone(&rl_p), Arc::clone(&src_p));
        let waiter = std::thread::Builder::new()
            .name("tearbench-runloop".into())
            .spawn(move || -> io::Result<()> {
                apply_thread_band(band)?;
                let mut ctx = SourceContext {
                    version: 0,
                    info: Arc::as_ptr(&st2).cast_mut().cast::<c_void>(),
                    retain: None,
                    release: None,
                    copy_description: None,
                    equal: None,
                    hash: None,
                    schedule: None,
                    cancel: None,
                    perform: Some(perform),
                };
                unsafe {
                    let rl = CFRunLoopGetCurrent();
                    let src = CFRunLoopSourceCreate(std::ptr::null(), 0, &raw mut ctx);
                    if src.is_null() {
                        return Err(io::Error::other("CFRunLoopSourceCreate returned null"));
                    }
                    CFRunLoopAddSource(rl, src, kCFRunLoopDefaultMode);
                    rl2.store(rl, Ordering::SeqCst);
                    src2.store(src, Ordering::SeqCst);
                    st2.armed.store(true, Ordering::SeqCst);
                    CFRunLoopRun();
                    CFRelease(src.cast_const());
                }
                Ok(())
            })?;
        let deadline = Instant::now() + Duration::from_secs(60);
        for i in 0..total {
            while !st.armed.swap(false, Ordering::SeqCst) {
                if Instant::now() > deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "run loop never re-armed",
                    ));
                }
                std::hint::spin_loop();
            }
            std::thread::sleep(gap(i));
            let rl = rl_p.load(Ordering::SeqCst);
            let src = src_p.load(Ordering::SeqCst);
            st.stamp.store(
                u64::try_from(st.origin.elapsed().as_nanos()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
            st.pending.store(true, Ordering::SeqCst);
            unsafe {
                CFRunLoopSourceSignal(src);
                CFRunLoopWakeUp(rl);
            }
        }
        waiter
            .join()
            .map_err(|_| io::Error::other("run-loop waiter panicked"))??;
        let lat = st
            .lat
            .lock()
            .map_err(|_| io::Error::other("run-loop latency lock poisoned"))?
            .clone();
        Ok(lat)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::time::Duration;

    use super::{TaskReading, ThreadReading};
    use crate::matrix::Band;

    pub fn apply_thread_band(band: Band) -> io::Result<()> {
        match band {
            Band::Interactive | Band::Default => Ok(()),
            Band::Background => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the background band is a macOS world fact; Linux has no equivalent here",
            )),
        }
    }

    pub fn set_process_background(on: bool) -> io::Result<()> {
        if on {
            apply_thread_band(Band::Background)
        } else {
            Ok(())
        }
    }

    pub fn set_background_of(pid: i32, on: bool) -> io::Result<()> {
        if pid == 0 {
            set_process_background(on)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the background band is a macOS world fact; Linux has no equivalent here",
            ))
        }
    }

    fn stat_fields(pid: i32) -> Option<Vec<String>> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let close = raw.rfind(')')?;
        Some(
            raw[close + 1..]
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        )
    }

    fn status_value(pid: i32, key: &str) -> Option<u64> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        raw.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
    }

    pub fn task(pid: i32) -> Option<TaskReading> {
        let f = stat_fields(pid)?;
        let ticks = 100u64;
        let utime: u64 = f.get(11)?.parse().ok()?;
        let stime: u64 = f.get(12)?.parse().ok()?;
        let nice: i32 = f.get(16)?.parse().ok()?;
        let threads: u32 = f.get(17)?.parse().ok()?;
        let rss_pages: u64 = f.get(21)?.parse().ok()?;
        let vol = status_value(pid, "voluntary_ctxt_switches:").unwrap_or(0);
        let invol = status_value(pid, "nonvoluntary_ctxt_switches:").unwrap_or(0);
        Some(TaskReading {
            priority: 20 - nice,
            context_switches: vol + invol,
            cpu_ns: (utime + stime) * (1_000_000_000 / ticks),
            resident_bytes: rss_pages * 4096,
            threads,
        })
    }

    pub fn started_at(pid: i32) -> Option<u64> {
        stat_fields(pid)?.get(19)?.parse().ok()
    }

    pub fn threads(pid: i32) -> Vec<ThreadReading> {
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| {
                let tid = e.file_name().to_string_lossy().into_owned();
                let name = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm")).ok()?;
                let raw = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
                let close = raw.rfind(')')?;
                let f: Vec<&str> = raw[close + 1..].split_whitespace().collect();
                let nice: i32 = f.get(16)?.parse().ok()?;
                Some(ThreadReading {
                    name: name.trim().to_string(),
                    current: 20 - nice,
                    base: 20 - nice,
                })
            })
            .collect()
    }

    pub fn run_loop_wakes(
        _total: usize,
        _gap: impl Fn(usize) -> Duration,
        _band: Band,
    ) -> io::Result<Vec<u64>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a CFRunLoop wake exists only on macOS",
        ))
    }
}

pub use imp::{run_loop_wakes, threads};

pub fn apply_thread_band(band: Band) -> io::Result<()> {
    imp::apply_thread_band(band)
}

pub fn apply_process_band(band: Band) -> io::Result<()> {
    imp::set_process_background(matches!(band, Band::Background))?;
    match band {
        Band::Background => Ok(()),
        Band::Interactive | Band::Default => imp::apply_thread_band(band),
    }
}

pub fn set_background_of(pid: i32, on: bool) -> io::Result<()> {
    imp::set_background_of(pid, on)
}

#[must_use]
pub fn task(pid: i32) -> Option<TaskReading> {
    imp::task(pid)
}

#[must_use]
pub fn in_background(pid: i32) -> Option<bool> {
    imp::task(pid).map(|t| t.priority <= 4)
}

#[must_use]
pub fn started_at(pid: i32) -> Option<u64> {
    imp::started_at(pid)
}

struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ONE_COUNT_AT_A_TIME: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

pub fn allocations_during(f: &mut dyn FnMut()) -> u64 {
    let _one = ONE_COUNT_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    f();
    COUNTING.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst) - before
}
