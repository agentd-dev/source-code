# SPDX-License-Identifier: AGPL-3.0-only
"""INTEROP: the official Python SDK, as a CLIENT, against a real agentd.

agentd's own tests speak A2A the way agentd's authors read the spec. This
speaks it the way the SDK's authors did: a2a-sdk 1.1.5 (pinned in
requirements.txt) resolves the card, signs every call through its own
AuthInterceptor, and parses every answer into its protobuf types with
`json_format.ParseDict` — which refuses a field the proto does not define,
an enum spelled any other way, or a wrong shape. So "the SDK parsed it" is
the claim this file tests, for the whole method list:

- the card, through A2ACardResolver, declaring the bearer scheme the listener
  enforces (or the AuthInterceptor would send nothing) — and read again raw
  and parsed strictly, because the resolver itself ignores unknown fields;
- SendMessage, blocking and with returnImmediately; GetTask; ListTasks, and a
  listing that matches nothing, whose raw answer must still carry every field
  the proto marks REQUIRED (ParseDict does not check that);
- SubscribeToTask on a task that is still WORKING, to its end;
- CancelTask on a finished task, refused as TaskNotCancelableError (-32002);
- GetExtendedAgentCard;
- CreateTaskPushNotificationConfig with authentication, and the delivery
  reaching the receiver with that credential;
- a command op, sent the way any client activates an extension: the
  A2A-Extensions service parameter and the message marked with its URI.

Every call carries A2A-Version 1.0 and the bearer; the SDK sets both, and a
request hook records what went on the wire so that is asserted, not assumed.

    AGENTD_E2E_BIN=target/debug/agentd python interface/test/stock/sdk_client.py

The daemon must be built with `--features a2a,internal-mocks` (the mock LLM).
Without AGENTD_E2E_BIN it skips — except under CI, where it fails.
"""

import asyncio
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx

from google.api import field_behavior_pb2
from google.protobuf import json_format

from a2a.client import (
    A2ACardResolver,
    AuthInterceptor,
    ClientCallContext,
    ClientConfig,
    ClientFactory,
    CredentialService,
)
from a2a.client.service_parameters import ServiceParametersFactory, with_a2a_extensions
from a2a.helpers import get_artifact_text, new_data_part, new_message, new_text_message
from a2a.types import (
    AgentCard,
    AuthenticationInfo,
    CancelTaskRequest,
    GetExtendedAgentCardRequest,
    GetTaskRequest,
    ListTasksRequest,
    ListTasksResponse,
    Message,
    Role,
    SendMessageConfiguration,
    SendMessageRequest,
    StreamResponse,
    SubscribeToTaskRequest,
    Task,
    TaskPushNotificationConfig,
    TaskState,
)
from a2a.utils.errors import TaskNotCancelableError

# The command extension's URI: the one name agentd declares it under.
COMMAND_EXTENSION = 'https://agentd.dev/a2a/ext/command'
BEARER = 'sdk-interop-' + uuid.uuid4().hex
PUSH_CREDENTIAL = 'push-' + uuid.uuid4().hex
PUSH_TOKEN = 'sdk-notification-token'
TERMINAL = {
    TaskState.TASK_STATE_COMPLETED,
    TaskState.TASK_STATE_FAILED,
    TaskState.TASK_STATE_CANCELED,
    TaskState.TASK_STATE_REJECTED,
}


def free_port() -> int:
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class PushReceiver:
    """A webhook: records the headers and body of every POST it gets."""

    def __init__(self):
        self.deliveries: list[dict] = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802 — http.server's spelling
                body = self.rfile.read(int(self.headers.get('content-length', 0)))
                outer.deliveries.append({'headers': {k.lower(): v for k, v in self.headers.items()}, 'body': body})
                self.send_response(200)
                self.end_headers()

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}/hook'
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


