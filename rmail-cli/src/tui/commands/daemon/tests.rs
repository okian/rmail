//! Task 94's table has its tests in `tui::commands::tests`, which drives them
//! through the shared dispatcher — the layer they are actually about.
//!
//! This module exists so `mod tests;` in `daemon.rs` resolves and so the split
//! is visible: `tui::commands::tag` and `::rule` (task 95) keep their own,
//! because their verify lines name those paths and a filter has to select them.
