//! Test-only allocation instrumentation. It is never a production host dependency.
//! Unsafe allocator forwarding is isolated here; callers install the allocator and measure
//! their callback through safe APIs. Counts include only the calling thread's enabled spans.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! {
    static TRACK: Cell<bool> = const {Cell::new(false)};
    static ALLOCATIONS: Cell<usize> = const {Cell::new(0)};
}
/// Install with `#[global_allocator] static A: CountingSystem = CountingSystem;` in test binaries.
pub struct CountingSystem;
fn count() {
    let _ = TRACK.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get().wrapping_add(1)));
        }
    });
}
// SAFETY: every operation forwards the unchanged pointer/layout to the standard System
// allocator. Instrumentation stores only thread-local integers and never accesses allocations.
unsafe impl GlobalAlloc for CountingSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout; System owns the result.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: the caller supplies a live System allocation and its original layout.
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        count();
        // SAFETY: GlobalAlloc's realloc contract is passed unchanged to System.
        unsafe { System.realloc(p, layout, new) }
    }
}
struct TrackingGuard(bool);
impl Drop for TrackingGuard {
    fn drop(&mut self) {
        TRACK.with(|enabled| enabled.set(self.0));
    }
}
/// Runs `f` and returns its result and allocation/reallocation calls on this thread.
/// Nested measurements are supported; unwinding restores the previous tracking state.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let start = ALLOCATIONS.with(Cell::get);
    let previous = TRACK.with(|enabled| enabled.replace(true));
    let guard = TrackingGuard(previous);
    let result = f();
    let count = ALLOCATIONS.with(Cell::get).wrapping_sub(start);
    drop(guard);
    (result, count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[global_allocator]
    static ALLOCATOR: CountingSystem = CountingSystem;
    #[test]
    fn counter_has_independent_positive_and_zero_controls() {
        let (value, count) = measure(|| std::hint::black_box(3) + std::hint::black_box(4));
        assert_eq!((value, count), (7, 0));
        let (value, count) = measure(|| {
            let mut bytes = Vec::with_capacity(std::hint::black_box(8));
            bytes.extend_from_slice(&[1u8; 8]);
            bytes.reserve(std::hint::black_box(4096));
            std::hint::black_box(bytes)
        });
        assert!(count >= 2, "allocation and growth must be observed");
        assert_eq!(value.len(), 8);
    }
    #[test]
    fn nested_and_unwound_measurements_restore_thread_state() {
        let (_, outer) = measure(|| {
            let (_, inner) = measure(|| std::hint::black_box(Box::new(std::hint::black_box(4))));
            assert!(inner > 0);
        });
        assert!(outer > 0);
        let panic = std::panic::catch_unwind(|| measure(|| panic!("instrumentation unwind control")));
        assert!(panic.is_err());
        assert!(!TRACK.with(Cell::get));
        let (_, count) = measure(|| std::hint::black_box(Box::new(std::hint::black_box(8))));
        assert!(count > 0);
    }
}
