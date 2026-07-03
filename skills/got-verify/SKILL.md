---
name: got-verify
description: Produce runtime evidence from a got machine that an implemented change actually works — tests, endpoints, database state, service behavior — before claiming completion. Use after implementing any feature or fix in a project that uses got, or when the user asks to verify, prove, or show evidence that something works.
---

# Verify with evidence from the machine

Claims of "done" must be backed by observations from the running
environment, not by reading the code. The machine is a real Linux VM with
the project's actual runtime — use it.

## Principle: best effort, honestly reported

Verify with what is available. If a config, secret, external service, or
test suite is missing, verify everything that can be verified without it
and **state explicitly what could not be checked and why**. Never skip
verification because it can only be partial; never present partial
verification as complete.

## What counts as evidence

In descending order of strength — collect the strongest available:

1. **Test suite** run inside the machine, with exit code:
   `got run <m> -- 'cd /srv/app && npm test'` (exit code propagates to
   the caller — check it, don't just read the output).
2. **Behavioral probe** of the running service: start it in the machine,
   hit it from the host via the mapped port (`got ls` shows
   `host->guest`), assert on the response:
   `curl -s --max-time 5 http://127.0.0.1:<host-port>/<endpoint>`
3. **State inspection** inside the machine: query the database, list
   files, check process state:
   `got run <m> -- 'psql -c "SELECT ..."'`
4. **Negative check**: demonstrate the old bug is gone (re-run the
   reproducing command) or that error paths behave (bad input rejected).

A compile or lint pass alone is not evidence that the requested thing
works.

## Workflow

1. Identify the claim to verify ("endpoint X returns Y", "migration adds
   column Z", "the crash no longer happens").
2. For each claim, run the strongest available check from the list above
   inside (or against) the machine for the current branch.
3. If a check needs setup that exists in the repo (seed script, fixture,
   `.env.example`), use it. If it needs something unavailable, fall back
   one level and note the gap.
4. After evidence passes, snapshot the verified state so it is
   reproducible at this commit:

```bash
got save <machine>
```

## Reporting format

End the task report with an evidence block — one line per claim:

```
Evidence (machine: feat/x, commit: a1b2c3d)
- npm test: exit 0, 42 passed          [test suite]
- GET /api/billing -> 200, totals match seed data   [behavioral probe]
- schema: invoices.due_date column present          [state inspection]
- not verified: Stripe webhook path (no API key available)
```

Every "not verified" line must say what is missing. If any check fails,
the task is not done — fix and re-verify rather than reporting the
failure as a caveat.
