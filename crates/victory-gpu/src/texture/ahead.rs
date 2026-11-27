//! First uploads of textures picked by index on a copy queue of their own
//! (`VICTORY_UPLOAD_AHEAD`): read, detiled and copied by threads of ours as
//! soon as the texture pass has made them, and the slots that wait for them
//! shown when the copy has run. Recorded into the pass's list, as every other
//! upload is, the copy ran with the frame that needs it, so a slot that shows
//! its texture only after its upload ([`crate::descriptor::host::show_after`])
//! was black or grey in that frame, and in the next.
//!
//! Only a texture that no list has used is taken: made in the last second,
//! in the common state, and never asked for an upload before
//! ([`crate::resource::HostTexture::ahead`]). It stays in the common state
//! (a copy queue's copies leave it there), from which the card's shaders read
//! it as any texture.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

use super::Staging;
use crate::object::This;
use crate::resource::HostTexture;

/// [`HostTexture::ahead`]: no upload was asked for yet; one is on its way
/// here; it has run; one was recorded into a list.
pub(crate) const NONE: u8 = 0;
pub(super) const PENDING: u8 = 1;
const DONE: u8 = 2;
pub(crate) const LISTED: u8 = 3;

/// Bytes and textures of one copy-list submission.
const BATCH: (u64, usize) = (32 << 20, 128);

/// Whether it is on (`VICTORY_UPLOAD_AHEAD`; off).
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_UPLOAD_AHEAD") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPLOAD_AHEAD").is_some_and(|v| v != "0"))
}

/// An upload on its way.
pub(crate) struct Job {
    staging: Arc<Staging>,
    copy: Arc<HostTexture>,
    asked: std::time::Instant,
    retries: AtomicU64,
    /// What to do when it has run; `None`: it has.
    then: Mutex<Option<Vec<Box<dyn FnOnce() + Send>>>>,
}

impl Job {
    /// Backs off repeatedly unreadable snapshots without abandoning them.
    fn retry_delay(&self) -> std::time::Duration {
        let attempt = self.retries.fetch_add(1, Relaxed).min(4);
        std::time::Duration::from_millis(8 << attempt)
    }

    /// Calls `f` when the copy has run: now if it has.
    pub(crate) fn then(&self, f: impl FnOnce() + Send + 'static) {
        let mut then = self.then.lock().unwrap_or_else(|e| e.into_inner());
        match then.as_mut() {
            Some(then) => then.push(Box::new(f)),
            None => {
                drop(then);
                f();
            }
        }
    }
}

/// What a texture asked for here gets.
pub(crate) enum Taken {
    /// A job of its own, to be [`send`] once what follows it is added.
    New(Arc<Job>),
    /// One already on its way, or that has run.
    Known(Arc<Job>),
}

struct WorkQueue<T> {
    fresh: VecDeque<T>,
    retries: BTreeMap<Instant, VecDeque<T>>,
}

impl<T> WorkQueue<T> {
    fn defer(&mut self, at: Instant, job: T) {
        self.retries.entry(at).or_default().push_back(job);
    }

    fn pop(&mut self, now: Instant) -> Option<T> {
        if let Some(mut due) = self.retries.first_entry() && *due.key() <= now {
            let jobs = due.get_mut();
            let job = jobs.pop_front();
            if jobs.is_empty() {
                due.remove();
            }
            return job;
        }
        self.fresh.pop_front()
    }
}

