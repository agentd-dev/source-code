// SPDX-License-Identifier: AGPL-3.0-only
// A stand-in for the flat's air conditioner (demo (e), home climate): the
// living room and the bedroom units, as MCP tools. Fictional.
//
//   node aircon.mjs [--port 8742]
//
// MCP (http://127.0.0.1:8742/mcp):
//   tools  state.read, power.set, mode.set, fan.set
// Control (POST http://127.0.0.1:8742/control/state, or control.mjs state aircon)
import { McpServer } from '@modelcontextprotocol/server';
import { z } from 'zod';
import { arg, logger, result, serve } from './lib.mjs';

const port = Number(arg('port', process.env.AIRCON_PORT || 8742));
const log = logger('aircon');
const units = new Map([
  ['living', { unit: 'living', power: 'off', mode: 'auto', fan: 'auto', setpoint: 24 }],
  ['bedroom', { unit: 'bedroom', power: 'off', mode: 'auto', fan: 'auto', setpoint: 24 }],
]);
const Unit = z.enum(['living', 'bedroom']);

function factory() {
  const mcp = new McpServer(
    { name: 'aircon-standin', version: '1.0.0', title: 'Air conditioner — the flat' },
    { capabilities: { tools: {} } },
  );
  mcp.registerTool('state.read', {
    description: 'Each unit: power, mode (auto, cool, dry, fan), fan speed and setpoint (°C). Give a unit for that one only.',
    inputSchema: z.object({ unit: Unit.optional() }),
    annotations: { readOnlyHint: true },
  }, async ({ unit }) => result({ units: unit ? [units.get(unit)] : [...units.values()] }));
  mcp.registerTool('power.set', {
    description: 'Turn a unit on or off; optionally with a setpoint in °C.',
    inputSchema: z.object({ unit: Unit, on: z.boolean(), setpoint: z.number().optional() }),
  }, async ({ unit, on, setpoint }) => {
    const u = units.get(unit);
    u.power = on ? 'on' : 'off';
    if (setpoint !== undefined) u.setpoint = setpoint;
    log('power.set', { unit, power: u.power, setpoint: u.setpoint });
    return result(u);
  });
  mcp.registerTool('mode.set', {
    description: 'Set a unit\'s mode: auto, cool, dry or fan.',
    inputSchema: z.object({ unit: Unit, mode: z.enum(['auto', 'cool', 'dry', 'fan']) }),
  }, async ({ unit, mode }) => {
    units.get(unit).mode = mode;
    log('mode.set', { unit, mode });
    return result(units.get(unit));
  });
  mcp.registerTool('fan.set', {
    description: 'Set a unit\'s fan speed: auto, low, medium or high.',
    inputSchema: z.object({ unit: Unit, speed: z.enum(['auto', 'low', 'medium', 'high']) }),
  }, async ({ unit, speed }) => {
    units.get(unit).fan = speed;
    log('fan.set', { unit, speed });
    return result(units.get(unit));
  });
  return mcp;
}

await serve({ name: 'aircon', port, factory, control: { state: async () => ({ units: [...units.values()] }) } });
