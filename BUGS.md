# zccache-fp Bugs

## 1. [v1.1.8] Hash cache `mark-success` cannot find pending file

**Severity:** High — hash cache is completely broken in 1.1.8

**Repro:**
```bash
echo "hello" > /tmp/test.txt
zccache-fp --cache-file /tmp/h.json --cache-type hash check --root /tmp --include "*.txt"
# creates h.pending (not h.json.pending)
zccache-fp --cache-file /tmp/h.json mark-success
# ERROR: "no pending data for /tmp/h.json: run `check` before `mark-success`/`mark-failure`"
```

**Root cause:** `check` writes the pending file as `<stem>.pending` (e.g., `h.pending`) but `mark-success` looks for `<stem>.json.pending` (e.g., `h.json.pending`). The two-layer cache type is not affected — only the hash cache type.

**Impact:** Any workflow using `--cache-type hash` is broken: check succeeds, but mark-success always fails with exit 2.

## 2. [v1.1.7, fixed in 1.1.8] Hash cache `mark-success` wrote `"status": "pending"` instead of `"success"`

Fixed in 1.1.8 (mark-success now errors instead, see bug #1).

## 3. [v1.1.7, fixed in 1.1.8] `mark-success` without prior `check` was a silent no-op

Fixed in 1.1.8 — now correctly returns exit 2 with error message.
