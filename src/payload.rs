//! ESB packet buffer pool with word-aligned DMA storage.
//!
//! Each packet is a fixed-size buffer preceded by a 4-byte [`EsbHeader`].
//! The layout is `[rssi, pipe, length, pid_no_ack, payload...]` where bytes 2+ are the DMA region.
//!
//! Buffer states are tracked with atomic state machines:
//! `free → tx_allocated → tx_queued → in_dma → free` (TX path)
//! `free → in_dma → rx_queued → free` (RX path)
//!
//! # Safety
//!
//! `Packet` is `pub(crate)` — it cannot be constructed or accessed outside this module.
//! All access goes through `PacketPool` methods, which enforce the state machine invariants.
//! This ensures no data race between the ISR and application context.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering, compiler_fence};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::header::EsbHeader;

/// Maximum ESB payload length.
pub const MAX_PAYLOAD: usize = 252;

/// Packet buffer states (atomic state machine).
mod state {
    pub const FREE: u8 = 0;
    pub const TX_ALLOCATED: u8 = 1;
    pub const TX_QUEUED: u8 = 2;
    pub const IN_DMA: u8 = 3;
    pub const RX_QUEUED: u8 = 4;
}

/// A single word-aligned packet buffer.
///
/// Layout: `[EsbHeader (4 bytes) | payload (SIZE bytes)]`.
/// DMA pointer points to offset 2 (the `length` field) per ESB hardware convention.
///
/// This type is `pub(crate)` to prevent external construction. All access is mediated
/// through `PacketPool`.
#[repr(C, align(4))]
pub(crate) struct Packet<const SIZE: usize> {
    data: UnsafeCell<[u8; SIZE]>,
}

#[allow(dead_code)]
impl<const SIZE: usize> Packet<SIZE> {
    /// Create a zeroed packet buffer.
    pub(crate) const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0u8; SIZE]),
        }
    }

    /// Get a pointer to the DMA region (offset 2 from start).
    pub(crate) fn dma_ptr(&self) -> *mut u8 {
        unsafe { (*self.data.get()).as_mut_ptr().add(EsbHeader::DMA_OFFSET) }
    }

    /// Access the full buffer as a mutable slice.
    ///
    /// # Safety
    ///
    /// Caller must hold exclusive access (guaranteed by pool state machine).
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn buf_mut(&self) -> &mut [u8; SIZE] {
        unsafe { &mut *self.data.get() }
    }

    /// Access the full buffer as a slice.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent mutable access.
    pub(crate) unsafe fn buf(&self) -> &[u8; SIZE] {
        unsafe { &*self.data.get() }
    }

    /// Access the header portion.
    ///
    /// # Safety
    ///
    /// Caller must hold exclusive access.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn header_mut(&self) -> &mut EsbHeader {
        unsafe {
            let ptr = self.data.get() as *mut EsbHeader;
            &mut *ptr
        }
    }

    /// Read-only access to the header portion.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent mutable access.
    pub(crate) unsafe fn header(&self) -> &EsbHeader {
        unsafe {
            let ptr = self.data.get() as *const EsbHeader;
            &*ptr
        }
    }
}

// SAFETY: Packet is pub(crate), only constructed by PacketPool::new().
// The pool's atomic state machine ensures no concurrent access to any Packet.
unsafe impl<const SIZE: usize> Send for Packet<SIZE> {}
unsafe impl<const SIZE: usize> Sync for Packet<SIZE> {}

const _: () = assert!(core::mem::align_of::<Packet<1>>() >= 4);

/// A pool of packet buffers with atomic state tracking and async queues.
///
/// `N` = number of packets, `SIZE` = buffer size per packet (including 4-byte header).
/// Typical sizes: `SIZE = 256` (4-byte header + 252-byte payload).
///
/// Must be placed in a `static` (e.g. via `static_cell::make_static`).
pub struct PacketPool<const N: usize, const SIZE: usize> {
    storage: [Packet<SIZE>; N],
    state: [AtomicU8; N],
    rx_queue: Channel<CriticalSectionRawMutex, usize, N>,
}

// SAFETY: PacketPool is thread-safe because all access is mediated by the
// atomic state machine. The embassy-sync Channels are Send+Sync.
unsafe impl<const N: usize, const SIZE: usize> Send for PacketPool<N, SIZE> {}
unsafe impl<const N: usize, const SIZE: usize> Sync for PacketPool<N, SIZE> {}

