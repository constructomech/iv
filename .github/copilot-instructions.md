Before committing any changes, review [ARCHITECTURE.md](ARCHITECTURE.md) and update it if the change affects the Grid data structure, tile state machine, worker threading model, thumbnail extraction pipeline, or rendering approach.

Never pipe build or test commands through filters (e.g. `| Select-String`, `| grep`). Run the command directly so progress is visible in the terminal, then grep the output afterwards if needed.
