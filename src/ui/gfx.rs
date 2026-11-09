//! Cell shape. Terminals are about twice as tall as they are wide, and real ones are measured, so
//! anything drawn round (the splash's lettering) can be drawn round.
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

// It is set by the main loop before each frame and read by the drawing code, which keeps every
// drawing function free of a context parameter.
static ASPECT_BITS: AtomicU32 = AtomicU32::new(0x4000_0000); // 2.0f32

/// Cell height over cell width.
pub fn aspect() -> f32 {
    f32::from_bits(ASPECT_BITS.load(Relaxed))
}
pub fn set_aspect(v: f32) {
    ASPECT_BITS.store(v.clamp(1.2, 3.0).to_bits(), Relaxed);
}
