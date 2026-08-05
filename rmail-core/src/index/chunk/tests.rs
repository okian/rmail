//! Boundary quality, span fidelity and the properties a stored chunk has to
//! keep for the vector attached to it to still mean anything.

use super::*;

/// A spec sized in bytes rather than tokens, for readable fixtures.
fn spec(bytes: usize, overlap_bytes: usize) -> ChunkSpec {
    ChunkSpec {
        tokens: bytes / BYTES_PER_TOKEN,
        overlap: overlap_bytes / BYTES_PER_TOKEN,
    }
}

#[test]
fn short_text_is_one_chunk() {
    let chunks = split("A short note about the invoice.", ChunkSpec::default());
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "A short note about the invoice.");
    assert_eq!(chunks[0].ordinal, 0);
}

#[test]
fn empty_and_blank_text_produce_nothing() {
    // A scanned PDF with no text layer, or a part that is only whitespace.
    // Embedding either produces a point that matches everything equally.
    for text in ["", "   ", "\n\n\t\n"] {
        assert!(split(text, ChunkSpec::default()).is_empty(), "{text:?}");
    }
}

#[test]
fn a_span_locates_the_chunk_in_the_original_text() {
    // A citation quotes the source, not a copy. If the span does not point at
    // the text the chunk holds, every quotation is subtly wrong — and nothing
    // downstream can tell, because both halves look plausible.
    let text = "First paragraph here.\n\nSecond paragraph, quite a bit longer than \
                the first one so that it lands in its own chunk.\n\nThird and last.";
    for chunk in split(text, spec(64, 8)) {
        assert_eq!(
            &text[chunk.span_start..chunk.span_end],
            chunk.text,
            "span {}..{} does not hold {:?}",
            chunk.span_start,
            chunk.span_end,
            chunk.text
        );
    }
}

#[test]
fn a_chunk_never_splits_a_word() {
    // Half a word is a token the model has never seen, on both sides of the
    // cut. It is the one boundary rule with no fallback below it.
    let text = "supercalifragilistic ".repeat(40);
    for chunk in split(&text, spec(64, 8)) {
        assert!(
            !chunk.text.starts_with("alifrag") && !chunk.text.ends_with("supercal"),
            "split mid-word: {:?}",
            chunk.text
        );
        for word in chunk.text.split_whitespace() {
            assert_eq!(word, "supercalifragilistic", "fragment {word:?}");
        }
    }
}

#[test]
fn a_paragraph_break_is_preferred_over_a_word_break() {
    // The whole point of choosing boundaries rather than counting to them.
    let first = "a".repeat(50);
    let second = "b".repeat(50);
    let text = format!("{first} words here.\n\n{second} words here.");
    let chunks = split(&text, spec(80, 0));
    assert!(chunks.len() >= 2);
    assert!(
        chunks[0].text.ends_with("words here."),
        "the first chunk should end at the blank line: {:?}",
        chunks[0].text
    );
}

#[test]
fn a_sentence_end_is_preferred_over_a_word_break() {
    let text = format!(
        "{} Second sentence follows here and runs on for a while so that it is \
         its own chunk rather than being absorbed into the first.",
        "word ".repeat(20)
    );
    let chunks = split(&text, spec(100, 0));
    assert!(chunks.len() >= 2);
    assert!(
        chunks[0].text.ends_with('.') || chunks[0].text.ends_with("word"),
        "{:?}",
        chunks[0].text
    );
}

#[test]
fn a_decimal_point_is_not_a_sentence_end() {
    // Without the "whitespace must follow" rule, "v1.2" and "example.com"
    // become sentence boundaries and every chunk ends mid-identifier.
    let text = format!("Version 1.2 of example.com is out. {}", "word ".repeat(30));
    for chunk in split(&text, spec(80, 0)) {
        assert!(
            !chunk.text.ends_with("1.") && !chunk.text.ends_with("example."),
            "split inside an identifier: {:?}",
            chunk.text
        );
    }
}

#[test]
fn consecutive_chunks_overlap() {
    // A passage straddling a boundary must be whole in one chunk or the other.
    let text = (0..60)
        .map(|n| format!("sentence number {n} here. "))
        .collect::<String>();
    let chunks = split(&text, spec(200, 60));
    assert!(chunks.len() >= 3);
    for pair in chunks.windows(2) {
        assert!(
            pair[1].span_start < pair[0].span_end,
            "chunk {} starts at {} but {} ends at {}",
            pair[1].ordinal,
            pair[1].span_start,
            pair[0].ordinal,
            pair[0].span_end
        );
    }
}

#[test]
fn chunks_cover_the_whole_text() {
    // A gap between two chunks is text that is not searchable and that nobody
    // will ever notice is missing.
    let text = (0..80)
        .map(|n| format!("sentence number {n} here. "))
        .collect::<String>();
    let chunks = split(&text, spec(200, 40));
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].span_start, 0);
    for pair in chunks.windows(2) {
        assert!(
            pair[1].span_start <= pair[0].span_end,
            "a gap between chunk {} and {}",
            pair[0].ordinal,
            pair[1].ordinal
        );
    }
    let last = chunks.last().unwrap_or_else(|| unreachable!());
    assert_eq!(last.span_end, text.trim_end().len());
}

