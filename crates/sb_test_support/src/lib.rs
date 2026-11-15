//! What scrollback's tests share (docs/PLAN.md §12): generated fixture transcript trees, a
//! location root in a temp dir, the loopback launcher and the test-only database provider.
//! Tests never read real transcripts.
//!
//! gpui: **never** (`script/check` step 3). Empty until phase 1 builds the first fixtures
//! (PLAN §13).
