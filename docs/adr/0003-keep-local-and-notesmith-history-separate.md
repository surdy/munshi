# Keep local and Notesmith history separate

When Git history is enabled, Munshi's archive repository and the Notesmith vault maintain separate
histories correlated by stable session and summary revision identifiers. Munshi does not push its
archive repository into Notesmith or share a writable repository with it. If Notesmith delivery is
enabled but cannot preserve correlated revision history, delivery is blocked rather than silently
degrading to latest-only storage.