#[test]
fn ordinals_are_dense_and_in_order() {
    // `(message, part, ordinal)` is a chunk's identity, and a stored vector is
    // attached to it. A gap or a repeat re-points a vector at different text.
    let text = "word ".repeat(400);
    let chunks = split(&text, spec(120, 20));
    for (n, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ordinal, n);
    }
}

#[test]
fn splitting_is_deterministic() {
    // Vectors are cached against a chunk's content hash. A split that varied
    // between runs would re-embed the whole mailbox on every pass.
    let text = "The quarterly invoice. ".repeat(100);
    assert_eq!(split(&text, spec(200, 40)), split(&text, spec(200, 40)));
}

#[test]
fn multi_byte_whitespace_never_splits_a_character() {
    // The separator itself being multi-byte is the case that breaks a splitter:
    // `rfind` returns where the character *starts*, so stepping past it by one
    // byte lands inside it. U+00A0 is what a pasted web page is full of.
    for sep in ["\u{a0}", "\u{3000}", "\u{2009}", "\u{2028}"] {
        let text = format!("word{sep}").repeat(80);
        let chunks = split(&text, spec(128, 16));
        assert!(!chunks.is_empty(), "{sep:?}");
        let mut covered = 0usize;
        for chunk in &chunks {
            assert!(
                text.is_char_boundary(chunk.span_start) && text.is_char_boundary(chunk.span_end),
                "span {}..{} is not a valid slice for {sep:?}",
                chunk.span_start,
                chunk.span_end
            );
            assert_eq!(&text[chunk.span_start..chunk.span_end], chunk.text);
            assert!(
                chunk.span_start <= covered,
                "a {} byte gap before chunk {} with {sep:?}",
                chunk.span_start - covered,
                chunk.ordinal
            );
            covered = covered.max(chunk.span_end);
        }
        assert_eq!(covered, text.trim_end().len(), "text lost with {sep:?}");
    }
}

#[test]
fn a_paragraph_break_is_preferred_over_a_line_break() {
    // The tiers are only a preference order if each is distinguishable from the
    // next. A blank line ends a topic; a single newline is often just a wrapped
    // line inside one.
    // Both candidates sit past the two-thirds floor, and the single newline is
    // the *later* of the two — so only the preference order can decide it.
    let text = format!(
        "{}\n\nbravo bravo\nand a wrapped continuation of the same thought {}",
        "alpha ".repeat(14),
        "charlie ".repeat(20)
    );
    let chunks = split(&text, spec(120, 0));
    assert!(chunks.len() >= 2);
    assert!(
        chunks[0].text.ends_with("alpha"),
        "the blank line should have won over the later single newline: {:?}",
        chunks[0].text
    );
}

#[test]
fn a_line_break_is_preferred_over_a_sentence_end() {
    // The tiers have to be distinguishable or the preference order is a comment
    // rather than behavior. Here a sentence ends well before the window and a
    // newline sits later in it: the newline must win.
    let text = format!(
        "{}. and then a clause that carries on without stopping\n{}",
        "word ".repeat(10),
        "tail ".repeat(30)
    );
    let chunks = split(&text, spec(120, 0));
    assert!(chunks.len() >= 2);
    assert!(
        chunks[0].text.ends_with("stopping"),
        "the newline should have won over the earlier full stop: {:?}",
        chunks[0].text
    );
}

#[test]
fn multi_byte_text_never_splits_a_character() {
    // Slicing a UTF-8 string at an arbitrary byte offset panics, and every
    // offset here is arithmetic on a byte budget meeting arbitrary mail.
    for text in [
        "日本語のテキストがここにあります。".repeat(60),
        "Ελληνικά κείμενα εδώ. ".repeat(60),
        "emoji 🎉 everywhere 🎊 ".repeat(60),
    ] {
        let chunks = split(&text, spec(120, 20));
        assert!(!chunks.is_empty());
        for chunk in chunks {
            assert_eq!(&text[chunk.span_start..chunk.span_end], chunk.text);
        }
    }
}

#[test]
fn text_with_no_separators_at_all_still_terminates() {
    // One enormous "word" — a base64 blob that survived extraction, say. There
    // is no boundary to prefer, and a splitter that insists on one loops.
    let text = "x".repeat(10_000);
    let chunks = split(&text, spec(200, 40));
    assert!(!chunks.is_empty());
    assert!(chunks.len() < 200, "produced {} chunks", chunks.len());
}

