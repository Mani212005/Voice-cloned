//! Text pipeline: normalization and the chunk planner.
//!
//! The engine is run once per chunk. The first sentence is a chunk of its own, so audio
//! starts sooner. A chunk is at most `max_words` words (19 for the turbo2 recipe, 14 for
//! Hindi, measured to stay within the 1.1 GB engine budget) and at most `max_chars`
//! characters. Chunks are cut at sentence boundaries first, then at clause punctuation,
//! then between words. Pure functions only.

mod normalize;
mod plan;

pub use normalize::normalize;
pub use plan::{Chunk, ChunkPlan, PlanError, PlanLimits, plan};
