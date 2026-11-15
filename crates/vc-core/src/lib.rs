//! Domain types shared by every `vcd` crate: ids, the job state machine, error codes, the
//! language set and the clock. Pure code, no IO.

pub mod clock;
pub mod error;
pub mod ids;
pub mod job;
pub mod lang;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::ErrorCode;
pub use ids::{ConsentId, JobId, VoiceId};
pub use job::JobState;
pub use lang::Language;
