use crate::io::clock::{MonotonicInstant, WallClockInstant};
use crate::io::{Buffer, Clock, File, FileId, FileSyncType, IO};
use crate::{Completion, MemoryIO, OpenFlags, Result};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
pub(super) enum Held {
    Read(Arc<dyn File>, u64, Completion),
    Sync(Arc<dyn File>, Completion, FileSyncType),
}
#[derive(Default)]
pub(super) struct Control {
    pub(super) disabled: AtomicBool,
    pub(super) hold_read: AtomicBool,
    pub(super) hold_sync: AtomicBool,
    pub(super) calls: AtomicUsize,
    pub(super) pending: Mutex<Option<Held>>,
}
impl Control {
    fn enter(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.disabled.load(Ordering::SeqCst),
            "new MAIN IO after exclusion"
        );
    }
}
pub(super) struct MainIo {
    pub(super) memory: MemoryIO,
    pub(super) control: Arc<Control>,
}
impl Clock for MainIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.memory.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.memory.current_time_wall_clock()
    }
}
impl IO for MainIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        self.control.enter();
        Ok(Arc::new(MainFile {
            file: self.memory.open_file(path, flags, direct)?,
            control: self.control.clone(),
        }))
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.control.enter();
        self.memory.remove_file(path)
    }
    fn file_id(&self, path: &str) -> Result<FileId> {
        self.control.enter();
        self.memory.file_id(path)
    }
    fn step(&self) -> Result<()> {
        self.control.enter();
        self.memory.step()
    }
    fn cancel(&self, completions: &[Completion]) -> Result<()> {
        self.control.enter();
        self.memory.cancel(completions)
    }
    fn drain_completions(&self, completions: &[Completion]) -> Result<()> {
        self.control.enter();
        self.memory.drain_completions(completions)
    }
    fn wait_for_completion(&self, completion: Completion) -> Result<()> {
        self.control.enter();
        self.memory.wait_for_completion(completion)
    }
}
struct MainFile {
    file: Arc<dyn File>,
    control: Arc<Control>,
}
impl File for MainFile {
    fn lock_file(&self, exclusive: bool) -> Result<()> {
        self.control.enter();
        self.file.lock_file(exclusive)
    }
    fn unlock_file(&self) -> Result<()> {
        self.control.enter();
        self.file.unlock_file()
    }
    fn size(&self) -> Result<u64> {
        self.control.enter();
        self.file.size()
    }
    fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
        self.control.enter();
        if self.control.hold_read.swap(false, Ordering::SeqCst) {
            *self.control.pending.lock().unwrap() =
                Some(Held::Read(self.file.clone(), pos, completion.clone()));
            return Ok(completion);
        }
        self.file.pread(pos, completion)
    }
    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, completion: Completion) -> Result<Completion> {
        self.control.enter();
        self.file.pwrite(pos, buffer, completion)
    }
    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        completion: Completion,
    ) -> Result<Completion> {
        self.control.enter();
        self.file.pwritev(pos, buffers, completion)
    }
    fn sync(&self, completion: Completion, mode: FileSyncType) -> Result<Completion> {
        self.control.enter();
        if self.control.hold_sync.swap(false, Ordering::SeqCst) {
            *self.control.pending.lock().unwrap() =
                Some(Held::Sync(self.file.clone(), completion.clone(), mode));
            return Ok(completion);
        }
        self.file.sync(completion, mode)
    }
    fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
        self.control.enter();
        self.file.truncate(len, completion)
    }
    fn checkpoint_wal_position(&self, salts: [u32; 2], frames: u64) -> Result<()> {
        self.control.enter();
        self.file.checkpoint_wal_position(salts, frames)
    }
}
