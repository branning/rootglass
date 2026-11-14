# Ergo journal recovery

On 2026-11-14, Ergo 6.0.6 rejected two YFYS3W `result` entries because their summaries exceeded the 120-character limit. Both original summaries below were copied from the untouched pre-repair journal backup. Only those two summary strings were shortened in `.ergo/journal.jsonl`.

## Entry at 2026-11-12T06:07:24Z

Original (161 characters), verbatim:

> Installed smoke #43164718587 booted, but the first console byte was consumed before its cursor-query response; changing the gate to hold the full submitted line.

Current journal summary (120 characters):

> Windows smoke #43164718587 booted; cursor-query handling consumed the first console byte; gate fix remained in progress.

## Entry at 2026-11-12T06:07:55Z

Original (150 characters), verbatim:

> Changed the console input gate to hold and release whole submitted lines; rootglass-tui tests 58/58, Clippy and rustfmt pass; installed smoke pending.

Current journal summary (115 characters):

> Input gate holds full submitted lines; 58 TUI tests, Clippy and formatting passed; Windows installed smoke pending.