class Bearer(CredentialService):
    """The one credential this client holds, offered for a bearer scheme."""

    def __init__(self, card: AgentCard):
        self.card = card

    async def get_credentials(self, security_scheme_name, context):
        scheme = self.card.security_schemes.get(security_scheme_name)
        if scheme is not None and scheme.HasField('http_auth_security_scheme'):
            return BEARER
        return None


def start_daemon(binary: str, workdir: str) -> tuple[subprocess.Popen, str, str]:
    """A v1.17 daemon: the bearer on its listener, the mock LLM, push on."""
    playbook = os.path.join(workdir, 'playbook.json')
    with open(playbook, 'w') as f:
        # A message saying SLOWPLEASE is answered late, so its task is still
        # WORKING when the client subscribes to it and registers a webhook.
        json.dump(
            {
                'turns': [{'content': 'ok'}],
                'match': [{'when_contains': 'SLOWPLEASE', 'delay_ms': 3000, 'content': 'late'}],
            },
            f,
        )
    port = free_port()
    config = os.path.join(workdir, 'agentd.yaml')
    with open(config, 'w') as f:
        f.write(
            '\n'.join(
                [
                    'agent:',
                    '  name: sdk-interop',
                    '  instruction: You are a test agent.',
                    '  preflight: never',
                    'intelligence:',
                    f'  endpoints: "mock:file:{playbook}"',
                    '  model: mock',
                    'store:',
                    '  kind: memory',
                    'a2a:',
                    f'  listen: http://127.0.0.1:{port}',
                    '  bearer: "{{secret:AGENTD_SDK_BEARER}}"',
                    '  push:',
                    '    enabled: true',
                    # The receiver is on loopback.
                    '    allow_private: true',
                    'lifecycle:',
                    '  run_until: drained',
                    '  drain_timeout: 2s',
                    '',
                ]
            )
        )
    log = open(os.path.join(workdir, 'daemon.log'), 'wb')
    daemon = subprocess.Popen(
        [binary, '--config', config],
        env={**os.environ, 'AGENTD_SDK_BEARER': BEARER},
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=log,
    )
    url = f'http://127.0.0.1:{port}'
    deadline = time.monotonic() + 20
    while True:
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=1):
                break
        except OSError:
            if daemon.poll() is not None or time.monotonic() > deadline:
                raise SystemExit(f'the daemon never listened:\n{daemon_log(workdir)}') from None
            time.sleep(0.05)
    return daemon, url, config


def daemon_log(workdir: str) -> str:
    with open(os.path.join(workdir, 'daemon.log'), errors='replace') as f:
        return f.read()


def expect(ok: bool, what: str) -> None:
    if not ok:
        raise AssertionError(what)


