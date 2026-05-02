//! ESB packet buffer pool with word-aligned DMA storage.
//!
//! Each packet is a fixed-size buffer preceded by a 4-byte [`EsbHeader`].
//! The layout is `[rssi, pipe, length, pid_no_ack, payload...]` where bytes 2+ are the DMA region.
//!
//! Buffer states are tracked with atomic state machines:
//! `free → tx_queued → in_dma → free` (TX path)
//! `free → in_dma → rx_queued → free` (RX path)
//!
//! # Safety
//!
//! `Packet` is `pub(crate)` — it cannot be constructed or accessed outside this module.
//! All access goes through `PacketPool` methods, which enforce the state machine invariants.
//! This ensures no data race between the ISR and application context.

use core::cell::UnsafeCell;
use core::sync::atomic::{compiler_fence, AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::header::EsbHeader;

/// Maximum ESB payload length.
pub const MAX_PAYLOAD: usize = 252;

/// Packet buffer states (atomic state machine).
mod state {
    pub const FREE: u8 = 0;
    pub const TX_QUEUED: u8 = 1;
    pub const IN_DMA: u8 = 2;
    pub const RX_QUEUED: u8 = 3;
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
    tx_queue: Channel<CriticalSectionRawMutex, usize, N>,
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
            tx_queue: Channel::new(),
            rx_queue: Channel::new(),
        }
    }

    /// Allocate a packet for TX: transitions `free → tx_queued`.
    ///
    /// Returns an index into the pool. Caller should fill the buffer then call
    /// `enqueue_tx()` or `release_tx()`.
    #[allow(clippy::manual_find)]
    pub fn alloc_tx(&self) -> Option<usize> {
        for i in 0..N {
            if self.state[i]
                .compare_exchange(
                    state::FREE,
                    state::TX_QUEUED,
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
        self.tx_queue.send(index).await;
    }

    /// Dequeue the next TX packet (called from ISR).
    pub fn try_dequeue_tx(&self) -> Option<usize> {
        self.tx_queue.try_receive().ok()
    }

    /// Transition a TX packet to DMA ownership: `tx_queued → in_dma`.
    ///
    /// Called by the ISR before setting PACKETPTR.
    pub fn tx_to_dma(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Release);
        self.state[index].store(state::IN_DMA, Ordering::Release);
    }

    /// Release a TX packet after DMA completes: `in_dma → free`.
    pub fn release_tx(&self, index: usize) {
        debug_assert!(index < N);
        compiler_fence(Ordering::Acquire);
        self.state[index].store(state::FREE, Ordering::Release);
    }

    /// Cancel a TX allocation: `tx_queued → free`.
    ///
    /// Use when a packet was allocated but will not be sent.
    pub fn cancel_tx(&self, index: usize) {
        debug_assert!(index < N);
        self.state[index].store(state::FREE, Ordering::Release);
    }

    /// Transition an RX packet to DMA ownership: `free → in_dma`.
    ///
    /// Returns false if the slot was not free.
    pub fn rx_to_dma(&self, index: usize) -> bool {
        debug_assert!(index < N);
        let ok = self.state[index]
            .compare_exchange(state::FREE, state::IN_DMA, Ordering::AcqRel, Ordering::Relaxed)
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
