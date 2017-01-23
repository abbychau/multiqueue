use std::cell::Cell;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

use alloc;
use consume::Consume;
use countedindex::{CountedIndex, Transaction};
use maybe_acquire::{MAYBE_ACQUIRE, maybe_acquire_fence};

#[derive(Clone, Copy)]
enum ReaderState {
    Single,
    Multi,
}

pub struct Reader {
    pos_data: CountedIndex,
    state: Cell<ReaderState>,
    num_consumers: AtomicUsize,
}

/// This represents the reader attempt at loading a transaction
/// It behaves similarly to a Transaction but has logic for single/multi
/// readers
struct ReadAttempt<'a> {
    linked: Transaction<'a>,
    reader: &'a Reader,
    state: ReaderState,
}

/// This holds the set of readers currently active.
/// This struct is held out of line from the cursor so it's easy to atomically replace it
struct ReaderGroup {
    readers: Vec<*const Reader>,
}

#[repr(C)]
pub struct ReadCursor {
    readers: AtomicPtr<ReaderGroup>,
}

impl<'a> ReadAttempt<'a> {
    #[inline(always)]
    pub fn get(&self) -> (isize, usize) {
        self.linked.get()
    }

    #[inline(always)]
    pub fn commit_attempt(self, by: u32, ord: Ordering) -> Option<ReadAttempt<'a>> {
        match self.state {
            ReaderState::Single => {
                self.linked.commit_direct(by, ord);
                None
            }
            ReaderState::Multi => {
                if self.reader.num_consumers.load(Ordering::Relaxed) == 1 {
                    fence(Ordering::Acquire);
                    self.reader.state.set(ReaderState::Single);
                    self.linked.commit_direct(by, ord);
                    None
                } else {
                    match self.linked.commit(by, ord) {
                        Some(transaction) => {
                            Some(ReadAttempt {
                                linked: transaction,
                                reader: self.reader,
                                state: ReaderState::Multi,
                            })
                        }
                        None => None,
                    }
                }
            }
        }
    }
}

impl Reader {
    #[inline(always)]
    pub fn load_attempt(&self, ord: Ordering) -> ReadAttempt {
        ReadAttempt {
            linked: self.pos_data.load_transaction(ord),
            reader: self,
            state: self.state.get(),
        }
    }

    #[inline(always)]
    pub fn load_nread(&self, ord: Ordering) -> usize {
        self.pos_data.load_count(ord)
    }

    pub fn dup_consumer(&self) {
        self.state.set(ReaderState::Multi);
        self.num_consumers.fetch_add(1, Ordering::SeqCst);
    }

    pub fn remove_consumer(&self) -> usize {
        self.num_consumers.fetch_sub(1, Ordering::SeqCst)
    }

    pub fn get_consumers(&self) -> usize {
        self.num_consumers.load(Ordering::Relaxed)
    }
}

impl ReaderGroup {
    pub fn new() -> ReaderGroup {
        ReaderGroup { readers: Vec::new() }
    }

    /// Only safe to call from a consumer of the queue!
    pub unsafe fn add_reader(&self, raw: usize, wrap: u32) -> (*mut ReaderGroup, *mut Reader) {
        let new_reader = alloc::allocate(1);
        let new_group = alloc::allocate(1);
        ptr::write(new_reader,
                   Reader {
                       pos_data: CountedIndex::from_usize(raw, wrap),
                       state: Cell::new(ReaderState::Single),
                       num_consumers: AtomicUsize::new(1),
                   });
        let mut new_readers = self.readers.clone();
        new_readers.push(new_reader as *const Reader);
        ptr::write(new_group, ReaderGroup { readers: new_readers });
        (new_group, new_reader)
    }

    pub unsafe fn remove_reader(&self, reader: *const Reader) -> *mut ReaderGroup {
        let new_group = alloc::allocate(1);
        let mut new_readers = self.readers.clone();
        new_readers.retain(|pt| *pt != reader);
        ptr::write(new_group, ReaderGroup { readers: new_readers });
        new_group
    }

