"""Deterministic provider transport, never a model or network client.

ACP advertises the inspected native schema and copies empty fixture history.
Interactive mode executes installed lifecycle hooks and ordinary background waits,
then models the provider's background-completion-to-turn callback explicitly.
The Rust driver controls delivery and acknowledgement with pipes.
"""
import json
import os
import pathlib
import select
import shlex
import subprocess
import sys
import urllib.parse
import uuid

root = pathlib.Path(os.environ["GROK_HOME"])
root.mkdir(parents=True, exist_ok=True)
args = sys.argv[1:]


def audit(value):
    with (root / "audit.jsonl").open("a") as out:
        out.write(json.dumps(value) + "\n")


def session_dir(cwd, sid):
    return root / "sessions" / urllib.parse.quote(cwd, safe="") / sid


def save_summary(cwd, sid, parent=None):
    folder = session_dir(cwd, sid)
    folder.mkdir(parents=True, exist_ok=True)
    summary = {"info": {"id": sid, "cwd": cwd}, "parent_session_id": parent}
    (folder / "summary.json").write_text(json.dumps(summary))


identity_names = ["CLANK_AGENT", "GROK_SESSION_ID", "GROK_AGENT", "CLAUDE_CODE_SESSION_ID",
                  "CODEX_THREAD_ID", "OPENCODE_SESSION_ID", "CLANK_BOOTSTRAP_SESSION_ID"]
audit({"kind": "start", "argv": args, "cwd": os.getcwd(), "pid": os.getpid(),
       "identity": {key: os.environ.get(key) for key in identity_names}})

if args[-3:] == ["agent", "--no-leader", "stdio"]:
    for line in sys.stdin:
        request = json.loads(line)
        audit({"kind": "rpc", "request": request})
        if request["method"] == "initialize":
            result = {"protocolVersion": 1, "_meta": {"agentVersion": "1.0.46"},
                      "agentCapabilities": {"_meta": {"x.ai/hooks": {
                          "blockingEvents": ["pre_tool_use", "stop"]}}}}
        else:
            assert request["method"] == "_x.ai/session/fork"
            params = request["params"]
            original = json.loads((session_dir(params["sourceCwd"], params["sourceSessionId"])
                                   / "summary.json").read_text())
            assert original["info"]["cwd"] == params["sourceCwd"]
            save_summary(params["newCwd"], params["newSessionId"], params["sourceSessionId"])
            result = {"newSessionId": params["newSessionId"], "newCwd": params["newCwd"],
                      "parentSessionId": params["sourceSessionId"]}
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    sys.exit(0)

assert args[0] == "--fixture-profile"
cwd = str(pathlib.Path(os.getcwd()).resolve())
assert args[args.index("--cwd") + 1] == cwd
sid = args[args.index("--resume") + 1] if "--resume" in args else (
    args[args.index("--session-id") + 1] if "--session-id" in args else str(uuid.uuid4()))
assert os.environ["CLANK_AGENT"] == "grok"
assert all(not os.environ.get(key) for key in identity_names if key not in
           ["CLANK_AGENT", "GROK_SESSION_ID"]), "foreign session identity leaked at exec"
assert os.environ.get("GROK_SESSION_ID") in [None, sid]
if "--resume" in args:
    assert json.loads((session_dir(cwd, sid) / "summary.json").read_text())["info"]["cwd"] == cwd
else:
    save_summary(cwd, sid)
os.environ["GROK_SESSION_ID"] = sid
os.environ["GROK_AGENT"] = "1"
hooks = json.loads((root / "hooks/clank.json").read_text())["hooks"]
workers = []


def emit(step, **values):
    print(json.dumps(dict(step=step, **values)), flush=True)


def receive(expected):
    message = json.loads(sys.stdin.readline())
    assert message["action"] == expected, message
    return message


def hook(event, turn="0", **values):
    native = {"SessionStart": "session_start", "Stop": "stop", "SessionEnd": "session_end"}[event]
    payload = dict(hookEventName=native, sessionId=sid, cwd=cwd, promptId=turn,
                   reason="end_turn", **values)
    env = dict(os.environ, GROK_HOOK_EVENT=native)
    entry = [entry for group in hooks[event] for entry in group["hooks"]
             if entry["command"] == "clank stop-hook --tool grok"]
    assert len(entry) == 1
    result = subprocess.run(shlex.split(entry[0]["command"]), input=json.dumps(payload),
                            text=True, capture_output=True, env=env, timeout=10)
    assert result.returncode == 0 and not result.stderr, result.stderr
    output = json.loads(result.stdout) if result.stdout.strip() else {}
    audit({"kind": "hook", "event": event, "turn": turn, "output": output})
    return output


try:
    hook("SessionStart")
    first = hook("Stop", backgroundTasks=[])
    assert first.get("decision") == "block", first
    emit("bound", session=sid, cwd=cwd, argv=args, stop=first)
    total = receive("begin")["cycles"]
    for cycle in range(total):
        turn = str(cycle)
        command = ["clank", "wait"]
        if cycle % 2 == 0:
            command.extend(["--repo", cwd])
        worker = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                  env=dict(os.environ, RUST_LOG="clank::cli::wait=debug"))
        workers.append(worker)
        emit("started", cycle=cycle, command=shlex.join(command), pid=worker.pid)
        stopped = hook("Stop", turn, stopHookActive=True, backgroundTasks=[{
            "id": "task-" + turn, "type": "shell", "status": "running",
            "command": shlex.join(command)}])
        assert stopped.get("decision") != "block", stopped
        ready = ""
        if cycle > 0:
            # The driver's next event waits for the existing monitor to accept
            # this subscriber. One readiness event, read over a pipe; no polling.
            assert select.select([worker.stderr, worker.stdout], [], [], 20)[0], "subscription timeout"
            ready = worker.stderr.readline()
            assert "monitor subscription registered" in ready, ready
        emit("waiting", cycle=cycle, stop=stopped, worker_running=worker.poll() is None)
        out, error = worker.communicate(timeout=20)
        error = ready + error
        assert worker.returncode == 0, error
        # The provider transports a completion into another turn. This fixture
        # reaction proves the wiring only; it is not an authenticated LM turn.
        emit("completed", cycle=cycle, output=out, error=error)
        ack = receive("react")
        handled = subprocess.run(["clank", "events", "ack", ack["selector"]],
                                 text=True, capture_output=True, timeout=10)
        assert handled.returncode == 0, handled.stderr
        emit("reacted", cycle=cycle)
    hook("SessionEnd", str(total))
    emit("ended", cycles=total)
finally:
    for worker in workers:
        if worker.poll() is None:
            worker.kill()
        worker.wait(timeout=5)
