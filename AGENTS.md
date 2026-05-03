Before committing any changes, review `ARCHITECTURE.md` and update it if the change affects the Grid data structure, tile state machine, worker threading model, thumbnail extraction pipeline, or rendering approach.

Before committing any changes, run `cargo run --bin lint-i18n` and fix any hard-coded UI strings it reports.

Never pipe build or test commands through filters (for example, `| Select-String`, `| Select-Object`, or `| grep`). Run the command directly so progress is visible in the terminal, then grep the output afterwards if needed.
