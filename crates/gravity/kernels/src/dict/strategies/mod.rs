//! `dict::strategies` (v5 serial step, landing 5: module lines written by modexit.py).
pub mod countwalk;
pub mod front_light;
pub mod heads;
pub mod like_word;
pub mod percode;
pub mod percode_const;
/// The verdict flags of the count walk and the constant-key window (B3-06, B3-05), for lowerv4's dict strategies.
pub use countwalk::CW_VERDICT;
pub use percode::Fold;
pub use percode_const::{percode_const_census, Verdicts, PK_VERDICT};
