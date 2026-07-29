#!/bin/sh
# Phase-aware fake summarizer for the issue #48 chunked map-reduce contract (v2).
#
# Invocation: phase-aware.sh <log-file>   (the log path is passed via --summarizer-arg).
#
# Validates every request against the v2 envelope — `contract_version` 2, a `phase` matching the
# MUNSHI_SUMMARIZER_PHASE environment variable, and the per-phase field contract — then appends
# one log line per invocation:
#
#   complete <events>
#   chunk <index> <count> <events> <first-marker> <last-marker> <prev>
#   reduce <chunk_summaries> <events>
#
# where markers are `event-NNNN` sequence tags embedded in fixture event content (for chunk
# boundary coverage checks) and <prev> is 1 when chunk.previous_chunk_summary is present.
#
# Failure injection (controlled by sibling files of the log):
#   <log>.fail-all            every invocation exits 9 without responding
#   <log>.fail-chunk-<index>  the chunk invocation with that 1-based index exits 9
# Chunk-response padding (to force reduce recursion in tests):
#   <log>.pad                 contains a byte count of `x` padding appended to each chunk goal
set -eu
log="$1"
# The Python program arrives on stdin via the heredoc, so the request is spooled to a file first.
request_file="${TMPDIR:-/tmp}/phase-aware-request.$$"
cat > "$request_file"
status=0
python3 - "$log" "$request_file" <<'PYTHON' || status=$?
import json, os, re, sys

log = sys.argv[1]
with open(sys.argv[2]) as handle:
    request = json.load(handle)

def fail(message):
    sys.stderr.write("phase-aware fake: %s\n" % message)
    sys.exit(3)

phase_env = os.environ.get("MUNSHI_SUMMARIZER_PHASE")
phase = request.get("phase")
if request.get("contract_version") != 2:
    fail("contract_version must be 2, got %r" % request.get("contract_version"))
if phase not in ("complete", "chunk", "reduce"):
    fail("unknown phase %r" % phase)
if phase_env != phase:
    fail("MUNSHI_SUMMARIZER_PHASE %r does not match request phase %r" % (phase_env, phase))
for field in ("instruction", "required_schema", "session", "events"):
    if field not in request:
        fail("request is missing %r" % field)

if os.path.exists(log + ".fail-all"):
    sys.exit(9)

def markers(events):
    found = []
    for event in events:
        found.extend(int(m) for m in re.findall(r"event-(\d+)", event["content"]))
    return found

events = request["events"]
summaries = request.get("chunk_summaries")
chunk = request.get("chunk")

def summary(title, goal, work, tags):
    return {
        "title": title,
        "goal": goal,
        "work_completed": work or ["none"],
        "decisions": ["none"],
        "files_changed": ["none"],
        "commands_and_validation": ["none"],
        "open_items": ["none"],
        "tags": tags,
    }

if phase == "complete":
    if chunk is not None or summaries is not None:
        fail("complete requests must not carry chunk fields")
    with open(log, "a") as handle:
        handle.write("complete %d\n" % len(events))
    result = summary(
        "Complete one-shot summary",
        "Summarize one below-threshold session in a single invocation.",
        ["Summarized %d events one-shot." % len(events)],
        ["one-shot"],
    )
elif phase == "chunk":
    if summaries is not None:
        fail("chunk requests must not carry chunk_summaries")
    if not isinstance(chunk, dict):
        fail("chunk requests must carry a chunk object")
    index, count = chunk.get("index"), chunk.get("count")
    if not (isinstance(index, int) and isinstance(count, int) and 1 <= index <= count):
        fail("chunk index/count invalid: %r/%r" % (index, count))
    previous = chunk.get("previous_chunk_summary")
    if (previous is not None) != (index > 1):
        fail("previous_chunk_summary must be present exactly when index > 1")
    if previous is not None and "Segment %d of %d" % (index - 1, count) != previous["title"]:
        fail("previous_chunk_summary is not the preceding segment's summary")
    if not events:
        fail("chunk requests must carry this segment's events")
    seen = markers(events)
    if seen != sorted(seen) or (seen and seen != list(range(seen[0], seen[-1] + 1))):
        fail("segment events out of order or with gaps: %r" % seen)
    if os.path.exists("%s.fail-chunk-%d" % (log, index)):
        with open(log, "a") as handle:
            handle.write("chunk-failed %d %d\n" % (index, count))
        sys.exit(9)
    with open(log, "a") as handle:
        handle.write(
            "chunk %d %d %d %s %s %d\n"
            % (
                index,
                count,
                len(events),
                seen[0] if seen else -1,
                seen[-1] if seen else -1,
                1 if previous is not None else 0,
            )
        )
    padding = 0
    if os.path.exists(log + ".pad"):
        padding = int(open(log + ".pad").read().strip())
    result = summary(
        "Segment %d of %d" % (index, count),
        "Segment summary." + "x" * padding,
        ["Summarized %d events in segment %d." % (len(events), index)],
        ["segment-%d" % index],
    )
else:
    if chunk is not None:
        fail("reduce requests must not carry a chunk object")
    if events:
        fail("reduce requests must not quote raw events")
    if not isinstance(summaries, list) or not summaries:
        fail("reduce requests must carry chunk_summaries")
    with open(log, "a") as handle:
        handle.write("reduce %d %d\n" % (len(summaries), len(events)))
    result = summary(
        "Reduced %d segment summaries" % len(summaries),
        "Synthesize per-segment summaries into one session summary.",
        ["Merged: %s" % "; ".join(s["title"] for s in summaries)],
        ["reduced"],
    )

sys.stdout.write(json.dumps(result))
PYTHON
rm -f "$request_file"
exit $status
