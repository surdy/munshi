# Make Markdown the durable record

Munshi treats the current Markdown summary as the durable archive record and SQLite as rebuildable
operational state for cursors, status, and delivery attempts. By default only the current summary
text is retained; optional Git history in a dedicated archive repository preserves earlier
revisions. This keeps archives useful without Munshi while avoiding a second revision store in
SQLite.
