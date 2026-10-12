# Preserve file paths across repeated context compaction

Comma-separated file history corrupted paths containing commas, surrounding
whitespace, or line breaks when the next compaction parsed the summary.
A failing regression reproduced the loss before changing the implementation.

Keep ordinary paths in the existing readable format. Encode delimiter-sensitive
paths as a JSON array under explicit `Files read (JSON):` and
`Files modified (JSON):` labels. Continue reading the legacy comma-separated
format; already corrupted legacy summaries cannot recover their original paths.

An independent review identified that the summary's character limit could cut
an array in half. A second failing regression reproduced that problem. File
history now fits complete paths within the remaining summary budget, reserves
the closing bracket and omission notice, and marks omitted paths. Oversized
ordinary lists also use this bounded representation. History beyond the budget
remains lossy; the original session log remains the source of truth.

Regression coverage includes three consecutive compactions with exact read and
write paths (commas, whitespace, line breaks, quotes, backslashes and Unicode),
legacy summary compatibility, and oversized arrays across another compaction.
No model calls or network defaults are changed.