static QUEUE: (Mutex<WorkQueue<Arc<Job>>>, Condvar) = (
    Mutex::new(WorkQueue { fresh: VecDeque::new(), retries: BTreeMap::new() }),
    Condvar::new(),
);
/// A submitted copy without a provable completion cannot be retried safely.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Those on their way, by host copy ([`HostTexture::serial`]).
static ON_THEIR_WAY: Mutex<Vec<(u64, Arc<Job>)>> = Mutex::new(Vec::new());
static LANDED: Condvar = Condvar::new();
static BYTES: AtomicU64 = AtomicU64::new(0);
/// Since the start: uploads, bytes, microseconds from the asking until the
/// copy had run, the longest; askings turned down; waits of lists.
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Bound upload staging (system RAM) in flight by both host RAM headroom and
/// the adapter's local-memory budget. Refresh OS queries at most once a
/// second, rather than once per texture.
fn most() -> u64 {
    const MIB: u64 = 1 << 20;
    static MOST: AtomicU64 = AtomicU64::new(96 * MIB);
    static CHECKED: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    if now.saturating_sub(CHECKED.load(Relaxed)) >= 1000 {
        const GIB: u64 = 1 << 30;
        let video_limit = match crate::host::host().and_then(|host| host.video_memory()).map(|[budget, ..]| budget) {
            Some(budget) if budget <= 6 * GIB => 96 * MIB,
            Some(budget) if budget <= 12 * GIB => 192 * MIB,
            Some(budget) if budget <= 20 * GIB => 256 * MIB,
            Some(_) => 384 * MIB,
            None => 96 * MIB,
        };
        let ram_limit = crate::host::available_physical_ram()
            .map(|available| (available / 64).clamp(32 * MIB, 384 * MIB))
            .unwrap_or(96 * MIB);
        MOST.store(video_limit.min(ram_limit), Relaxed);
        CHECKED.store(now, Relaxed);
    }
    MOST.load(Relaxed)
}

/// Reserve capacity before allocating a staging buffer; concurrent draw
/// threads must not each pass the same free-space check.
fn reserve(bytes: u64) -> bool {
    let limit = most();
    BYTES.fetch_update(Relaxed, Relaxed, |used| used.checked_add(bytes).filter(|next| *next <= limit)).is_ok()
}

/// Takes the first upload of `resource`'s levels from `first` on, `there` of
/// whose bytes are mapped (`u64::MAX`: not known). `None`: the list's.
pub(crate) fn take(resource: This, first: u32, there: u64) -> Option<Taken> {
    if on() && !FAILED.load(Relaxed) { claim(resource, first, there) } else { None }
}

fn claim(resource: This, first: u32, there: u64) -> Option<Taken> {
    workers()?;
    let turned_down = || {
        STATS[4].fetch_add(1, Relaxed);
        None
    };
    let copy = crate::resource::host_copy(resource)?;
    if copy.ahead.load(Relaxed) == PENDING {
        let way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        return way.iter().find(|(serial, _)| *serial == copy.serial).map(|(_, job)| Taken::Known(job.clone()));
    }
    if BYTES.load(Relaxed) >= most()
        || copy.flags() & 0x7 != 0
        || copy.state.load(Relaxed) != 0
        || copy.grown.load(Relaxed)
        || copy.tiled.is_some()
        || copy.holds.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        || !crate::pool::fresh(&copy.texture)
        || super::host_wrote(resource as usize)
        || crate::resource::alias_of(resource).is_some()
    {
        return turned_down();
    }
    let Some((_, estimated)) = super::title_span(resource, first) else {
        return turned_down();
    };
    let reserved = estimated;
    if !reserve(reserved) {
        return turned_down();
    }
    if copy.ahead.compare_exchange(NONE, PENDING, Relaxed, Relaxed).is_err() {
        BYTES.fetch_sub(reserved, Relaxed);
        return turned_down();
    }
    super::PICKING.set(Some(there));
    let staging = super::staging(resource, &copy, first);
    super::PICKING.set(None);
    let Some(staging) = staging.filter(|s| {
        !s.on_demand && s.first == s.level0 && !s.uploaded.load(Relaxed) && s.once.load(Relaxed)
    }) else {
        BYTES.fetch_sub(reserved, Relaxed);
        copy.ahead.store(LISTED, Relaxed);
        return turned_down();
    };
    // The placed-footprint buffer includes row padding: charge its actual
    // system-memory allocation, not only the packed title bytes.
    let actual = staging.bytes;
    if actual > reserved && !reserve(actual - reserved) {
        BYTES.fetch_sub(reserved, Relaxed);
        copy.ahead.store(LISTED, Relaxed);
        return turned_down();
    }
    if reserved > actual {
        BYTES.fetch_sub(reserved - actual, Relaxed);
    }
    if super::no_wait() && super::fill_unlocked() {
        staging.expected.store(if there == u64::MAX { super::mapped_bytes(staging.memory, staging.read) } else { there }, Relaxed);
    }
    let job = Arc::new(Job {
        staging,
        copy,
        asked: std::time::Instant::now(),
        retries: AtomicU64::new(0),
        then: Mutex::new(Some(Vec::new())),
    });
    ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner()).push((job.copy.serial, job.clone()));
    Some(Taken::New(job))
}

