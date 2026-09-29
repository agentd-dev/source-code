# SPDX-License-Identifier: AGPL-3.0-only
"""The official A2A "Hello World" sample agent, as a peer for the interop tests.

Someone else's reading of the spec: this is the helloworld sample of
a2aproject/a2a-samples (samples/python/agents/helloworld, Apache-2.0) on the
official Python SDK, a2a-sdk 1.1.5 — the same card, executor and routes —
with only what a test needs added around it:

- the port comes from argv (0 picks a free one), and the card names the
  address the server really listens on;
- once listening it prints one line, `listening <url>`, so a test waits on
  the server rather than on a sleep;
- every HTTP request is written to stdout as one JSON line (method, path,
  the A2A service parameters and the body), before the SDK sees it, so a
  test can assert what agentd — or the TypeScript client — actually sent.

Nothing here changes how the SDK answers. The agent itself stays the
sample's: it acknowledges the request and completes the task.

    python hello_agent.py [port]
"""

import asyncio
import json
import socket
import sys

import uvicorn

from a2a.helpers import (
    get_message_text,
    new_task_from_user_message,
    new_text_message,
    new_text_part,
)
from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.routes import create_agent_card_routes, create_jsonrpc_routes
from a2a.server.tasks import InMemoryTaskStore, TaskUpdater
from a2a.types import (
    AgentCapabilities,
    AgentCard,
    AgentInterface,
    AgentSkill,
    TaskState,
)
from starlette.applications import Starlette


class HelloWorldAgentExecutor(AgentExecutor):
    """The sample's executor: WORKING, one text artifact, COMPLETED."""

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        if context.current_task:
            task = context.current_task
        else:
            task = new_task_from_user_message(context.message)
            await event_queue.enqueue_event(task)
        updater = TaskUpdater(event_queue=event_queue, task_id=task.id, context_id=task.context_id)
        await updater.update_status(
            state=TaskState.TASK_STATE_WORKING,
            message=new_text_message('Processing request...'),
        )
        query = get_message_text(context.message)
        result = (
            f'Hello, World! I have received your request ({query})'
            if query
            else 'No text input is provided!'
        )
        await updater.add_artifact(parts=[new_text_part(text=result, media_type='text/plain')])
        await updater.update_status(
            state=TaskState.TASK_STATE_COMPLETED,
            message=new_text_message('Request is completed!'),
        )

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        raise NotImplementedError('Cancel is not supported.')


def cards(url: str) -> tuple[AgentCard, AgentCard]:
    """The sample's public and extended cards, at `url`."""
    skill = AgentSkill(
        id='echo_bot',
        name='Echo Bot',
        description='An example agent that acknowledges client request and responds with a "Hello World" message.',
        input_modes=['text/plain'],
        output_modes=['text/plain'],
        tags=['a2a', 'echo-example'],
        examples=['hi', 'how are you'],
    )
    interfaces = [AgentInterface(protocol_binding='JSONRPC', url=url, protocol_version='1.0')]
    public = AgentCard(
        name='Hello World Agent',
        description='Just a hello world agent',
        version='0.0.1',
        default_input_modes=['text/plain'],
        default_output_modes=['text/plain'],
        capabilities=AgentCapabilities(streaming=True, extended_agent_card=True),
        supported_interfaces=interfaces,
        skills=[skill],
    )
    extended = AgentCard(
        name='Hello World Agent - Extended Edition',
        description='The full-featured hello world agent for authenticated users.',
        version='0.0.2',
        default_input_modes=['text/plain'],
        default_output_modes=['text/plain'],
        capabilities=AgentCapabilities(streaming=True, extended_agent_card=True),
        supported_interfaces=interfaces,
        skills=[
            skill,
            AgentSkill(
                id='echo_bot_super_mode',
                name='Echo Bot (Super Mode)',
                description='An extended version of Echo Bot that responds with extra enthusiasm!',
                tags=['a2a', 'echo-example', 'extended'],
                examples=['super hi', 'give me a super hello'],
            ),
        ],
    )
    return public, extended


class RequestLog:
    """ASGI middleware: one JSON line per request, then the request unchanged.

    The body is read whole and replayed to the app, so what is logged is
    exactly what the SDK then parses. After the replay, `receive` is the
    server's own again, so a streaming response still hears the client leave.
    """

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope['type'] != 'http':
            await self.app(scope, receive, send)
            return
        chunks = []
        while True:
            msg = await receive()
            chunks.append(msg.get('body', b''))
            if not msg.get('more_body', False):
                break
        body = b''.join(chunks)
        headers = {k.decode('latin-1').lower(): v.decode('latin-1') for k, v in scope['headers']}
        try:
            parsed = json.loads(body) if body else None
        except ValueError:
            parsed = body.decode('utf-8', 'replace')
        line = {
            'method': scope['method'],
            'path': scope['path'],
            'a2a-version': headers.get('a2a-version'),
            'a2a-extensions': headers.get('a2a-extensions'),
            'authorization': 'authorization' in headers,
            'body': parsed,
        }
        print(json.dumps(line), flush=True)
        replayed = False

        async def replay():
            nonlocal replayed
            if not replayed:
                replayed = True
                return {'type': 'http.request', 'body': body, 'more_body': False}
            return await receive()

        await self.app(scope, replay, send)


async def main(port: int) -> None:
    # Bind first, so the card can name the port a `0` picked.
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(('127.0.0.1', port))
    url = f'http://127.0.0.1:{sock.getsockname()[1]}'
    public, extended = cards(url)
    handler = DefaultRequestHandler(
        agent_executor=HelloWorldAgentExecutor(),
        task_store=InMemoryTaskStore(),
        agent_card=public,
        extended_agent_card=extended,
    )
    routes = [*create_agent_card_routes(public), *create_jsonrpc_routes(handler, '/')]
    app = RequestLog(Starlette(routes=routes))
    server = uvicorn.Server(uvicorn.Config(app, log_level='warning'))
    serving = asyncio.create_task(server.serve(sockets=[sock]))
    while not server.started:
        if serving.done():
            await serving
            return
        await asyncio.sleep(0.02)
    print(f'listening {url}', flush=True)
    await serving


if __name__ == '__main__':
    asyncio.run(main(int(sys.argv[1]) if len(sys.argv) > 1 else 0))
