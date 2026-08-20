//! Task 100's table has its tests in `tui::commands::tests`, which drives them
//! through the shared dispatcher — the layer they are actually about.
//!
//! This module exists so `mod tests;` in `compose.rs` resolves, and so the split
//! task 95 made is visible: `tui::commands::tag` and `::rule` keep their own,
//! because their verify lines name those paths and a filter has to select them.