/// Sends a job of [`take`] on its way.
pub(crate) fn send(job: &Arc<Job>) {
    QUEUE.0.lock().unwrap_or_else(|e| e.into_inner()).fresh.push_back(job.clone());
    QUEUE.1.notify_one();
}

/// A list is about to record an upload into `copy`: one on its way here runs
/// first, and no other is taken after it.
pub(crate) fn listed(copy: &HostTexture) {
    if copy.ahead.compare_exchange(NONE, LISTED, Relaxed, Relaxed) != Err(PENDING) {
        return;
    }
    STATS[5].fetch_add(1, Relaxed);
    let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
    let started = std::time::Instant::now();
    while way.iter().any(|(serial, _)| *serial == copy.serial) && started.elapsed().as_millis() < 1000 {
        way = LANDED.wait_timeout(way, std::time::Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// The copy queue and its threads, made on first use.
fn workers() -> Option<&'static ID3D12CommandQueue> {
    /// Free-threaded.
    struct Queue(ID3D12CommandQueue);
    // SAFETY: a command queue is free-threaded.
    unsafe impl Send for Queue {}
    // SAFETY: as above.
    unsafe impl Sync for Queue {}
    static WORKERS: OnceLock<Option<Queue>> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            let host = crate::host::host()?;
            // SAFETY: plain object creation.
            let queue: ID3D12CommandQueue = unsafe {
                host.device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    })
                    .ok()?
            };
            // Detiling and snapshot reads run alongside the title's recording
            // threads; scale conservatively on CPUs with fewer logical cores.
            let count = std::thread::available_parallelism().map_or(2, |n| (n.get() / 5).clamp(2, 4));
            for n in 0..count {
                let worker = Worker::new(&host.device, queue.clone()).ok()?;
                std::thread::Builder::new().name(format!("uploads ahead {n}")).spawn(move || worker.work()).ok()?;
            }
            Some(Queue(queue))
        })
        .as_ref()
        .map(|q| &q.0)
}

struct Worker {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
}

// SAFETY: a worker's objects are its thread's own; the queue is free-threaded.
unsafe impl Send for Worker {}

enum Copied {
    Complete,
    Rejected(windows::core::Error),
    Uncertain(windows::core::Error),
}