impl<const N: usize, const SIZE: usize> Default for PacketPool<N, SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const SIZE: usize> PacketPool<N, SIZE> {
    /// Create a zeroed pool.
    ///
    /// Must be placed in a `static` (e.g. via `static_cell`).
    pub const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const NEW: AtomicU8 = AtomicU8::new(state::FREE);
        Self {
            storage: [const { Packet::new() }; N],
            state: [NEW; N],
            rx_queue: Channel::new(),
        }
    }

    /// Allocate a packet for TX: transitions `free → tx_allocated`.
    ///
    /// Returns an index into the pool. Caller should fill the buffer then call
    /// `enqueue_tx()` or `release_tx()`.
    #[allow(clippy::manual_find)]
    pub fn alloc_tx(&self) -> Option<usize> {
        for i in 0..N {
            if self.state[i]
                .compare_exchange(
                    state::FREE,
                    state::TX_ALLOCATED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }

    /// Enqueue a TX packet for the ISR to send.
    pub async fn enqueue_tx(&self, index: usize) {
        debug_assert!(index < N);
        let queued = self.state[index]
            .compare_exchange(
                state::TX_ALLOCATED,
                state::TX_QUEUED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok();
        debug_assert!(queued, "enqueue_tx called for a non-allocated packet");
        compiler_fence(Ordering::Release);
    }

    /// Dequeue the next TX packet (called from ISR).
    pub fn try_dequeue_tx(&self) -> Option<usize> {
        for index in 0..N {
            if self.try_claim_queued_tx(index) {
                return Some(index);
            }
        }
        None
    }

    /// Dequeue a queued TX packet for a specific pipe (called from ISR).
    ///
    /// This is used by PRX ACK payload handling so payloads queued for one
    /// pipe cannot be consumed by packets received on another pipe. Stale
    /// queue entries are skipped later by `try_dequeue_tx()`.
    pub fn try_dequeue_tx_for_pipe(&self, pipe: u8) -> Option<usize> {
        for index in 0..N {
            if self.state[index].load(Ordering::Acquire) != state::TX_QUEUED {
                continue;
            }

            let header = unsafe { self.header(index) };
            if header.pipe == pipe && self.try_claim_queued_tx(index) {
                return Some(index);
            }
        }
        None
    }

    /// Transition a TX packet to DMA ownership: `tx_queued → in_dma`.
    ///
    /// Called by the ISR before setting PACKETPTR.
    pub fn tx_to_dma(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Release);
        if self.state[index]
            .compare_exchange(
                state::TX_QUEUED,
                state::IN_DMA,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            debug_assert_eq!(self.state[index].load(Ordering::Acquire), state::IN_DMA);
        }
    }

    /// Release a TX packet after DMA completes: `in_dma → free`.
    pub fn release_tx(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Acquire);
        self.state[index].store(state::FREE, Ordering::Release);
    }

    /// Cancel a TX allocation: `tx_allocated/tx_queued → free`.
    ///
    /// Use when a packet was allocated but will not be sent.
    pub fn cancel_tx(&self, index: usize) {
        debug_assert!(index < N);
        self.state[index].store(state::FREE, Ordering::Release);
    }

    fn try_claim_queued_tx(&self, index: usize) -> bool {
        debug_assert!(index < N);
        let ok = self.state[index]
            .compare_exchange(
                state::TX_QUEUED,
                state::IN_DMA,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok();
        if ok {
            compiler_fence(Ordering::Acquire);
        }
        ok
    }

    /// Transition an RX packet to DMA ownership: `free → in_dma`.
    ///
    /// Returns false if the slot was not free.
    pub fn rx_to_dma(&self, index: usize) -> bool {
        debug_assert!(index < N);
        let ok = self.state[index]
            .compare_exchange(
                state::FREE,
                state::IN_DMA,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok();
        if ok {
            compiler_fence(Ordering::Release);
        }
        ok
    }

    /// Complete RX: transition `in_dma → rx_queued` and enqueue for app.
    pub fn rx_complete(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Acquire);
        self.state[index].store(state::RX_QUEUED, Ordering::Release);
        let _ = self.rx_queue.try_send(index);
    }

    /// Receive the next RX packet (async, called from app context).
    pub async fn receive_rx(&self) -> usize {
        self.rx_queue.receive().await
    }

    /// Try to receive an RX packet without blocking.
    pub fn try_receive_rx(&self) -> Option<usize> {
        self.rx_queue.try_receive().ok()
    }

    /// Release an RX packet back to the pool: `rx_queued → free`.
    pub fn release_rx(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Acquire);
        self.state[index].store(state::FREE, Ordering::Release);
    }
}

