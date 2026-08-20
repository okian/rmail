//! The config-block presentation: what a reader can see, and what it promises.
#![allow(clippy::panic)]

use super::*;

fn block(reason: ReadOnlyReason) -> ConfigBlock {
    ConfigBlock::new(
        "a hook",
        "[[hooks.hooks]]\nname = \"notify\"\nevent = \"on_new_message\"\n",
        PathBuf::from("/home/ada/.config/rmail/config.toml"),
        reason,
        "restart rmaild for it to take effect",
    )
}

#[test]
fn the_block_is_drawn_one_line_per_row() {
    // Folded into one cell it would be elided at the column width, and a block a
    // reader cannot see is a block they cannot check before pasting.
    let rows = block(ReadOnlyReason::ConfigFileOnly).rows();
    let toml: Vec<&str> = rows
        .iter()
        .take(3)
        .map(|row| row.cells[1].as_str())
        .collect();
    assert_eq!(
        toml,
        vec![
            "[[hooks.hooks]]",
            "name = \"notify\"",
            "event = \"on_new_message\"",
        ]
    );
}

#[test]
fn the_file_and_when_it_takes_effect_are_both_named() {
    // A block with no path is a block somebody has to guess the destination of,
    // and a hook pasted into a running daemon's config does nothing until it
    // restarts — which is exactly the surprise worth spending a row on.
    let rows = block(ReadOnlyReason::ConfigFileOnly).rows();
    let cell = |what: &str| {
        rows.iter()
            .find(|row| row.cells[0] == what)
            .map(|row| row.cells[1].clone())
            .unwrap_or_else(|| panic!("no {what} row"))
    };
    assert!(cell("file").ends_with("config.toml"), "{}", cell("file"));
    assert!(cell("effect").contains("restart"), "{}", cell("effect"));
}

#[test]
fn a_setting_with_no_rpc_says_so_and_one_with_an_rpc_names_it() {
    // The distinction a reader needs: "the config file is the only way" and "the
    // config file is one of two ways" are different situations, and collapsing
    // them would either hide an available verb or imply one that does not exist.
    let rows = block(ReadOnlyReason::ConfigFileOnly).rows();
    let by = rows.last().expect("a written-by row");
    assert!(
        by.cells[1].contains("nothing changes this over the wire"),
        "{:?}",
        by.cells
    );

    let rows = block(ReadOnlyReason::AlsoOverTheWire("account new")).rows();
    let by = rows.last().expect("a written-by row");
    assert!(by.cells[1].contains(":account new"), "{:?}", by.cells);
}
