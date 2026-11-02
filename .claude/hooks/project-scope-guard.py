#!/usr/bin/env python3
"""
PreToolUse guard: refuse Write/Edit outside this repository.

WHY THIS EXISTS. The Dev-loop runs one worker per project, several at once, on
an always-on machine with nobody watching. Each worker is TOLD which project it
owns — but that is an instruction in a prompt, not a boundary. Tested on
2026-07-27: a session whose cwd was policynest wrote a file into a sibling repo
with no objection at all. Nothing scopes Edit/Write to the project by default.

The realistic failure is not malice, it is confusion — a worker researching a
shared pattern, wandering into a sibling checkout, and editing there. The change
then appears in a repository nobody was building, which is a genuinely horrible
thing to debug.

WHY A HOOK rather than permission deny rules. Deny rules need absolute paths,
and this pipeline runs on two machines with different home directories
(/Users/coreybell and /Users/corey-dev). A hook resolves the repo root at
runtime via `git rev-parse --show-toplevel`, so the same committed file works
anywhere, including a machine that does not exist yet.

SCRATCH IS EXEMPT, and that exemption is load-bearing. `dev-build` instructs
agents to keep working notes out of the checkout and write them to
~/.claude/dev-loop/scratch/ instead, because a single stray untracked file makes
the next build's preflight refuse a dirty tree — that has already bricked this
project once. A guard that blocks the sanctioned scratch location pushes agents
straight back into the failure it was meant to prevent, so the two rules have to
agree. Temp directories are exempt for the same reason; neither is ever somebody
else's source checkout, which is the only thing this guard is protecting.

Fails OPEN on malformed input by design: this is a containment guard, not an
authentication boundary, and a crash here must never wedge a build.
"""
import json, sys, os, subprocess, tempfile

def contains(parent, child):
    return child == parent or child.startswith(parent + os.sep)

try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
f = (d.get("tool_input") or {}).get("file_path")
if not f:
    sys.exit(0)
try:
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True).stdout.strip() or os.getcwd()
except Exception:
    root = os.getcwd()
p = os.path.realpath(f)
root = os.path.realpath(root)

# Sanctioned write destinations outside the repo. Note ~/.claude/dev-loop/scratch
# specifically and NOT ~/.claude/dev-loop, which holds secrets.json.
allowed = [root]
for extra in (os.path.expanduser("~/.claude/dev-loop/scratch"),
              tempfile.gettempdir(), "/tmp", "/private/tmp"):
    try:
        allowed.append(os.path.realpath(extra))
    except Exception:
        pass

# Session memory lives under ~/.claude/projects/<slug>/memory/. Same reasoning
# as scratch: it is Claude's own data directory, never a source checkout, and
# blocking it breaks a system the assistant is told to use. Matched by suffix so
# it holds for any project slug on any machine.
def is_memory_dir(path):
    home = os.path.realpath(os.path.expanduser("~/.claude/projects"))
    return contains(home, path) and (os.sep + "memory" + os.sep) in path

# THE SKILLS DIRECTORY, and why it is gated rather than simply allowed.
#
# The loop's standing rule is that every failure becomes a skill change, so the
# supervised session a human is sitting in front of has to be able to edit
# ~/dev-loop/skills. But an UNATTENDED worker must never edit them: a skill
# change alters how every project builds, forever, with nobody reading the diff.
# Those two facts point opposite ways, and the guard cannot tell the two kinds
# of session apart — a worker is `claude --print` on the build machine, and
# nothing in the hook payload distinguishes that from a human's terminal.
#
# So the human declares it, per machine, by creating a marker file. Absent on
# the build machine, skill edits stay denied there even though the same hook
# file is checked out — which is the granularity that matters.
#
# HOW STRONG THIS IS, stated honestly. The hook matches Write|Edit|NotebookEdit
# only. Bash is NOT matched, so an agent can create this marker with `touch`, or
# for that matter bypass this entire guard with a shell redirect. That is a
# pre-existing hole in the guard as a whole, not one introduced here, and it is
# worth closing separately. What the marker therefore buys is protection against
# the threat this guard was actually written for — confusion, not malice — by
# making a skill edit a thing someone chose rather than a thing that just
# happened. Do not read it as a security boundary.
#
# Deliberately NOT used instead: a TTY check (Claude Code captures hook stdout,
# so it is not a TTY in either mode), and a hostname or home-directory check
# (both silently wrong the first time a worker runs on the human's laptop).
SKILL_MARKER = os.path.expanduser("~/.claude/dev-loop/ALLOW_SKILL_EDITS")

# The whole pipeline repo, not just its skills. The runner in ~/dev-loop/bin is
# the same kind of artifact as a skill — shared machinery that changes how every
# project builds — and it needs the same treatment: editable by a supervised
# session, never by an unattended worker. Scoping this to skills/ only forced a
# choice between leaving the runner unmaintainable from here and reaching for
# Bash to write it, which is the improvisation this guard exists to stop.
#
# NOTE the two different directories, which are easy to confuse: ~/dev-loop is
# the pipeline's source (allowed under the marker) while ~/.claude/dev-loop
# holds secrets.json and the registry (never allowed, by anyone).
def is_pipeline_edit(path):
    for d in ("~/dev-loop", "~/.claude/skills"):
        try:
            if contains(os.path.realpath(os.path.expanduser(d)), path):
                return True
        except Exception:
            pass
    return False

skill_edit = is_pipeline_edit(p)
if skill_edit and os.path.exists(SKILL_MARKER):
    allowed.append(p)

if not (any(contains(a, p) for a in allowed) or is_memory_dir(p)):
    if skill_edit:
        print(json.dumps({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": (
                "Blocked: " + p + " is Dev-loop pipeline code. Changes here alter how EVERY "
                "project builds, so they are denied unless a human has enabled them on "
                "this machine. If you are a human working interactively, run:\n\n"
                "    touch " + SKILL_MARKER + "\n\n"
                "and retry. If you are an unattended build worker, this is working as "
                "intended — report the skill change you wanted to make and stop."),
        }}))
        sys.exit(0)
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": (
            "Blocked: " + p + " is outside this project (" + root + "). "
            "Dev-loop workers are scoped to one repository so parallel builds "
            "cannot modify each other's code. Scratch files belong in "
            "~/.claude/dev-loop/scratch/, which is allowed."),
    }}))