#[allow(dead_code)]
impl<const N: usize, const SIZE: usize> PacketPool<N, SIZE> {
    /// Get the DMA pointer for a packet by index.
    ///
    /// # Safety
    ///
    /// Caller must ensure the packet is in IN_DMA state.
    pub(crate) unsafe fn dma_ptr(&self, index: usize) -> *mut u8 {
        self.storage[index].dma_ptr()
    }

    /// Get mutable buffer access for a packet by index.
    ///
    /// # Safety
    ///
    /// Caller must ensure exclusive access per state machine.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn buf_mut(&self, index: usize) -> &mut [u8; SIZE] {
        unsafe { self.storage[index].buf_mut() }
    }

    /// Get the header for a packet by index.
    ///
    /// # Safety
    ///
    /// Caller must ensure exclusive access per state machine.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn header_mut(&self, index: usize) -> &mut EsbHeader {
        unsafe { self.storage[index].header_mut() }
    }

    /// Get read-only access to a packet header by index.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent mutable access.
    pub(crate) unsafe fn header(&self, index: usize) -> &EsbHeader {
        unsafe { self.storage[index].header() }
    }

    /// Get read access to a packet buffer by index.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent mutable access.
    pub(crate) unsafe fn buf(&self, index: usize) -> &[u8; SIZE] {
        unsafe { self.storage[index].buf() }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{PacketPool, state};
    use core::future::Future;
    use core::task::{Context, Poll};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    struct NoopWaker;

    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    fn assert_ready<F: Future<Output = ()>>(future: F) {
        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        let mut future = core::pin::pin!(future);

        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(())));
    }

    #[test]
    fn tx_allocation_is_not_visible_until_enqueued() {
        let pool = PacketPool::<2, 16>::new();

        let idx = pool.alloc_tx().expect("free tx slot");
        assert_eq!(pool.state[idx].load(core::sync::atomic::Ordering::Acquire), state::TX_ALLOCATED);
        assert_eq!(pool.try_dequeue_tx(), None);

        unsafe {
            let header = pool.header_mut(idx);
            header.pipe = 3;
            header.length = 4;
        }

        assert_ready(pool.enqueue_tx(idx));

        let claimed = pool.try_dequeue_tx().expect("queued tx slot");
        assert_eq!(claimed, idx);
        assert_eq!(pool.state[idx].load(core::sync::atomic::Ordering::Acquire), state::IN_DMA);

        pool.release_tx(idx);
        assert_eq!(pool.state[idx].load(core::sync::atomic::Ordering::Acquire), state::FREE);
    }

    #[test]
    fn pipe_filtered_dequeue_claims_only_matching_pipe() {
        let pool = PacketPool::<3, 16>::new();

        let pipe0 = pool.alloc_tx().expect("pipe0 slot");
        let pipe1 = pool.alloc_tx().expect("pipe1 slot");

        unsafe {
            pool.header_mut(pipe0).pipe = 0;
            pool.header_mut(pipe1).pipe = 1;
        }

        assert_ready(pool.enqueue_tx(pipe0));
        assert_ready(pool.enqueue_tx(pipe1));

        assert_eq!(pool.try_dequeue_tx_for_pipe(2), None);

        let claimed_pipe1 = pool.try_dequeue_tx_for_pipe(1).expect("pipe1 queued slot");
        assert_eq!(claimed_pipe1, pipe1);
        assert_eq!(pool.state[pipe1].load(core::sync::atomic::Ordering::Acquire), state::IN_DMA);
        assert_eq!(pool.state[pipe0].load(core::sync::atomic::Ordering::Acquire), state::TX_QUEUED);

        let claimed_any = pool.try_dequeue_tx().expect("remaining queued slot");
        assert_eq!(claimed_any, pipe0);

        pool.release_tx(pipe0);
        pool.release_tx(pipe1);
    }

    #[test]
    fn cancel_tx_releases_allocated_or_queued_slots() {
        let pool = PacketPool::<1, 16>::new();

        let idx = pool.alloc_tx().expect("free tx slot");
        pool.cancel_tx(idx);
        assert_eq!(pool.state[idx].load(core::sync::atomic::Ordering::Acquire), state::FREE);

        let idx = pool.alloc_tx().expect("reused tx slot");
        assert_ready(pool.enqueue_tx(idx));
        pool.cancel_tx(idx);
        assert_eq!(pool.state[idx].load(core::sync::atomic::Ordering::Acquire), state::FREE);
    }
}
