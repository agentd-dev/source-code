// SPDX-License-Identifier: AGPL-3.0-only
// A stand-in for the flat's thermostat (demo (e), home climate): every room's
// temperature, humidity and heating target as MCP tools, and the living room
// as a subscribable resource. The flat, its rooms and its people are
// fictional.
//
//   node thermostat.mjs [--port 8741]
//
// MCP (http://127.0.0.1:8741/mcp):
//   tools      rooms.read, target.set, schedule.read
//   resources  thermostat://rooms/<room> — living, bedroom, study; subscribable:
//              a reading or a target that changes tells every subscriber
// Control (POST http://127.0.0.1:8741/control/<name>, JSON; or control.mjs):
//   room   {"room": "living", "temp": 27, "occupants": ["nataliia"], "sun": true}
//   state  {}   every room
import { McpServer } from '@modelcontextprotocol/server';
import { z } from 'zod';
import { arg, logger, refusal, result, serve } from './lib.mjs';

const port = Number(arg('port', process.env.THERMOSTAT_PORT || 8741));
const log = logger('thermostat');
const rooms = new Map([
  ['living', { room: 'living', name: 'Living room', temp: 21.0, humidity: 44, target: 21.5, occupants: [], windowOpen: false, sunOnWindows: false }],
  ['bedroom', { room: 'bedroom', name: 'Bedroom', temp: 20.0, humidity: 47, target: 19.0, occupants: [], windowOpen: false, sunOnWindows: false }],
  ['study', { room: 'study', name: 'Study', temp: 21.2, humidity: 41, target: 21.5, occupants: [], windowOpen: false, sunOnWindows: false }],
]);
const uriOf = (room) => `thermostat://rooms/${room}`;
const schedule = { day: { from: '07:00', until: '23:00', target: 21.5 }, night: { from: '23:00', until: '07:00', bedroom: 19.0 }, away: 17.0 };
// The flat's own limits, whatever a client asks (the document's guardrail says the same).
const MIN = 16;
const MAX = 28;

let handler;
const changed = (room) => {
  handler.notify.resourceUpdated(uriOf(room));
  log('resources/updated', { uri: uriOf(room) });
};

function factory() {
  const mcp = new McpServer(
    { name: 'thermostat-standin', version: '1.0.0', title: 'Thermostat — the flat' },
    { capabilities: { tools: {}, resources: { subscribe: true } } },
  );
  for (const r of rooms.keys()) {
    mcp.registerResource(`room-${r}`, uriOf(r), { title: rooms.get(r).name, mimeType: 'application/json' },
      async () => {
        log('resources/read', { uri: uriOf(r) });
        return { contents: [{ uri: uriOf(r), mimeType: 'application/json', text: JSON.stringify(rooms.get(r)) }] };
      });
  }
  mcp.registerTool('rooms.read', {
    description: "Every room's temperature (°C), humidity (%), heating target (°C), who is in it, and whether a window is open or the sun is on it. Give a room for that one only.",
    inputSchema: z.object({ room: z.enum(['living', 'bedroom', 'study']).optional() }),
    annotations: { readOnlyHint: true },
  }, async ({ room }) => {
    log('rooms.read', { room: room ?? 'all' });
    return result({ rooms: room ? [rooms.get(room)] : [...rooms.values()] });
  });
  mcp.registerTool('target.set', {
    description: `Set one room's heating target in °C (${MIN}–${MAX}). One room at a time.`,
    inputSchema: z.object({ room: z.enum(['living', 'bedroom', 'study']), celsius: z.number() }),
    annotations: { idempotentHint: true },
  }, async ({ room, celsius }) => {
    if (celsius < MIN || celsius > MAX) {
      log('target.set refused', { room, celsius });
      return refusal(`${celsius} °C is outside ${MIN}–${MAX} °C`);
    }
    const r = rooms.get(room);
    const before = r.target;
    r.target = celsius;
    log('target.set', { room, celsius, before });
    changed(room);
    return result({ room, target: celsius, before });
  });
  mcp.registerTool('schedule.read', {
    description: 'The heating schedule: day, night and away targets and hours.',
    inputSchema: z.object({}),
    annotations: { readOnlyHint: true },
  }, async () => result(schedule));
  return mcp;
}

({ handler } = await serve({
  name: 'thermostat',
  port,
  factory,
  control: {
    room: async ({ room = 'living', temp, humidity, occupants, sun, window: open }) => {
      const r = rooms.get(room);
      if (!r) return { error: `no room ${room}` };
      if (temp !== undefined) r.temp = Number(temp);
      if (humidity !== undefined) r.humidity = Number(humidity);
      if (occupants !== undefined) r.occupants = occupants;
      if (sun !== undefined) r.sunOnWindows = Boolean(sun);
      if (open !== undefined) r.windowOpen = Boolean(open);
      log('room', { room, temp: r.temp, occupants: r.occupants.join(',') || '-', sun: r.sunOnWindows });
      changed(room);
      return r;
    },
    state: async () => ({ rooms: [...rooms.values()], schedule }),
  },
}));