impl Worker {
    fn new(device: &ID3D12Device, queue: ID3D12CommandQueue) -> windows::core::Result<Worker> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Worker { queue, allocator, list, fence, event })
        }
    }

    fn work(self) {
        let mut value = 0;
        loop {
            let mut jobs = Vec::new();
            {
                let mut queue = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if FAILED.load(Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    let mut bytes = 0;
                    while bytes < BATCH.0 && jobs.len() < BATCH.1
                        && let Some(job) = queue.pop(now)
                    {
                        bytes += job.staging.bytes;
                        jobs.push(job);
                    }
                    if !jobs.is_empty() {
                        break;
                    }
                    queue = if let Some((&at, _)) = queue.retries.first_key_value() {
                        QUEUE.1.wait_timeout(queue, at.saturating_duration_since(now))
                            .unwrap_or_else(|e| e.into_inner()).0
                    } else {
                        QUEUE.1.wait(queue).unwrap_or_else(|e| e.into_inner())
                    };
                }
            }
            let mut ready = Vec::with_capacity(jobs.len());
            let mut pending = Vec::new();
            for job in jobs {
                job.staging.refresh();
                if job.staging.ready.load(Relaxed) {
                    ready.push(job);
                } else {
                    pending.push(job);
                }
            }
            if !pending.is_empty() {
                // A remap can make a nonblocking snapshot incomplete. Keep
                // ownership and retry; never publish the pooled buffer's old
                // bytes as this texture.
                defer(pending);
            }
            if ready.is_empty() {
                continue;
            }
            value += 1;
            match self.copy(&ready, value) {
                Copied::Complete => finish(ready),
                Copied::Rejected(error) => {
                    tracing::warn!(%error, "uploads ahead were rejected before submission");
                    if FAILED.load(Relaxed) {
                        return;
                    }
                    defer(ready);
                }
                Copied::Uncertain(error) => {
                    tracing::error!(%error, "upload completion is uncertain; stopping the copy workers and retaining GPU-visible resources");
                    {
                        // Publish failure under the queue lock so a worker
                        // cannot miss the wake-up while entering its wait.
                        let _queue = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
                        FAILED.store(true, Relaxed);
                    }
                    QUEUE.1.notify_all();
                    // The GPU may still reference this allocator, list and
                    // staging resources. Quarantine them until process exit.
                    std::mem::forget(self);
                    return;
                }
            }
        }
    }

    fn copy(&self, jobs: &[Arc<Job>], value: u64) -> Copied {
        // SAFETY: this worker's allocator and list, whose last submission has
        // run; the jobs hold what the list names until after the wait.
        unsafe {
            if let Err(error) = self.allocator.Reset() {
                return Copied::Rejected(error);
            }
            if let Err(error) = self.list.Reset(&self.allocator, None) {
                return Copied::Rejected(error);
            }
            for job in jobs {
                job.record(&self.list);
            }
            if let Err(error) = self.list.Close() {
                return Copied::Rejected(error);
            }
            let commands = match self.list.cast() {
                Ok(commands) => commands,
                Err(error) => return Copied::Rejected(error),
            };
            self.queue.ExecuteCommandLists(&[Some(commands)]);
            if let Err(error) = self.queue.Signal(&self.fence, value) {
                return Copied::Uncertain(error);
            }
            // Keep staging buffers and allocator/list alive through completion.
            if let Err(error) = self.fence.SetEventOnCompletion(value, self.event) {
                return Copied::Uncertain(error);
            }
            if WaitForSingleObject(self.event, INFINITE).0 != 0 && self.fence.GetCompletedValue() < value {
                return Copied::Uncertain(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
        }
        Copied::Complete
    }
}

impl Job {
    /// # Safety
    /// `list` is an open copy list, and the job lives until it has run.
    unsafe fn record(&self, list: &ID3D12GraphicsCommandList) {
        let (s, texture) = (&self.staging, &self.copy.texture);
        for (i, footprint) in s.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: s.first - s.level0 + i as u32 },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&s.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: s.base + footprint.Offset, ..*footprint },
                },
            };
            // SAFETY: the caller's.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
}

/// Preserve each job's backoff without holding up ready uploads.
fn defer(jobs: Vec<Arc<Job>>) {
    let now = Instant::now();
    let mut queue = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
    for job in jobs {
        queue.defer(now + job.retry_delay(), job);
    }
    drop(queue);
    QUEUE.1.notify_all();
}

/// The jobs' copies have run.
fn finish(jobs: Vec<Arc<Job>>) {
    for job in &jobs {
        job.staging.uploaded.store(true, Relaxed);
        job.staging.dirty.store(false, Relaxed);
        job.staging.note_held();
        job.copy.ahead.store(DONE, Relaxed);
        BYTES.fetch_sub(job.staging.bytes, Relaxed);
    }
    {
        let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        way.retain(|(serial, _)| !jobs.iter().any(|job| job.copy.serial == *serial));
        LANDED.notify_all();
    }
    for job in &jobs {
        let us = job.asked.elapsed().as_micros() as u64;
        let n = STATS[0].fetch_add(1, Relaxed) + 1;
        let bytes = STATS[1].fetch_add(job.staging.read as u64, Relaxed) + job.staging.read as u64;
        let all = STATS[2].fetch_add(us, Relaxed) + us;
        let most = STATS[3].fetch_max(us, Relaxed).max(us);
        if n == 1 || n.is_multiple_of(5000) {
            tracing::info!(
                "textures uploaded ahead of the lists (VICTORY_UPLOAD_AHEAD=1), since the start: {n}, {} MB, {:.1} ms from the asking until they were there, the longest {:.0}; {} turned down, {} waited for by lists",
                bytes >> 20,
                all as f64 / n as f64 / 1e3,
                most as f64 / 1e3,
                STATS[4].load(Relaxed),
                STATS[5].load(Relaxed)
            );
        }
        let then = job.then.lock().unwrap_or_else(|e| e.into_inner()).take();
        for f in then.into_iter().flatten() {
            f();
        }
    }
    crate::descriptor::host::flip_now();
}

