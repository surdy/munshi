# Identify logical sessions by source ID

Munshi identifies a logical session by the source harness and its stable session ID, not by
repository, working directory, title, or timestamps. A session may span multiple repositories; the
first detected repository is retained only as its origin project for stable routing. This avoids
duplicates when sessions resume or move while preserving project-oriented organization.