async def run(url: str) -> list[httpx.Request]:
    receiver = PushReceiver()
    sent: list[httpx.Request] = []

    async def record(request: httpx.Request) -> None:
        sent.append(request)

    async with httpx.AsyncClient(timeout=30, event_hooks={'request': [record]}) as http:
        # The card, the way any SDK client finds an agent.
        card = await A2ACardResolver(http, url).get_agent_card()
        expect(card.name == 'sdk-interop', f'the card: {card.name}')
        # A2ACardResolver parses with ignore_unknown_fields=True, so a
        # misspelled field on the public card would pass it. Parsed strictly
        # here, the way every JSON-RPC answer below is.
        raw_card = await http.get(f'{url}/.well-known/agent-card.json')
        json_format.ParseDict(raw_card.json(), AgentCard())
        bearer_schemes = [
            name
            for name, s in card.security_schemes.items()
            if s.HasField('http_auth_security_scheme') and s.http_auth_security_scheme.scheme.lower() == 'bearer'
        ]
        expect(bearer_schemes, f'the card declares the bearer it enforces: {card.security_schemes}')
        expect(
            any(set(r.schemes) & set(bearer_schemes) for r in card.security_requirements),
            'a security requirement names the bearer scheme',
        )
        expect(card.capabilities.push_notifications, 'push is on, and the card says so')

        auth = [AuthInterceptor(Bearer(card))]
        unary = ClientFactory(ClientConfig(streaming=False, httpx_client=http)).create(card, auth)
        streaming = ClientFactory(ClientConfig(streaming=True, httpx_client=http)).create(card, auth)

        # SendMessage, blocking: the finished task.
        blocking = SendMessageRequest(
            message=new_text_message('hello from the SDK', role=Role.ROLE_USER),
            configuration=SendMessageConfiguration(return_immediately=False),
        )
        replies = [r async for r in unary.send_message(blocking)]
        expect(len(replies) == 1 and replies[0].HasField('task'), f'a blocking send answers a Task: {replies}')
        done: Task = replies[0].task
        expect(done.status.state == TaskState.TASK_STATE_COMPLETED, f'the blocking send finished: {done.status}')

        # SendMessage, returnImmediately: the task at once, still moving.
        immediate = SendMessageRequest(
            message=new_text_message('SLOWPLEASE take your time', role=Role.ROLE_USER),
            configuration=SendMessageConfiguration(return_immediately=True),
        )
        replies = [r async for r in unary.send_message(immediate)]
        expect(len(replies) == 1 and replies[0].HasField('task'), f'returnImmediately answers a Task: {replies}')
        slow: Task = replies[0].task
        expect(slow.status.state not in TERMINAL, f'returnImmediately did not wait: {slow.status}')

        # GetTask and ListTasks read them back.
        got = await unary.get_task(GetTaskRequest(id=slow.id))
        expect(got.id == slow.id, f'GetTask: {got}')
        listed = await unary.list_tasks(ListTasksRequest())
        ids = {t.id for t in listed.tasks}
        expect({done.id, slow.id} <= ids, f'ListTasks lists both: {ids}')

        # A listing that matches nothing, read raw: ProtoJSON drops an empty
        # list, an empty string and a zero, and ParseDict would fill all three
        # back in without a word — so only the raw answer shows whether the
        # fields A2A marks REQUIRED are on the wire.
        empty = await http.post(
            url,
            headers={'A2A-Version': '1.0', 'Authorization': f'Bearer {BEARER}'},
            json={'jsonrpc': '2.0', 'id': 'empty', 'method': 'ListTasks', 'params': {'contextId': 'no-such-context'}},
        )
        result = empty.json().get('result')
        required = [
            f.json_name
            for f in ListTasksResponse.DESCRIPTOR.fields
            if field_behavior_pb2.REQUIRED in f.GetOptions().Extensions[field_behavior_pb2.field_behavior]
        ]
        expect(required, 'the proto marks some ListTasksResponse field REQUIRED')
        expect(
            isinstance(result, dict) and all(k in result for k in required),
            f'an empty listing carries every REQUIRED field ({required}): {empty.text}',
        )
        expect(result['tasks'] == [] and result['totalSize'] == 0, f'an empty listing: {result}')
        json_format.ParseDict(result, ListTasksResponse())

        # A webhook for the working task, with a credential to present.
        pushed = await unary.create_task_push_notification_config(
            TaskPushNotificationConfig(
                task_id=slow.id,
                url=receiver.url,
                token=PUSH_TOKEN,
                authentication=AuthenticationInfo(scheme='Bearer', credentials=PUSH_CREDENTIAL),
            )
        )
        expect(pushed.id and pushed.task_id == slow.id and pushed.url == receiver.url, f'the push config: {pushed}')

        # SubscribeToTask on the WORKING task, followed to its end.
        events = [e async for e in streaming.subscribe(SubscribeToTaskRequest(id=slow.id))]
        expect(events and events[0].HasField('task'), f'a subscription opens with the Task: {events[:1]}')
        states = [e.status_update.status.state for e in events if e.HasField('status_update')]
        final = events[-1].task.status.state if events[-1].HasField('task') else (states[-1] if states else None)
        expect(final == TaskState.TASK_STATE_COMPLETED, f'the stream follows the task to its end: {events}')
        finished = await unary.get_task(GetTaskRequest(id=slow.id))
        expect(
            any('late' in get_artifact_text(a) for a in finished.artifacts)
            or any('late' in json_format.MessageToJson(m) for m in finished.history),
            f'the late reply is on the task: {finished}',
        )

        # CancelTask on a finished task: TaskNotCancelableError, by its code.
        try:
            await unary.cancel_task(CancelTaskRequest(id=done.id))
        except TaskNotCancelableError:
            pass
        else:
            raise AssertionError('CancelTask on a finished task was not refused')

        # The extended card, as the authenticated caller sees it.
        extended = await unary.get_extended_agent_card(GetExtendedAgentCardRequest())
        expect(extended.name == card.name, f'the extended card: {extended.name}')

        # A command, activated the way A2A activates any extension.
        command = new_message(parts=[new_data_part({'agentd': {'op': 'status'}})], role=Role.ROLE_USER)
        command.extensions.append(COMMAND_EXTENSION)
        ctx = ClientCallContext(
            service_parameters=ServiceParametersFactory.create([with_a2a_extensions([COMMAND_EXTENSION])])
        )
        replies = [r async for r in unary.send_message(SendMessageRequest(message=command), context=ctx)]
        expect(len(replies) == 1 and replies[0].HasField('message'), f'a read op answers a Message: {replies}')
        answer: Message = replies[0].message
        expect(
            any(p.HasField('data') for p in answer.parts),
            f'the status document is a DataPart: {answer}',
        )

        # The webhook was told, with the credential it was given.
        deadline = time.monotonic() + 15
        while not receiver.deliveries and time.monotonic() < deadline:
            await asyncio.sleep(0.05)
        expect(receiver.deliveries, 'the webhook was never called')
        for d in receiver.deliveries:
            expect(d['headers'].get('authorization') == f'Bearer {PUSH_CREDENTIAL}', f'a delivery without the credential: {d["headers"]}')
            expect(d['headers'].get('x-a2a-notification-token') == PUSH_TOKEN, f'a delivery without the token: {d["headers"]}')
            json_format.Parse(d['body'], StreamResponse())
        receiver.server.shutdown()
    return sent