/// Whether `resource`'s host copy was filled here: no list still brings what
/// it shows.
pub(crate) fn there(resource: This) -> bool {
    crate::resource::made_copy(resource).is_some_and(|copy| copy.ahead.load(Relaxed) == DONE)
}

/// The state of [`HostTexture::ahead`] of a new host copy.
pub(crate) fn fresh() -> AtomicU8 {
    AtomicU8::new(NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_uploads_run_while_retries_back_off() {
        use std::time::Duration;
        let mut queue = WorkQueue { fresh: VecDeque::new(), retries: BTreeMap::new() };
        let now = Instant::now();
        let early = now + Duration::from_millis(8);
        let late = now + Duration::from_millis(128);
        queue.defer(late, "slow retry");
        queue.defer(early, "first retry");
        queue.defer(early, "second retry");
        queue.fresh.extend(["ready", "later fresh"]);
        assert_eq!(queue.pop(now), Some("ready"));
        assert_eq!(queue.pop(early), Some("first retry"));
        assert_eq!(queue.pop(early), Some("second retry"));
        assert_eq!(queue.pop(early), Some("later fresh"));
        assert_eq!(queue.pop(early), None);
        assert_eq!(queue.pop(late), Some("slow retry"));
        assert_eq!(queue.pop(late), None);
    }

    /// The texels of a placed texture arrive by the copy queue, all levels of
    /// them, and what waits for them is called then; a second asking finds the
    /// first on its way, and a later one is the list's.
    #[test]
    fn a_placed_texture_is_filled_ahead_of_the_lists() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        let Some(host) = crate::host::host() else { return };
        // 256x256 BC7, nine levels.
        let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = crate::resource::placed_bytes(&desc).unwrap() as usize;
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }.cast::<u8>();
        // SAFETY: as above.
        unsafe { (0..whole).for_each(|i| at.add(i).write((i * 13 + i / 509) as u8)) };
        let texture = crate::resource::texture_at(at as u64, &desc).unwrap();
        crate::resource::want_level(texture, 0);
        let copy = crate::resource::host_copy(texture).unwrap();
        if !crate::pool::fresh(&copy.texture) {
            return;
        }
        assert!(!there(texture));
        let Some(Taken::New(job)) = claim(texture, 0, u64::MAX) else { panic!("not taken") };
        let (told, heard) = std::sync::mpsc::channel();
        let also = told.clone();
        let Some(Taken::Known(same)) = claim(texture, 0, u64::MAX) else { panic!("not known") };
        assert!(Arc::ptr_eq(&job, &same));
        job.then(move || told.send(1).unwrap());
        same.then(move || also.send(2).unwrap());
        send(&job);
        let wait = std::time::Duration::from_secs(20);
        assert_eq!((heard.recv_timeout(wait), heard.recv_timeout(wait)), (Ok(1), Ok(2)));
        assert!(there(texture) && claim(texture, 0, u64::MAX).is_none());
        // Level 0 as the card has it, and as the layout says.
        let layout = crate::resource::texture_memory(texture)
            .and_then(|(_, d)| crate::resource::texture_layout(&d).map(|t| t.planes[0].layout.clone()))
            .unwrap();
        let (pitch, rows) = (1024usize, 64usize);
        let mut expected = vec![0u8; pitch * rows];
        // SAFETY: the memory above.
        let tiled = unsafe { std::slice::from_raw_parts(at, whole) };
        layout.detile(tiled, 0, 0, &mut expected, pitch, pitch * rows).unwrap();
        let back = host.read_back_buffer((pitch * rows) as u64).unwrap();
        let list = host.new_list().unwrap();
        list.close();
        list.reset();
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(98),
                Width: 256,
                Height: 256,
                Depth: 1,
                RowPitch: pitch as u32,
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&back) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&copy.texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        // SAFETY: an open list, live resources; the mapping spans the buffer.
        unsafe {
            list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            list.close();
            host.execute(&[&list]);
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            let seen = std::slice::from_raw_parts(data.cast::<u8>(), pitch * rows);
            assert!(seen == expected.as_slice(), "level 0's blocks");
            back.Unmap(0, None);
        }
    }
}

