#!/usr/bin/env python3
"""A scripted stand-in for `codex app-server` used in tests.

Speaks the app-server framing: one JSON object per line on stdin/stdout,
no Content-Length headers and no "jsonrpc" field.

Usage: codex_app_server.py SCENARIO.json [ignored args...]

The scenario file is a JSON object with a "steps" list. Steps run in order:

  {"expect": "thread/start", "as": "start", "match": {"cwd": "/work/demo"}}
      Read the next client message and check its method. "as" names it for a
      later "respond" (default: the method). "match" is optional and must be a
      subset of the message's params (objects compare key by key, everything
      else by equality). Client notifications such as "initialized" are
      expected the same way.

  {"respond": "start", "result": {...}}
  {"respond": "start", "error": {"code": -32600, "message": "bad"}}
      Answer an earlier expected request by its name, using its id.

  {"send": {...}}
      Write the object as one line, as is. Use it for notifications.

  {"request": "item/commandExecution/requestApproval", "id": 0,
   "params": {...}, "reply": {"decision": "accept"}}
      Send a server request, then read the next client message, which must be
      a reply with the same id. "reply" is optional; when given, the reply's
      result must equal it.

  {"sleep": 0.2}
      Pause for that many seconds.

  {"exit": 0}
      Exit at once with that code, leaving anything unanswered.

After the last step the fake reads stdin until it closes, then exits 0.
Any mismatch prints a line to stderr and exits with code 3.
"""

import json
import sys
import time


def fail(msg):
    sys.stderr.write("fake codex: " + msg + "\n")
    sys.stderr.flush()
    sys.exit(3)


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def read_message():
    line = sys.stdin.readline()
    if not line:
        fail("stdin closed while waiting for a message")
    try:
        return json.loads(line)
    except ValueError:
        fail("not JSON: " + line.strip())


def is_subset(want, got):
    if isinstance(want, dict):
        if not isinstance(got, dict):
            return False
        return all(k in got and is_subset(v, got[k]) for k, v in want.items())
    return want == got


def main():
    if len(sys.argv) < 2:
        fail("usage: codex_app_server.py SCENARIO.json")
    with open(sys.argv[1]) as f:
        scenario = json.load(f)

    seen = {}
    for step in scenario.get("steps", []):
        if "expect" in step:
            msg = read_message()
            method = step["expect"]
            if msg.get("method") != method:
                fail("expected %s, got %s" % (method, json.dumps(msg)))
            if "jsonrpc" in msg:
                fail("unexpected jsonrpc field: " + json.dumps(msg))
            want = step.get("match")
            if want is not None and not is_subset(want, msg.get("params")):
                fail("params of %s do not match: %s" % (method, json.dumps(msg)))
            seen[step.get("as", method)] = msg
        elif "respond" in step:
            name = step["respond"]
            if name not in seen or "id" not in seen[name]:
                fail("no request named " + name)
            reply = {"id": seen[name]["id"]}
            if "error" in step:
                reply["error"] = step["error"]
            else:
                reply["result"] = step.get("result", {})
            send(reply)
        elif "send" in step:
            send(step["send"])
        elif "request" in step:
            rid = step["id"]
            send({"method": step["request"], "id": rid, "params": step.get("params", {})})
            msg = read_message()
            if "method" in msg or msg.get("id") != rid:
                fail("expected a reply to request %s, got %s" % (rid, json.dumps(msg)))
            if "result" not in msg:
                fail("reply to %s has no result: %s" % (rid, json.dumps(msg)))
            if "reply" in step and msg["result"] != step["reply"]:
                fail("reply to %s was %s" % (rid, json.dumps(msg["result"])))
        elif "sleep" in step:
            time.sleep(step["sleep"])
        elif "exit" in step:
            sys.exit(step["exit"])
        else:
            fail("unknown step: " + json.dumps(step))

    while sys.stdin.readline():
        pass


if __name__ == "__main__":
    main()
