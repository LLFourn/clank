"""Installed, interactive Claude question protocol acceptance; fixture accounts only.

Invoked by the ignored claude_question_protocol Rust test. A real PTY (no -p),
isolated profile, fake key and localhost model prove native hook/terminal races.
No Clank production hook is installed or enabled by this fixture.
Broker mode forwards native payloads through the actual Clank command and
lets the Rust controller submit through the production registry.
"""
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

QUESTION = {
    "questions": [{"question": "Choose the fixture direction", "header": "Direction",
                   "options": [{"label": "North", "description": "First"},
                               {"label": "South", "description": "Second"}],
                   "multiSelect": False}]
}
TOOL_ID = "toolu_clank_question_proof"
HOOK = r'''
import json, os, pathlib, sys
root=pathlib.Path(sys.argv[1])
value=json.load(sys.stdin)
event=value['hook_event_name']
(root/(event+'.json')).write_text(json.dumps(value))
(root/(event+'.transcript')).write_bytes(pathlib.Path(value['transcript_path']).read_bytes())
if event=='PermissionRequest':
    # Self-held FIFO avoids blocking before publication. This is a local
    # fixture response channel, not the proposed production hook transport.
    fd=os.open(root/'answer.fifo',os.O_RDWR)
    (root/'hook-ready').write_text(str(os.getpid()))
    answer=os.read(fd,65536).decode()
    os.close(fd)
    result={'hookSpecificOutput': {'hookEventName':event,'decision': {
        'behavior':'allow','updatedInput': {**value['tool_input'],
            'answers': {'Choose the fixture direction':answer}}}}}
    (root/'hook-returned.json').write_text(json.dumps(result))
    print(json.dumps(result))
'''


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        # Startup must not depend on an external service.
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.requests.append(body)
        results = tool_results(body.get("messages", []))
        if results:
            content = [{"type": "text", "text": "Fixture answer received."}]
            reason = "end_turn"
        else:
            content = [{"type": "tool_use", "id": TOOL_ID,
                        "name": "AskUserQuestion", "input": QUESTION}]
            reason = "tool_use"
        message = {"id": "msg_fixture", "type": "message", "role": "assistant",
                   "model": body.get("model", "claude-sonnet-4-6"), "content": content,
                   "stop_reason": reason, "stop_sequence": None,
                   "usage": {"input_tokens": 10, "output_tokens": 10}}
        self.send_response(200)
        if not body.get("stream"):
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(message).encode())
            return
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        events = [("message_start", {"type": "message_start", "message": {
            **message, "content": [], "stop_reason": None,
            "usage": {"input_tokens": 10, "output_tokens": 0}}})]
        for index, block in enumerate(content):
            tool = block["type"] == "tool_use"
            events.append(("content_block_start", {"type": "content_block_start", "index": index,
                "content_block": {**block, ("input" if tool else "text"): ({} if tool else "")}}))
            delta = ({"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
                     if tool else {"type": "text_delta", "text": block["text"]})
            events.extend([
                ("content_block_delta", {"type": "content_block_delta", "index": index, "delta": delta}),
                ("content_block_stop", {"type": "content_block_stop", "index": index})])
        events.extend([
            ("message_delta", {"type": "message_delta", "delta": {
                "stop_reason": reason, "stop_sequence": None}, "usage": {"output_tokens": 10}}),
            ("message_stop", {"type": "message_stop"})])
        try:
            for event, value in events:
                self.wfile.write(("event: " + event + "\ndata: " + json.dumps(value) + "\n\n").encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # The owning fixture may interrupt the pending turn.


def tool_results(messages):
    return [block for message in messages
            for block in (message.get("content", []) if isinstance(message.get("content"), list) else [])
            if block.get("type") == "tool_result" and block.get("tool_use_id") == TOOL_ID]


def published(transcript, prompt_id):
    """What of this question the native record holds: its prompt, its call."""
    seen = {"prompt": False, "tool_use": False}
    for line in transcript.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        seen["prompt"] |= record.get("promptId") == prompt_id
        content = record.get("message", {}).get("content")
        seen["tool_use"] |= isinstance(content, list) and any(
            block.get("type") == "tool_use" and block.get("id") == TOOL_ID for block in content)
    return seen


def recorded_results(profile):
    results = []
    for path in (profile / "projects").glob("*/*.jsonl"):
        for line in path.read_text().splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue  # Writer may be midway through its final line.
            results.extend(tool_results([record.get("message", {})]))
    return results

BROKER_HOOK = r'''
import json, pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
raw = sys.stdin.read()
value = json.loads(raw)
(root / (value['hook_event_name'] + '.json')).write_text(json.dumps(value))
(root / (value['hook_event_name'] + '.transcript')).write_bytes(pathlib.Path(value['transcript_path']).read_bytes())
if value['hook_event_name'] == 'PermissionRequest':
    (root / 'hook-ready').write_text('clank')
result = subprocess.run([sys.argv[2], 'question-hook'], input=raw, text=True, capture_output=True)
if result.returncode:
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)
sys.stdout.write(result.stdout)
'''


def run(binary, scenario, broker=None):
    with tempfile.TemporaryDirectory(prefix="clank-claude-question-") as directory:
        root = Path(directory).resolve()
        home, repo = root / "home", root / "repo"
        profile = home / ".claude"
        profile.mkdir(parents=True)
        if broker:
            repo = Path(broker['repo']).resolve()
        else:
            repo.mkdir()
        hook = root / "hook.py"
        hook.write_text(BROKER_HOOK if broker else HOOK)
        if not broker:
            os.mkfifo(root / "answer.fifo")
        command = f"python3 {shlex.quote(str(hook))} {shlex.quote(str(root))}"
        if broker:
            command += ' ' + shlex.quote(broker['clank'])
        settings = {"hooks": {event: [{"matcher": "AskUserQuestion", "hooks": [
            {"type": "command", "command": command, "timeout": 120}]}]
            for event in ("PreToolUse", "PermissionRequest", "PostToolUse", "PostToolUseFailure")}}
        (profile / "settings.json").write_text(json.dumps(settings))
        (profile / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True, "lastOnboardingVersion": "2.1.280", "theme": "dark",
            "projects": {str(repo): {"hasTrustDialogAccepted": True,
                                    "hasCompletedProjectOnboarding": True}}}))
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
        server.requests, server.lock = [], threading.Lock()
        server.daemon_threads = True
        model_thread = threading.Thread(target=server.serve_forever, daemon=True)
        model_thread.start()
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("ANTHROPIC_", "CLAUDE_", "CLANK_", "CODEX_"))}
        env.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(profile),
                   ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}",
                   ANTHROPIC_API_KEY="fixture-local-only",
                   CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
                   TERM="xterm-256color")
        child = None
        terminal = b""
        try:
            extra = ['--session-id', broker['session']] if broker else []
            child = subprocess.Popen([
                binary, *extra, "--model", "claude-sonnet-4-6", "--tools", "AskUserQuestion",
                "--settings", str(profile / "settings.json"),
                "--debug-file", str(root / "debug.log"), "Ask the fixture question."],
                cwd=repo, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
            slave = None
            deadline = time.monotonic() + 30
            trusted = key_accepted = sent = late = verified = False
            while time.monotonic() < deadline:
                if select.select([master], [], [], .05)[0]:
                    try:
                        terminal += os.read(master, 65536)
                    except OSError:
                        break
                screen = re.sub(r"\x1b\[[0-9;?<>]*[a-zA-Z]", " ", terminal.decode(errors="replace"))
                if not trusted and "Yes, I trust this folder" in screen:
                    os.write(master, b"\x1b[B\r")
                    trusted = True
                if not key_accepted and "Do you want to use this API key?" in screen:
                    os.write(master, b"\x1b[A\r")
                    key_accepted = True
                if not sent and (root / "hook-ready").exists() and "Enter to select" in screen:
                    pre = json.loads((root / "PreToolUse.json").read_text())
                    permission = json.loads((root / "PermissionRequest.json").read_text())
                    assert pre["tool_use_id"] == TOOL_ID
                    assert pre["session_id"] == permission["session_id"]
                    assert pre["prompt_id"] == permission["prompt_id"]
                    assert pre["tool_input"] == permission["tool_input"] == QUESTION
                    assert "tool_use_id" not in permission, "Do not invent a PermissionRequest tool id"
                    # Both hooks run before the native record publishes the
                    # prompt or the call (2.1.280 holds neither then); the
                    # call is published while the question still waits, so
                    # a viewer's transcript card needs no other source.
                    at_hook = {event: published((root / (event + ".transcript")).read_text(), pre["prompt_id"])
                               for event in ("PreToolUse", "PermissionRequest")}
                    print(f"publication at hook: {at_hook}", file=sys.stderr, flush=True)
                    transcript = Path(pre["transcript_path"])
                    waited = time.monotonic()
                    while not published(transcript.read_text(), pre["prompt_id"])["tool_use"]:
                        assert time.monotonic() - waited < 10, "The pending call was never published"
                        time.sleep(.02)
                    assert not recorded_results(profile), "Published only once answered"
                    # The interactive local dialog is visible while PermissionRequest blocks.
                    if broker:
                        print(json.dumps({'ready': True, 'input': pre}), flush=True)
                        assert sys.stdin.readline().strip() == 'continue'
                        if scenario != 'remote':
                            os.write(master, b"\r" if scenario == 'local' else b"\x1b")
                    elif scenario == "remote":
                        write_answer(root, "South")
                    else:
                        os.write(master, b"\r" if scenario == "local" else b"\x1b")
                    sent = True
                results = recorded_results(profile) if sent else []
                if not results:
                    continue
                assert len(results) == 1, "There must be exactly one native tool result"
                result = results[0]
                if scenario == "cancel":
                    assert result.get("is_error") and "rejected" in result["content"].lower()
                    if broker:
                        assert child.poll() is None
                        verified = True
                        break
                    # Escape must cancel the parked hook while the CLI stays alive.
                    try:
                        fd = os.open(root / "answer.fifo", os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno == errno.ENXIO:
                            assert child.poll() is None
                            verified = True
                            break
                        raise
                    else:
                        os.close(fd)
                        continue
                expected = "South" if scenario == "remote" else "North"
                assert f'="{expected}"' in result["content"]
                post_file = root / "PostToolUse.json"
                if not post_file.exists():
                    continue
                post = json.loads(post_file.read_text())
                assert post["tool_use_id"] == TOOL_ID
                assert post["session_id"] == pre["session_id"]
                assert post["tool_input"]["questions"] == QUESTION["questions"]
                assert post["tool_response"]["answers"]["Choose the fixture direction"] == expected
                if not broker and scenario == "local" and not late:
                    # Some releases keep PermissionRequest running after a local answer.
                    # If still alive, deliver an opposite late answer; it must not replace North.
                    try:
                        write_answer(root, "South")
                    except OSError as error:
                        assert error.errno == errno.ENXIO
                    late = True
                    continue
                if not broker and scenario == "local" and not (root / "hook-returned.json").exists():
                    # Cancellation is also valid cleanup; a remaining reader would be a failure.
                    try:
                        fd = os.open(root / "answer.fifo", os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        assert error.errno == errno.ENXIO
                    else:
                        os.close(fd)
                        continue
                if not broker and scenario == "local" and (root / "hook-returned.json").exists():
                    # Wait until the CLI has processed the late response, rather
                    # than stop immediately after our hook wrote its stdout.
                    debug = (root / "debug.log").read_text()
                    if "returned permissionDecision: allow" not in debug and "cancelled" not in debug:
                        continue
                with server.lock:
                    consumed = [result for request in server.requests
                                for result in tool_results(request.get("messages", []))]
                if not consumed:
                    continue
                assert all(f'="{expected}"' in item["content"] for item in consumed)
                assert child.poll() is None
                verified = True
                break
            else:
                raise AssertionError("Timed out waiting for native question arbitration")
            assert verified and sent and recorded_results(profile), "Question arbitration was not completed"
            print(f"PASS: interactive Claude {scenario}; exact native identity, local dialog and one result",
                  flush=True)
            if broker:
                # The controller verifies receipt/EOF before fixture teardown
                # can close the native process and manufacture an EOF.
                assert sys.stdin.readline().strip() == 'finish'
        except BaseException:
            print(terminal[-6000:].decode(errors="replace"), file=sys.stderr)
            raise
        finally:
            if child is not None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
            if slave is not None:
                os.close(slave)
            os.close(master)
            server.shutdown()
            server.server_close()
            model_thread.join()


def write_answer(root, answer):
    fd = os.open(root / "answer.fifo", os.O_WRONLY | os.O_NONBLOCK)
    try:
        os.write(fd, answer.encode())
    finally:
        os.close(fd)


if __name__ == "__main__":
    def terminate(_signal, _frame):
        raise SystemExit(1)  # Run fixture-owned child/server cleanup on test failure.
    signal.signal(signal.SIGTERM, terminate)
    if len(sys.argv) > 2 and sys.argv[2] == '--broker':
        run(sys.argv[1], sys.argv[3], {
            'repo': sys.argv[4], 'clank': sys.argv[5], 'session': sys.argv[6]})
    else:
        for mode in ("remote", "local", "cancel"):
            run(sys.argv[1], mode)