#[test]
fn a_trailing_fragment_is_absorbed_rather_than_embedded_alone() {
    // "Thanks," on its own is a point in vector space that matches every polite
    // message ever sent. Dropping it would be worse still: text missing from a
    // search index is not something anyone notices.
    //
    // The fixture has to actually reach the absorb branch: an earlier version
    // left a 114-byte tail against a 48-byte floor, so the branch never ran and
    // setting the floor to zero changed nothing.
    let text = format!("{}. Thanks,", "word ".repeat(49));
    let chunks = split(&text, spec(60, 0));
    assert!(
        chunks.len() > 1,
        "the fixture must split at all: {chunks:?}"
    );
    // A literal, not `MIN_CHUNK_BYTES`: comparing the output against the very
    // constant that produced it is self-referential, and setting that constant
    // to zero would then satisfy the assertion by construction.
    for chunk in &chunks {
        assert!(
            chunk.text.len() >= 40,
            "chunk {} is {} bytes — a fragment that short is a point in vector \
             space matching every polite message ever sent: {:?}",
            chunk.ordinal,
            chunk.text.len(),
            chunk.text
        );
    }
    assert!(
        !chunks.iter().any(|c| c.text == "Thanks,"),
        "{:?}",
        chunks.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
    let last = chunks.last().unwrap_or_else(|| unreachable!());
    assert!(last.text.ends_with("Thanks,"), "{:?}", last.text);
    assert_eq!(last.span_end, text.len(), "no text was lost");
}

#[test]
fn a_message_that_is_only_a_fragment_is_still_chunked() {
    // The exception to the rule above: dropping short chunks must not drop the
    // whole message. A three-word reply is a real message somebody may search
    // for.
    let chunks = split("Sounds good.", ChunkSpec::default());
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "Sounds good.");
}

#[test]
fn a_degenerate_spec_cannot_produce_a_non_terminating_split() {
    // These come from a config file. An overlap at or above the window makes
    // every chunk start where the last one did.
    use crate::config::IndexSemanticConfig;

    for (tokens, overlap) in [(0, 0), (1, 100), (512, 512), (512, 100_000), (0, 0)] {
        let spec = ChunkSpec::from_config(&IndexSemanticConfig {
            chunk_tokens: tokens,
            chunk_overlap: overlap,
            ..IndexSemanticConfig::default()
        });
        assert!(spec.overlap < spec.tokens, "{tokens}/{overlap} → {spec:?}");
        let chunks = split(&"word ".repeat(500), spec);
        assert!(!chunks.is_empty());
        assert!(
            chunks.len() < 2000,
            "{tokens}/{overlap} produced {}",
            chunks.len()
        );
    }
}

#[test]
fn token_estimates_are_never_zero() {
    // A zero-token chunk would violate the schema check and divide by zero in
    // any context-window budget that reads it.
    assert_eq!(estimate_tokens(""), 1);
    assert_eq!(estimate_tokens("a"), 1);
    assert_eq!(estimate_tokens(&"a".repeat(4)), 1);
    assert_eq!(estimate_tokens(&"a".repeat(5)), 2);
}

#[test]
fn the_span_contract_holds_for_anything_at_all() {
    // A deterministic fuzz over an alphabet chosen to be hostile: multi-byte
    // whitespace, CRLF, sentence punctuation, and characters that are three and
    // four bytes wide. Hand-picked fixtures found none of the multi-byte
    // whitespace failures — they all used ASCII separators, which is the shape
    // a person reaches for.
    const ALPHABET: [&str; 14] = [
        "a",
        "word",
        " ",
        "\u{a0}",
        "\u{3000}",
        "\u{2009}",
        "\n",
        "\r\n",
        "\n\n",
        ".",
        "! ",
        "日本語",
        "🎉",
        "",
    ];
    // A fixed LCG rather than a random seed: a fuzz that finds a bug once and
    // cannot be re-run on the same input has told you very little.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..4000u32 {
        let len = (next() % 200) as usize;
        let text: String = (0..len)
            .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
            .collect();
        let spec = ChunkSpec {
            tokens: 8 + (next() % 60) as usize,
            overlap: (next() % 8) as usize,
        };
        let chunks = split(&text, spec);

        let trimmed_end = text.trim_end().len();
        if trimmed_end == 0 {
            assert!(chunks.is_empty(), "case {case}: {text:?}");
            continue;
        }
        assert!(!chunks.is_empty(), "case {case}: {text:?}");

        let mut covered = 0usize;
        for (n, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.ordinal, n, "case {case}");
            assert!(
                text.is_char_boundary(chunk.span_start) && text.is_char_boundary(chunk.span_end),
                "case {case}: span {}..{} is not a valid slice of {text:?}",
                chunk.span_start,
                chunk.span_end
            );
            assert_eq!(
                &text[chunk.span_start..chunk.span_end],
                chunk.text,
                "case {case}: the span and the text disagree"
            );
            assert!(chunk.tokens > 0, "case {case}: the schema forbids zero");
            // A gap is allowed only where there was nothing to index: chunk
            // spans start at the first character, so the whitespace between two
            // chunks belongs to neither. Anything else is text that is silently
            // unsearchable, which is the kind of loss nobody ever notices.
            if chunk.span_start > covered {
                let gap = &text[covered..chunk.span_start];
                assert!(
                    gap.trim().is_empty(),
                    "case {case}: {gap:?} is in no chunk at all"
                );
            }
            covered = covered.max(chunk.span_end);
        }
        assert_eq!(covered, trimmed_end, "case {case}: text lost from {text:?}");
    }
}
