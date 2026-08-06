# Resume state — parallel build

Written when a session usage limit terminated ten in-flight agents at once.
`tasks.md` remains the definitive progress tracker; this file only records the
part that is not derivable from it — which worktree branch holds which task's
unfinished work.

## Merged and checked off

Tasks 25, 27, 38, 43, 86 are on `docs/tasks-breakdown`, each verified on the
combined tree (not just in its own worktree): 757/757 tests, clippy clean,
`cargo deny`/`cargo audit` clean, `buf lint` + `buf breaking` clean, gitleaks
clean across all commits.

## Unfinished work preserved on branches

Each branch below has a `wip(...)` commit holding the working tree exactly as
the agent left it. **None of these passed the gate and none were reviewed.**
Resume from the branch rather than restarting the task — several were close to
done.

| Task | Branch | State when interrupted |
|---|---|---|
| 26 QueryPlan assembly | `worktree-agent-a6a741ecfa7b46c3f` | Implementation complete, gate green, about to run the reviewer |
| 44 PII redaction firewall | `worktree-agent-a8f26c0144fe769bf` | Implementation complete, about to run the reviewer |
| 23 OCR path | `worktree-agent-a92770680c94c9608` | Implementation largely complete (Vision + Tesseract backends, migration, config) |
| 45 AI audit ledger | `worktree-agent-acc659bc3822f8b59` | Core + proto + service + migration written, untested |
| 46 AI policy engine | `worktree-agent-aa5abdb61ba5c430f` | Mid-edit on the resolver |
| 39 MailService | `worktree-agent-a4179a6fb64b7fc4d` | Compiling, tests not yet run |

Tasks 24, 56, 67, 71, 79 were launched but died early enough that nothing is
worth keeping; start them fresh.

## Reserved migration numbers

`V14` api_tokens (merged) · `V15` OCR provenance (task 23) · `V16` query vocab
(task 26) · `V17` unused (task 27 needed none) · `V18` ai_ledger (task 45) ·
`V19` ai_policy (task 46) · `V20` redaction (task 44, likely unused) ·
`V21` mail service (task 39) · `V22` index service (task 24) · `V23` notes
(task 56) · `V24` oauth (task 79) · `V25` hooks (task 67) · `V26` response
times (task 71). Next free: `V27`.

## Orchestration notes worth not relearning

- **Each worktree must build in its own `target/`.** Sharing one build
  directory across worktrees corrupts results: cargo uplifts binaries to
  `target/debug/<name>`, that path is not keyed by source path, and
  `CARGO_BIN_EXE_<name>` — which the `rmail-cli` tests use to exec `mail` —
  resolves to it. See `.claude/BUILD_BRIEF.md`.
- A worktree `target/` costs 3–8 GB. Delete it after merging the branch.
- Ten concurrent agents exhausted the session usage limit. Fewer, longer-lived
  agents get further per unit of quota than many short ones.