def main() -> int:
    binary = os.environ.get('AGENTD_E2E_BIN')
    if not binary:
        if os.environ.get('CI'):
            print('sdk_client.py: AGENTD_E2E_BIN is not set', file=sys.stderr)
            return 1
        print('sdk_client.py: skipped (set AGENTD_E2E_BIN to an agentd built with a2a,internal-mocks)')
        return 0
    with tempfile.TemporaryDirectory(prefix='agentd-sdk-') as workdir:
        daemon, url, _ = start_daemon(binary, workdir)
        try:
            sent = asyncio.run(run(url))
        except BaseException:
            print(daemon_log(workdir), file=sys.stderr)
            raise
        finally:
            daemon.terminate()
            try:
                daemon.wait(10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        # What went on the wire: every JSON-RPC call with the version and the
        # bearer, set by the SDK itself.
        calls = [r for r in sent if r.method == 'POST']
        methods = set()
        for r in calls:
            method = json.loads(r.content)['method']
            methods.add(method)
            expect(r.headers.get('a2a-version') == '1.0', f'{method} without A2A-Version 1.0: {dict(r.headers)}')
            expect(r.headers.get('authorization') == f'Bearer {BEARER}', f'{method} without the bearer')
        want = {
            'SendMessage',
            'GetTask',
            'ListTasks',
            'SubscribeToTask',
            'CancelTask',
            'GetExtendedAgentCard',
            'CreateTaskPushNotificationConfig',
        }
        expect(want <= methods, f'the run exercised every method: {sorted(methods)}')
        print(f'sdk_client.py: ok — {len(calls)} calls, {", ".join(sorted(methods))}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