    pub fn get_max_diff(&self, cur_writer: usize) -> Option<u32> {
        let mut max_diff: usize = 0;
        unsafe {
            for reader_ptr in &self.readers {
                // If a reader has passed the writer during this function call
                // then what must have happened is that somebody else has completed this
                // written to the queue, and a reader has bypassed it. We should retry
                let rpos = (**reader_ptr).pos_data.load_count(MAYBE_ACQUIRE);
                let diff = cur_writer.wrapping_sub(rpos);
                if diff > (1 << 30) {
                    return None;
                }
                max_diff = if diff > max_diff { diff } else { max_diff };
            }
        }
        maybe_acquire_fence();

        assert!(max_diff <= (1 << 30));
        Some(max_diff as u32)
    }
}

impl ReadCursor {
    pub fn new(wrap: u32) -> (ReadCursor, AtomicPtr<Reader>) {
        let rg = ReaderGroup::new();
        unsafe {
            let (real_group, reader) = rg.add_reader(0, wrap);
            (ReadCursor { readers: AtomicPtr::new(real_group) }, AtomicPtr::new(reader))
        }
    }

    #[inline(always)]
    pub fn prefetch_metadata(&self) {
        unsafe {
            let rg = &*self.readers.load(Consume);
            let dummy_ptr: *const usize = mem::transmute(&rg.readers);
            ptr::read_volatile(dummy_ptr);
        }
    }

    pub fn get_max_diff(&self, cur_writer: usize) -> Option<u32> {
        loop {
            unsafe {
                let first_ptr = self.readers.load(Consume);
                let rg = &*first_ptr;
                let rval = rg.get_max_diff(cur_writer);
                // This check ensures that the pointer hasn't changed
                // We must first read the diff, *and then* check the pointer
                // for changes. This is extremely similar to the seqlock except
                // with the pointer as the sequence lock instead of a different int
                //
                // We don't need another acquire fence here since the
                // relevant loads in get_max_diff are going to ordered
                // before all loads after the function exit, and also
                // ordered after the original pointer load
                let second_ptr = self.readers.load(MAYBE_ACQUIRE);
                if second_ptr == first_ptr {
                    maybe_acquire_fence();
                    return rval;
                }
            }
        }
    }

    // There's no fundamental reason these need to leak,
    // I just haven't implemented the memory management yet.
    // It's not too hard since we can track readers and writers active
    pub fn add_reader(&self, reader: &Reader) -> AtomicPtr<Reader> {
        let mut current_ptr = self.readers.load(Consume);
        loop {
            unsafe {
                let current_group = &*current_ptr;
                let raw = reader.pos_data.load_raw(Ordering::Relaxed);
                let wrap = reader.pos_data.wrap_at();
                let (new_group, new_reader) = current_group.add_reader(raw, wrap);
                match self.readers
                    .compare_exchange(current_ptr, new_group, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => {
                        fence(Ordering::SeqCst);
                        return AtomicPtr::new(new_reader);
                    }
                    Err(val) => {
                        current_ptr = val;
                        alloc::deallocate(new_group, 1);
                        alloc::deallocate(new_reader, 1);
                    }
                }
            }
        }
    }

    pub fn remove_reader(&self, reader: &Reader) {
        let mut current_group = self.readers.load(Consume);
        loop {
            unsafe {
                let new_group = (*current_group).remove_reader(reader);
                match self.readers
                    .compare_exchange(current_group,
                                      new_group,
                                      Ordering::SeqCst,
                                      Ordering::SeqCst) {
                    Ok(_) => {
                        fence(Ordering::SeqCst);
                        break;
                    }
                    Err(val) => {
                        current_group = val;
                        alloc::deallocate(new_group, 1);
                    }
                }
            }
        }
    }
}