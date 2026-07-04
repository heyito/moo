---
name: moo-verify
description: Produce real, human-checkable evidence from a moo machine that an implemented change actually works — videos for UI, raw API request/responses for backends, queries with outputs and plans for databases — before claiming completion. Use after implementing any feature or fix in a project that uses moo, or when the user asks to verify, prove, or show evidence that something works.
---

# Verify with evidence a human can check

Claims of "done" must be backed by observations of the running system, not
by reading the code. The machine is a real Linux VM with the project's
actual runtime, reachable from the host via its mapped ports (`moo ls`).
The deliverable of verification is an **evidence pack**: raw artifacts a
human can open and judge in seconds without re-running anything.

Two absolutes:

- **Never verify against production.** Evidence comes from the moo machine
  (or another local/staging target) only. Never use production
  credentials, databases, or live customer data. If a URL, cookie, or
  connection string looks production-like, stop and record the claim as
  blocked instead.
- **Narration is not execution.** A sentence describing what an artifact
  would show is not evidence; only output from commands actually run
  counts. If no command was run, the claim is unverified — say so.

## The evidence hierarchy — match the artifact to the change

Produce the artifact whose *form* matches what changed. A human verifying
a UI change wants to see it; a human verifying an API wants the bytes on
the wire; a human verifying a migration wants the data.

| What changed | Required evidence |
|---|---|
| Frontend / UI | **Video** of the real interaction (screen recording while driving the app), or a stepwise screenshot sequence when video is unavailable. Capture the moment the feature works, not a static end state. |
| Backend / API | **Exact request + raw response**: the curl/fetch command verbatim, full status line, relevant headers, and unedited body. Cover the happy path AND at least one rejection/edge path. |
| Database / migration | **Queries, outputs, and plans**: the SQL run verbatim, its raw output, and `EXPLAIN (ANALYZE)` where performance or index use is part of the claim. For migrations: schema/state readback before and after. |
| Background job / service | Trigger command, the service's **log lines** for the handled event, and a **state readback** proving the effect persisted. |
| Race / concurrency fix | The concurrent trigger commands, **both raw responses**, and a post-race state read showing the invariant held. |
| Bug fix | **Negative check**: the exact reproduction command from the bug report, run against the fix, showing the old behavior is gone. |
| Anything | Test suite run inside the machine with exit code — necessary but not sufficient. A compile, lint, or unit-test pass alone is not evidence the requested thing works. |

## Where evidence lives

One directory per claim, next to the work, never committed:

```bash
mkdir -p .evidence/<machine>/<claim-id>
grep -qxF '/.evidence/' .git/info/exclude || echo '/.evidence/' >> .git/info/exclude
```

(`.git/info/exclude` keeps it out of git without touching the project's
.gitignore; being ignored also keeps it out of the machine's synced tree.)

Name files by what they prove, predictably:

```
.evidence/feat-x/CLAIM-1/
├── repro.sh              # exact commands to reproduce the check
├── api-response.txt      # raw request + response transcript
├── db-query.txt          # SQL, raw output, EXPLAIN ANALYZE
├── state-readback.txt    # persisted-state proof after the action
├── service.log           # relevant log excerpt from the machine
├── recording.mp4         # UI interaction video
└── screenshot-*.png      # stepwise UI captures
```

## Evidence pack format

Packs must read like a console session — a human replays them with their
eyes. `#` comments for observations; everything else is exact commands and
unedited output:

```
# CLAIM: POST /webhooks/github with ready_for_review triggers exactly one run
# machine: feat/1761, commit a1b2c3d, host port 25911 -> guest 3001

$ curl -si -X POST http://127.0.0.1:25911/webhooks/github \
    -H 'Content-Type: application/json' -d @ready_for_review.json
HTTP/1.1 202 Accepted
{"queued":true,"runId":512}

# state readback — one pending run on the head SHA, not two:
$ moo run feat/1761 -- "su postgres -c \"psql -d app -c \\\"SELECT id,status,head_sha FROM pipeline_runs WHERE pull_request_id=7\\\"\""
 id  | status  | head_sha
 512 | pending | a1b2c3d...
(1 row)
```

Rules, in the spirit of a QA agent's raw captures:

- Save the **exact command and the raw output** — never paraphrase,
  truncate the decisive lines, or reconstruct from memory.
- When asserting persisted state, save the raw readback (query output or
  API re-fetch), not a claim that it looked right.
- Capture UI evidence **at the moment the result is on screen** — a video
  or screenshot taken after the fact of a different state proves nothing.
- Do not paste application source code into packs; evidence is behavior.
- Any scaffolding that weakens realism (auth bypass, mocked service,
  seeded data, feature flag forced on) must be stated in the pack header —
  never present a bypassed run as pure end-to-end.

## Workflow

1. Turn the change into claims ("endpoint X returns Y", "column Z exists
   and is indexed", "clicking A shows B"). One evidence pack per claim.
2. Get the runtime serving: start the app/services inside the machine
   (`moo run <m> -- 'cd /srv/app && nohup <serve> &'`), find the host
   port with `moo ls`.
3. For each claim, capture the matching artifact from the table:
   - UI: drive the app through a real browser against
     `http://127.0.0.1:<host-port>` and record it (screen recording,
     browser tooling, or `playwright ... --video`). Screenshots at each
     meaningful step if video is impossible.
   - API: `curl -si` (or the project's client) from the host, saved
     verbatim with the response.
   - DB: `moo run <m> -- 'psql ... -c "<query>"'` for output and
     `EXPLAIN (ANALYZE, BUFFERS)` for plans.
   - Tests: `moo run <m> -- 'cd /srv/app && <test cmd>'` — record the
     exit code, not just the tail of the output.
4. Run the negative/edge case, not only the happy path.
5. If a check needs setup that exists in the repo (seed script, fixture,
   `.env.example`), use it. If something is genuinely unavailable (a
   secret, an external service), fall back one level — mock it, disclose
   the mock, and record what could not be checked.
6. After evidence passes, snapshot the verified runtime so the evidence
   is reproducible at this exact commit:

```bash
moo save <machine>
```

## Blocked is a result too — with proof

If a claim cannot be verified after genuinely different strategies, report
it blocked with the attempts, not silently skipped:

```
- BLOCKED: webhook signature path — attempt 1: replayed recorded payload
  (rejected, no secret); attempt 2: generated test secret (app requires
  GitHub App key). Needs: GITHUB_APP_PRIVATE_KEY.
```

Never present partial verification as complete; never skip verification
because it can only be partial.

## Reporting format

End the task report with an evidence block — one line per claim, each
pointing at its artifact:

```
Evidence (machine: feat/x, commit: a1b2c3d, snapshot: s_ab12cd34)
- one-shot draft run queued via @itoqa   [api-response.txt: 202 + runId]
- no duplicate on redelivery             [state-readback.txt: 1 row]
- runs list renders new badge            [recording.mp4 0:12–0:19]
- suite: 1569 pass, 8 pre-existing fail  [test run, exit 0]
- not verified: Slack notification (no webhook URL available)
```

Every artifact path must exist; every "not verified" line must say what is
missing. If any check fails, the task is not done — fix and re-verify
rather than reporting the failure as a caveat.
